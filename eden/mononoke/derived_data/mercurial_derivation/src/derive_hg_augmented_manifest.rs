/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use blobstore::KeyedBlobstore;
use blobstore::Storable;
use blobstore::StoreLoadable;
use cloned::cloned;
use context::CoreContext;
use derived_data::prefetch_content_metadata;
use derived_data_manager::ManifestDerivation;
use either::Either;
use filestore::FetchKey;
use futures::future;
use futures::future::FutureExt;
use futures::stream;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use manifest::Entry;
use manifest::LeafInfo;
use manifest::ManifestComparison;
use manifest::ManifestOps;
use manifest::PathOrPrefix;
use manifest::Span;
use manifest::TreeInfo;
use manifest::TreeInfoSubentries;
use mercurial_types::HgAugmentedManifestEntry;
use mercurial_types::HgAugmentedManifestEnvelope;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::HgNodeHash;
use mercurial_types::HgParents;
use mercurial_types::ShardedHgAugmentedManifest;
use mercurial_types::blobs::ContentBlobMeta;
use mercurial_types::blobs::UploadHgFileContents;
use mercurial_types::blobs::UploadHgFileEntry;
use mercurial_types::blobs::UploadHgNodeHash;
use mercurial_types::calculate_hg_node_id_stream;
use mercurial_types::calculate_hg_node_ids_multi;
use mercurial_types::preloaded_augmented_manifest::serialize_manifest_entry;
use mercurial_types::sharded_augmented_manifest::HgAugmentedDirectoryNode;
use mercurial_types::sharded_augmented_manifest::HgAugmentedFileLeafNode;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::ContentMetadataV2;
use mononoke_types::FileChange;
use mononoke_types::FileType;
use mononoke_types::MPath;
use mononoke_types::MPathElement;
use mononoke_types::MPathElementPrefix;
use mononoke_types::NonRootMPath;
use mononoke_types::RepoPath;
use mononoke_types::TrackedFileChange;
use mononoke_types::TrieMap;
use mononoke_types::acl_manifest::AclManifest;
use mononoke_types::acl_manifest::AclManifestEntry;
use mononoke_types::hash::Blake3;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
use mononoke_types::sharded_map_v2::LookupKind;
use mononoke_types::sharded_map_v2::ShardedMapV2Node;
use mononoke_types::typed_hash::AclManifestId;
use restricted_paths_common::ArcRestrictedPathsConfigBased;
use restricted_paths_common::ManifestIdStoreWriteCallsite;
use restricted_paths_common::ManifestType;
use restricted_paths_common::RestrictedPathManifestIdEntry;
use restricted_paths_common::RestrictedPathsConfigBased;
use restricted_paths_common::maybe_propagate_manifest_id_store_write_error;
use tracing::error;

use crate::derive_hg_manifest::ParentIndex;
use crate::indexed_augmented_manifest::IndexedAugmentedDirectory;
use crate::indexed_augmented_manifest::IndexedAugmentedEntry;
use crate::indexed_augmented_manifest::IndexedAugmentedFile;
use crate::indexed_augmented_manifest::IndexedAugmentedTrieMap;
use crate::indexed_augmented_manifest::indexed_augmented_entry;

/// Derive an HgAugmentedManifestId from an HgManifestId and parents.
///
/// Canonical wrapper around [`derive_from_hg_manifest_and_parents_staged`]:
/// derives the whole tree starting at the root (always a tree) with no known
/// entries. With `stage_path == MPath::ROOT` and empty `known_entries`, the
/// staged core is byte-identical to the original hand-rolled traversal.
pub async fn derive_from_hg_manifest_and_parents(
    ctx: &CoreContext,
    blobstore: &(impl KeyedBlobstore + 'static),
    hg_manifest_id: HgManifestId,
    parents: Vec<HgAugmentedManifestId>,
    content_metadata_cache: &HashMap<ContentId, ContentMetadataV2>,
    restricted_paths: &RestrictedPathsConfigBased,
    acl_root_overlay: Option<AclManifestId>,
) -> Result<HgAugmentedManifestId> {
    let entry = derive_from_hg_manifest_and_parents_staged(
        ctx,
        blobstore,
        MPath::ROOT,
        Entry::Tree(hg_manifest_id),
        parents.into_iter().map(Some).collect(),
        HashMap::new(),
        content_metadata_cache,
        restricted_paths,
        acl_root_overlay,
    )
    .await?;
    match entry {
        Some(HgAugmentedManifestEntry::DirectoryNode(dir)) => {
            Ok(HgAugmentedManifestId::new(dir.treenode))
        }
        _ => bail!("root augmented manifest entry must be a directory"),
    }
}

/// Stage-scoped core of augmented manifest derivation.
///
/// Derives the augmented manifest subtree rooted at `stage_path`, returning the
/// entry at that path (a `DirectoryNode` for trees, a `FileNode` when the stage
/// root is a file, or `None` when nothing exists at the stage path).
///
/// `parents` are the parent augmented subtrees at `stage_path` (positional,
/// bonsai-parent order; `None` for parents with nothing there). `known_entries`
/// holds already-derived child-stage outputs keyed by absolute path; when the
/// traversal reaches one of those paths it reuses the entry instead of
/// recursing -- the same short-circuit the manifest crate's
/// `derive_manifest_with_known_entries` performs. This is byte-identical because
/// each known entry is content-addressed.
///
/// `hg_entry` is the stage-root entry from the hg manifest: a tree to traverse,
/// or a leaf when the stage path resolves to a file in this commit.
pub async fn derive_from_hg_manifest_and_parents_staged(
    ctx: &CoreContext,
    blobstore: &(impl KeyedBlobstore + 'static),
    stage_path: MPath,
    hg_entry: Entry<HgManifestId, (FileType, HgFileNodeId)>,
    parents: Vec<Option<HgAugmentedManifestId>>,
    known_entries: HashMap<MPath, Option<HgAugmentedManifestEntry>>,
    content_metadata_cache: &HashMap<ContentId, ContentMetadataV2>,
    restricted_paths: &RestrictedPathsConfigBased,
    acl_overlay: Option<AclManifestId>,
) -> Result<Option<HgAugmentedManifestEntry>> {
    // The stage root may itself be a file (a configured stage path that resolves
    // to a file in this commit). There is no tree to traverse, so build the
    // single augmented leaf and return it.
    let hg_manifest_id = match hg_entry {
        Entry::Leaf((file_type, filenode_id)) => {
            let leaf = build_augmented_file_leaf(
                ctx,
                blobstore,
                content_metadata_cache,
                file_type,
                filenode_id,
            )
            .await?;
            return Ok(Some(HgAugmentedManifestEntry::FileNode(leaf)));
        }
        Entry::Tree(hg_manifest_id) => hg_manifest_id,
    };

    let restricted_paths_enabled = justknobs::eval(
        "scm/mononoke:enabled_restricted_paths_access_logging",
        None, // hashing
        // Adding a switch value to be able to disable writes only
        Some("hg_augmented_manifest_write"),
    );
    let stage_path = &stage_path;
    // Already-derived child-stage subtrees, keyed by absolute path. When the
    // traversal reaches one of these paths it reuses the entry instead of
    // recursing, at any depth.
    let known_entries = &known_entries;

    let root = bounded_traversal::bounded_traversal(
        256,
        (stage_path.clone(), None, hg_manifest_id, parents, acl_overlay),
        |(parent_path, name, hg_manifest_id, parents, acl_overlay): (MPath, Option<MPathElement>, _, _, _)| {
            async move {
                // At the traversal root, `name` is None and this is the stage
                // path itself (`join_element(None)` returns the parent path).
                let path = parent_path.join_element(name.as_ref());
                // Short-circuit a subtree already derived in an earlier stage:
                // emit it directly without loading, diffing, or recursing. Only
                // directories are reached here -- files are handled as leaves by
                // the parent and rebuilt identically, and a `None` known entry is
                // never reached because that path is absent from the hg manifest.
                if let Some(Some(HgAugmentedManifestEntry::DirectoryNode(dir))) =
                    known_entries.get(&path)
                {
                    let children: Vec<(
                        MPath,
                        Option<MPathElement>,
                        HgManifestId,
                        Vec<Option<HgAugmentedManifestId>>,
                        Option<AclManifestId>,
                    )> = Vec::new();
                    return anyhow::Ok((Either::Left((name, dir.clone())), children));
                }
                let (hg_manifest, parents) = future::try_join(
                    hg_manifest_id.load(ctx, blobstore),
                    future::try_join_all(parents.iter().map(|p| async move {
                        match p {
                            Some(p) => {
                                let aug_parent = p.load(ctx, blobstore).await?;
                                let hg_parent =
                                    HgManifestId::new(aug_parent.augmented_manifest.hg_node_id)
                                        .load(ctx, blobstore)
                                        .await?;
                                Ok((Some(aug_parent), Some(hg_parent)))
                            }
                            None => Ok((None, None)),
                        }
                    })),
                )
                .await?;

                let (aug_parents, hg_parents): (Vec<Option<_>>, Vec<Option<_>>) =
                    parents.into_iter().unzip();

                // Build ACL child directory map from the current overlay
                let child_acl_map = match &acl_overlay {
                    Some(acl_id) => load_acl_child_directory_map(ctx, blobstore, acl_id).await?,
                    None => HashMap::new(),
                };

                let mut children = Vec::new();
                let mut new_subentries = Vec::new();

                // For each parent we store a list consisting of path elements to fetch entries for,
                // and path element prefixes to fetch partial maps for.
                let mut path_elems_to_fetch_from_aug_parents: Vec<
                    Vec<Either<MPathElement, MPathElementPrefix>>,
                > = vec![vec![]; aug_parents.len()];

                let mut diff =
                    manifest::compare_manifest(ctx, blobstore, hg_manifest.clone(), hg_parents)
                        .await?;

                while let Some(diff) = diff.try_next().await? {
                    match diff {
                        ManifestComparison::New(span) => {
                            let mut handle_new = |elem: MPathElement, entry| match entry {
                                Entry::Tree(id) => {
                                    let child_acl = child_acl_map.get(&elem).copied();
                                    children.push((
                                        path.clone(),
                                        Some(elem),
                                        id,
                                        Vec::new(),
                                        child_acl,
                                    ));
                                }
                                Entry::Leaf((file_type, filenode)) => {
                                    new_subentries.push((elem, file_type, filenode));
                                }
                            };
                            match span {
                                Span::Element(elem, entry) => handle_new(elem, entry),
                                Span::Prefix(prefix, entries) => {
                                    for (suffix, entry) in entries {
                                        let elem = prefix.clone().join_into_element(suffix)?;
                                        handle_new(elem, entry);
                                    }
                                }
                            }
                        }
                        ManifestComparison::Changed(elem, entry, _parent_entries) => {
                            match entry {
                            Entry::Tree(id) => {
                                let child_parents = {
                                    let elem = &elem;
                                    let child_parents_futs: Vec<_> =
                                        aug_parents
                                            .iter()
                                            .map(|p| async move {
                                                match p {
                                                    Some(p) => match p
                                                        .augmented_manifest
                                                        .subentries
                                                        .lookup(ctx, blobstore, elem.as_ref())
                                                        .await?
                                                    {
                                                        Some(
                                                            HgAugmentedManifestEntry::DirectoryNode(
                                                                dir,
                                                            ),
                                                        ) => anyhow::Ok(Some(
                                                            HgAugmentedManifestId::new(
                                                                dir.treenode,
                                                            ),
                                                        )),
                                                        None
                                                        | Some(
                                                            HgAugmentedManifestEntry::FileNode(_),
                                                        ) => Ok(None),
                                                    },
                                                    None => Ok(None),
                                                }
                                            })
                                            .collect();
                                    future::try_join_all(child_parents_futs).await?
                                };
                                let child_acl = child_acl_map.get(&elem).copied();
                                children.push((path.clone(), Some(elem), id, child_parents, child_acl));
                            }
                            Entry::Leaf((file_type, filenode)) => {
                                new_subentries.push((elem, file_type, filenode));
                            }
                            }
                        }
                        ManifestComparison::Same(span, index) => {
                            let key = match span {
                                Span::Element(elem, _entry) => Either::Left(elem),
                                Span::Prefix(prefix, _entries) => Either::Right(prefix),
                            };
                            path_elems_to_fetch_from_aug_parents
                                .get_mut(index)
                                .ok_or_else(|| {
                                    anyhow!(
                                        "Cannot find corresponding parent {index} of {hg_manifest_id} (ManifestComparison::Same)"
                                    )
                                })?
                                .push(key);
                        }
                        ManifestComparison::Removed(_) => {
                            // Removed items are omitted.
                        }
                    }
                }

                let reused_subentries_and_partial_maps =
                    path_elems_to_fetch_from_aug_parents.into_iter().enumerate().map(
                        |(index, path_elems_or_prefixes)| {
                            let aug_parents = &aug_parents;
                            async move {
                                if path_elems_or_prefixes.is_empty() {
                                    return anyhow::Ok(futures::stream::empty().left_stream());
                                }

                                let aug_parent = aug_parents
                                    .get(index)
                                    .and_then(Option::as_ref)
                                    .ok_or_else(|| {
                                        anyhow!(
                                            "Cannot find corresponding parent {index} of {hg_manifest_id}"
                                        )
                                    })?;

                                let lookup_keys = path_elems_or_prefixes.into_iter().map(|path_elem_or_prefix| {
                                    match path_elem_or_prefix {
                                        Either::Left(elem) => (elem.to_smallvec(), LookupKind::Entry),
                                        Either::Right(prefix) => (prefix.to_smallvec(), LookupKind::PartialMap),
                                    }
                                })
                                .collect();

                                Ok(
                                    stream::iter(
                                        aug_parent
                                            .augmented_manifest
                                            .subentries
                                            .clone()
                                            .get_entries_and_partial_maps(ctx, blobstore, lookup_keys, 100).await?
                                    )
                                    .map(anyhow::Ok)
                                    .right_stream()
                                )
                            }
                        },
                    )
                    .collect::<FuturesUnordered<_>>()
                    .try_flatten()
                    .try_collect::<Vec<_>>()
                    .await?;

                anyhow::Ok((
                    Either::Right((
                        path,
                        name,
                        hg_manifest,
                        new_subentries,
                        reused_subentries_and_partial_maps,
                        child_acl_map,
                        acl_overlay,
                    )),
                    children,
                ))
            }
            .boxed()
        },
        |out,
         children: bounded_traversal::Iter<(
            Option<MPathElement>,
            HgAugmentedManifestId,
            Blake3,
            u64,
        )>| {
            async move {
                // A short-circuited known subtree: emit it directly. The parent
                // applies its own child_acl_map to the directory pointer, so this
                // subtree's own acl is not needed here.
                let (path, name, hg_manifest, new_subentries, reused_subentries_and_partial_maps, child_acl_map, acl_overlay) =
                    match out {
                        Either::Left((name, dir)) => {
                            return Ok((
                                name,
                                HgAugmentedManifestId::new(dir.treenode),
                                dir.augmented_manifest_id,
                                dir.augmented_manifest_size,
                            ));
                        }
                        Either::Right(state) => state,
                    };
                let mut subentries = TrieMap::default();
                let new_subentries = stream::iter(new_subentries)
                    .map(|(elem, file_type, filenode_id)| {
                        anyhow::Ok(async move {
                            let leaf = build_augmented_file_leaf(
                                ctx,
                                blobstore,
                                content_metadata_cache,
                                file_type,
                                filenode_id,
                            )
                            .await?;
                            Ok((elem, HgAugmentedManifestEntry::FileNode(leaf)))
                        })
                    })
                    .try_buffered(100)
                    .try_collect::<Vec<_>>()
                    .await?;
                for (elem, entry) in new_subentries {
                    subentries.insert(elem, Either::Left(entry));
                }
                // Reused entries from parents are safe to use with their existing ACL
                // pointers: ACL pointer changes only occur when .slacl files are
                // added/removed, which always causes ManifestComparison::Changed on
                // the path to the .slacl file. Directories not on such a path either
                // (a) are not in the ACL tree (pointer is None) or (b) have an
                // unchanged ACL subtree (pointer remains valid).
                for (key, reused_item) in reused_subentries_and_partial_maps {
                    subentries.insert(key, reused_item);
                }
                for (name, treenode, augmented_manifest_id, augmented_manifest_size) in children {
                    let child_name = name.ok_or_else(|| {
                        anyhow!("child name must be defined for non-root traversal nodes")
                    })?;
                    let child_acl_id = child_acl_map.get(&child_name).copied();
                    subentries.insert(
                        child_name,
                        Either::Left(HgAugmentedManifestEntry::DirectoryNode(
                            HgAugmentedDirectoryNode {
                                treenode: treenode.into_nodehash(),
                                augmented_manifest_id,
                                augmented_manifest_size,
                                acl_manifest_directory_id: child_acl_id,
                            },
                        )),
                    );
                }
                let subentries =
                    ShardedMapV2Node::from_entries_and_partial_maps(ctx, blobstore, subentries)
                        .await?;

                let hg_node_id = hg_manifest.node_id();
                let augmented_manifest = ShardedHgAugmentedManifest {
                    hg_node_id,
                    p1: hg_manifest.p1(),
                    p2: hg_manifest.p2(),
                    computed_node_id: hg_manifest.computed_node_id(),
                    subentries,
                    acl_manifest_directory_id: acl_overlay,
                };

                // Enforce root invariant: the repo-root pointer must never be
                // Some(canonical_empty_acl_manifest_id). Only the true repo root
                // (stage root at MPath::ROOT) is subject to this; a non-root
                // stage root is an interior directory.
                if name.is_none() && stage_path.is_root() {
                    assert_root_acl_pointer_invariant(
                        &augmented_manifest.acl_manifest_directory_id,
                    )?;
                }

                let (augmented_manifest_id, augmented_manifest_size) = augmented_manifest
                    .clone()
                    .compute_content_addressed_digest(ctx, blobstore)
                    .await?;

                let envelope = HgAugmentedManifestEnvelope {
                    augmented_manifest_id,
                    augmented_manifest_size,
                    augmented_manifest,
                };
                let hg_augmented_manifest_id = envelope.store(ctx, blobstore).await?;

                if restricted_paths_enabled {
                    if let Some(non_root_path) = path.clone().into_optional_non_root_path() {
                        let should_record = restricted_paths
                            .should_record_manifest_id_entry(&non_root_path);
                        if should_record {
                            let entry = RestrictedPathManifestIdEntry::new(
                                ManifestType::HgAugmented,
                                hg_augmented_manifest_id.to_string().into(),
                                RepoPath::DirectoryPath(non_root_path),
                            )?;

                            if let Err(e) = restricted_paths
                                .manifest_id_store()
                                .add_entry(ctx, entry)
                                .await
                                .with_context(|| {
                                    format!("Failed to track restricted path at {path}")
                                })
                            {
                                error!(path = %path, error = %e, "Failed to track restricted path");
                                maybe_propagate_manifest_id_store_write_error(
                                    e,
                                    ManifestIdStoreWriteCallsite::DeriveFromHgManifestAndParentsStaged,
                                )?;
                            }
                        }
                    }
                }
                Ok((
                    name,
                    hg_augmented_manifest_id,
                    augmented_manifest_id,
                    augmented_manifest_size,
                ))
            }
            .boxed()
        },
    )
    .await?;
    let (_, hg_augmented_manifest_id, augmented_manifest_id, augmented_manifest_size) = root;
    Ok(Some(HgAugmentedManifestEntry::DirectoryNode(
        HgAugmentedDirectoryNode {
            treenode: hg_augmented_manifest_id.into_nodehash(),
            augmented_manifest_id,
            augmented_manifest_size,
            acl_manifest_directory_id: acl_overlay,
        },
    )))
}

/// Build the augmented file leaf for a single filenode, attaching content
/// blake3, sha1, and size from metadata (the prefetched cache, falling back to
/// the filestore).
async fn build_augmented_file_leaf(
    ctx: &CoreContext,
    blobstore: &impl KeyedBlobstore,
    content_metadata_cache: &HashMap<ContentId, ContentMetadataV2>,
    file_type: FileType,
    filenode_id: HgFileNodeId,
) -> Result<HgAugmentedFileLeafNode> {
    let filenode = filenode_id.load(ctx, blobstore).await?;
    let metadata = get_metadata(
        ctx,
        blobstore,
        content_metadata_cache,
        filenode.content_id(),
    )
    .await?;
    Ok(HgAugmentedFileLeafNode {
        file_type,
        filenode: filenode_id.into_nodehash(),
        total_size: metadata.total_size,
        content_blake3: metadata.seeded_blake3,
        content_sha1: metadata.sha1,
        file_header_metadata: if filenode.metadata().is_empty() {
            None
        } else {
            Some(filenode.metadata().clone())
        },
    })
}

async fn get_metadata<'a>(
    ctx: &CoreContext,
    blobstore: &impl KeyedBlobstore,
    content_metadata_cache: &'a HashMap<ContentId, ContentMetadataV2>,
    content_id: ContentId,
) -> Result<Cow<'a, ContentMetadataV2>> {
    Ok(match content_metadata_cache.get(&content_id) {
        Some(metadata) => Cow::Borrowed(metadata),
        None => {
            let metadata =
                filestore::get_metadata(blobstore, ctx, &FetchKey::Canonical(content_id))
                    .await?
                    .ok_or_else(|| anyhow!("Missing metadata for {content_id}"))?;
            Cow::Owned(metadata)
        }
    })
}

/// Normalize a derived ACL root into an optional overlay.
/// Returns `None` if the ACL manifest is the canonical empty one (no .slacl
/// files), `Some(id)` otherwise.
pub fn normalize_acl_root(
    root_acl_manifest_id: &acl_manifest::RootAclManifestId,
) -> Result<Option<AclManifestId>> {
    let id = *root_acl_manifest_id.inner_id();
    if id == AclManifest::empty_id() {
        Ok(None)
    } else {
        Ok(Some(id))
    }
}

async fn resolve_copy_paths_from_augmented_root<Store>(
    ctx: CoreContext,
    blobstore: Store,
    root: HgAugmentedManifestId,
    parent_csid: ChangesetId,
    paths: Vec<(NonRootMPath, MPath)>,
) -> Result<HashMap<(NonRootMPath, ChangesetId), HgFileNodeId>>
where
    Store: KeyedBlobstore + Clone + 'static,
{
    if paths.iter().all(|(_, lookup_path)| lookup_path.is_root()) {
        return Ok(HashMap::new());
    }

    let found: HashMap<MPath, HgFileNodeId> = root
        .find_entries(
            ctx,
            blobstore,
            paths
                .iter()
                .filter(|&(_, lookup_path)| !lookup_path.is_root())
                .map(|(_, lookup_path)| manifest::PathOrPrefix::Path(lookup_path.clone())),
        )
        .try_filter_map(|(path, entry)| async move {
            Ok(entry
                .into_leaf()
                .map(|file| (path, HgFileNodeId::new(file.filenode))))
        })
        .try_collect()
        .await?;
    Ok(paths
        .into_iter()
        .filter_map(|(copy_path, lookup_path)| {
            found
                .get(&lookup_path)
                .copied()
                .map(|filenode| ((copy_path, parent_csid), filenode))
        })
        .collect())
}

/// Pre-resolve copy-from source filenodes from parent augmented manifests.
///
/// Sources inside `stage_path` are resolved relative to parent stage outputs;
/// sources outside it are resolved from the corresponding parent root.
pub async fn resolve_copy_from_filenodes<Store>(
    ctx: &CoreContext,
    blobstore: &Store,
    stage_path: &MPath,
    file_changes: &[(NonRootMPath, Option<TrackedFileChange>)],
    parent_bonsai_csids: (Option<ChangesetId>, Option<ChangesetId>),
    parents: &[Option<HgAugmentedManifestEntry>],
    external_copy_parent_roots: &HashMap<ChangesetId, HgAugmentedManifestId>,
) -> Result<HashMap<(NonRootMPath, ChangesetId), HgFileNodeId>>
where
    Store: KeyedBlobstore + Clone + 'static,
{
    let parent_csids = [parent_bonsai_csids.0, parent_bonsai_csids.1];
    let mut stage_paths_by_parent: [Vec<(NonRootMPath, MPath)>; 2] = [Vec::new(), Vec::new()];
    let mut external_paths_by_parent: [Vec<NonRootMPath>; 2] = [Vec::new(), Vec::new()];

    for (_, change) in file_changes {
        let Some((copy_path, copy_csid)) = change.as_ref().and_then(TrackedFileChange::copy_from)
        else {
            continue;
        };
        let Some(parent_index) = parent_csids
            .iter()
            .position(|parent_csid| parent_csid.as_ref() == Some(copy_csid))
        else {
            continue;
        };
        if stage_path.is_prefix_of(copy_path) {
            stage_paths_by_parent[parent_index].push((
                copy_path.clone(),
                MPath::from(copy_path.clone()).remove_prefix_component(stage_path),
            ));
        } else {
            external_paths_by_parent[parent_index].push(copy_path.clone());
        }
    }

    stream::iter(
        stage_paths_by_parent
            .into_iter()
            .zip(external_paths_by_parent)
            .enumerate(),
    )
    .map(|(parent_index, (stage_paths, external_paths))| {
        let parent_csid = parent_csids[parent_index];
        let parent_entry = parents.get(parent_index).and_then(Option::as_ref).cloned();
        let external_root =
            parent_csid.and_then(|csid| external_copy_parent_roots.get(&csid).copied());
        let stage_ctx = ctx.clone();
        let stage_blobstore = blobstore.clone();
        let external_ctx = ctx.clone();
        let external_blobstore = blobstore.clone();
        async move {
            let Some(parent_csid) = parent_csid else {
                return Ok::<HashMap<_, _>, anyhow::Error>(HashMap::new());
            };
            let stage_sources = async move {
                match parent_entry {
                    Some(HgAugmentedManifestEntry::FileNode(file)) => Ok(stage_paths
                        .into_iter()
                        .filter(|(_, relative_path)| relative_path.is_root())
                        .map(|(copy_path, _)| {
                            ((copy_path, parent_csid), HgFileNodeId::new(file.filenode))
                        })
                        .collect()),
                    Some(HgAugmentedManifestEntry::DirectoryNode(dir)) => {
                        resolve_copy_paths_from_augmented_root(
                            stage_ctx,
                            stage_blobstore,
                            HgAugmentedManifestId::new(dir.treenode),
                            parent_csid,
                            stage_paths,
                        )
                        .await
                    }
                    None => Ok(HashMap::new()),
                }
            };
            let external_sources = async move {
                match external_root {
                    Some(root) => {
                        resolve_copy_paths_from_augmented_root(
                            external_ctx,
                            external_blobstore,
                            root,
                            parent_csid,
                            external_paths
                                .into_iter()
                                .map(|path| (path.clone(), path.into()))
                                .collect(),
                        )
                        .await
                    }
                    None => Ok(HashMap::new()),
                }
            };
            let (mut stage_sources, external_sources) =
                future::try_join(stage_sources, external_sources).await?;
            stage_sources.extend(external_sources);
            Ok(stage_sources)
        }
    })
    .buffer_unordered(2)
    .try_concat()
    .await
}

/// Decision for a leaves-only merge conflict.
#[derive(Debug)]
pub(crate) enum LeafConflictResolution {
    /// Reuse an hg-relevant parent's leaf (p1/p2).
    Reuse(HgAugmentedFileLeafNode),
    /// No hg-relevant parent has the file; caller must mint a parentless
    /// filenode using this representative content.
    MintFresh(HgAugmentedFileLeafNode),
}

/// Resolve a leaves-only conflict to match the HgManifest path.
///
/// All contributing parents must agree on file type and content. Only p1/p2 can
/// provide reusable Mercurial filenode parentage; step-parent-only files require
/// minting a parentless filenode.
pub(crate) fn check_content_identical_at_parents(
    path: &NonRootMPath,
    parents: &[IndexedAugmentedFile],
) -> Result<LeafConflictResolution> {
    use crate::derive_hg_manifest::unique_or_nothing;

    let Some(representative) = parents.first().map(|t| &t.node) else {
        bail!("Unresolved leaf conflict at {path}: no contributing parents");
    };

    unique_or_nothing(parents.iter().map(|t| t.node.file_type))
        .with_context(|| format!("Unresolved file type conflict at {path}"))?;

    // Match the legacy HgManifest resolver's content identity check.
    unique_or_nothing(
        parents
            .iter()
            .map(|t| (t.node.content_sha1, t.node.total_size)),
    )
    .with_context(|| format!("Unresolved content conflict at {path}"))?;

    // Step-parents affect conflict detection but cannot be filenode parents.
    let (maybe_reuse_leaf, _) = indexed_file_hg_parents(parents);
    match maybe_reuse_leaf {
        Some(leaf) => Ok(LeafConflictResolution::Reuse(leaf)),
        None => {
            // Content matches, but no reusable hg parent exists.
            Ok(LeafConflictResolution::MintFresh(representative.clone()))
        }
    }
}

/// Mint a fresh parentless Mercurial filenode for a file that appears only in
/// the step-parents (p3+) of an octopus merge, and build the augmented leaf for
/// it. Mirrors `resolve_conflict`'s no-reusable-filenode branch in
/// `derive_hg_manifest.rs`: a filenode with `p1 = p2 = None` and no copy
/// metadata is uploaded for the (already content-identical) data, so both the
/// direct and reference paths produce the same filenode and thus the same
/// augmented manifest.
async fn mint_parentless_leaf(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    path: &NonRootMPath,
    src: &HgAugmentedFileLeafNode,
) -> Result<HgAugmentedFileLeafNode> {
    // The augmented leaf carries content hashes but not a `ContentId`, so
    // recover it (and the canonical content size) from the source step-parent's
    // filenode envelope — the same envelope `resolve_conflict` loads to read
    // content identity.
    let envelope = HgFileNodeId::new(src.filenode)
        .load(ctx, blobstore)
        .await
        .with_context(|| format!("loading step-parent filenode envelope for {path}"))?;

    let contents = ContentBlobMeta {
        id: envelope.content_id(),
        size: envelope.content_size(),
        copy_from: None,
    };
    let (filenode_id, _) = UploadHgFileEntry {
        upload_node_id: UploadHgNodeHash::Generate,
        contents: UploadHgFileContents::ContentUploaded(contents),
        p1: None,
        p2: None,
    }
    .upload_with_path(ctx.clone(), blobstore.clone(), path.clone())
    .await?;

    build_augmented_file_leaf(ctx, blobstore, &HashMap::new(), src.file_type, filenode_id).await
}

/// Compute the Mercurial node id for an augmented-manifest directory by
/// streaming the entries through `calculate_hg_node_id_stream`.
///
/// This intentionally avoids materialising the full directory in memory: each
/// entry is serialised to its hg-manifest line and fed straight into the sha1
/// state. Peak memory is O(1) per directory regardless of how many entries
/// the directory has, which matters a lot for huge directories like
/// `fbcode/third-party` where the previous `try_collect`-based path could OOM
/// or stampede the blobstore.
pub async fn compute_hg_node_id(
    node: ShardedMapV2Node<HgAugmentedManifestEntry>,
    ctx: &CoreContext,
    blobstore: &impl KeyedBlobstore,
    parents: &HgParents,
) -> Result<HgNodeHash> {
    let lines = node
        .into_entries(ctx, blobstore)
        .and_then(|(path, entry)| async move {
            let path = MPathElement::from_smallvec(path)?;
            serialize_manifest_entry(&path, &entry)
        });
    calculate_hg_node_id_stream(lines, parents).await
}

/// Result of probing a merged directory for reusable parent envelopes.
#[derive(Debug, PartialEq, Eq)]
pub struct ParentEnvelopeReuse {
    pub reusable_parent: Option<HgAugmentedManifestId>,
    pub fresh_computed_node_id: HgNodeHash,
}

fn indexed_augmented_manifest_id(dir: &IndexedAugmentedDirectory) -> HgAugmentedManifestId {
    HgAugmentedManifestId::new(dir.node.treenode)
}

async fn indexed_augmented_directory_from_id<Store: KeyedBlobstore>(
    ctx: &CoreContext,
    blobstore: &Store,
    id: HgAugmentedManifestId,
    index: Option<ParentIndex>,
) -> Result<IndexedAugmentedDirectory> {
    let envelope = id.load(ctx, blobstore).await?;
    Ok(IndexedAugmentedDirectory {
        node: HgAugmentedDirectoryNode {
            treenode: id.into_nodehash(),
            augmented_manifest_id: envelope.augmented_manifest_id,
            augmented_manifest_size: envelope.augmented_manifest_size,
            acl_manifest_directory_id: envelope.augmented_manifest.acl_manifest_directory_id,
        },
        index,
    })
}

fn select_hg_parents<T>(
    parents: impl Iterator<Item = (Option<ParentIndex>, T)>,
) -> (Option<T>, Option<T>) {
    let mut parents = parents.filter_map(|(index, value)| match index {
        Some(ParentIndex(0)) | Some(ParentIndex(1)) => Some(value),
        Some(_) | None => None,
    });

    (parents.next(), parents.next())
}

fn indexed_directory_hg_parents(
    parents: &[IndexedAugmentedDirectory],
) -> (
    Option<&IndexedAugmentedDirectory>,
    Option<&IndexedAugmentedDirectory>,
) {
    select_hg_parents(parents.iter().map(|parent| (parent.index, parent)))
}

fn indexed_file_hg_parents(
    parents: &[IndexedAugmentedFile],
) -> (
    Option<HgAugmentedFileLeafNode>,
    Option<HgAugmentedFileLeafNode>,
) {
    select_hg_parents(
        parents
            .iter()
            .map(|parent| (parent.index, parent.node.clone())),
    )
}

fn expected_hg_copy_from(
    copy_from_filenodes: &HashMap<(NonRootMPath, ChangesetId), HgFileNodeId>,
    parent_bonsai_csids: (Option<ChangesetId>, Option<ChangesetId>),
    change: &TrackedFileChange,
) -> Option<(NonRootMPath, HgFileNodeId)> {
    let is_parent = |csid: &ChangesetId| {
        parent_bonsai_csids.0.as_ref() == Some(csid) || parent_bonsai_csids.1.as_ref() == Some(csid)
    };

    change
        .copy_from()
        .filter(|(_, copy_csid)| is_parent(copy_csid))
        .and_then(|(copy_path, copy_csid)| {
            copy_from_filenodes
                .get(&(copy_path.clone(), *copy_csid))
                .copied()
                .map(|filenode_id| (copy_path.clone(), filenode_id))
        })
}

/// Leaf callback for derive_manifest: creates an HgAugmentedFileLeafNode
/// for a new or changed file.
///
/// Parents carry `ParentIndex` in `IndexedAugmentedFile` so we can recover the
/// original bonsai parent index after `derive_manifest`'s value-only dedup. We
/// only feed p1/p2 filenodes to `store_file_change` because Mercurial
/// filenodes only encode (p1, p2) parentage; p3+ are ignored.
async fn create_augmented_leaf(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    content_metadata_cache: Arc<HashMap<ContentId, ContentMetadataV2>>,
    copy_from_filenodes: Arc<HashMap<(NonRootMPath, ChangesetId), HgFileNodeId>>,
    parent_bonsai_csids: (Option<ChangesetId>, Option<ChangesetId>),
    leaf_info: LeafInfo<IndexedAugmentedFile, TrackedFileChange>,
) -> Result<((), IndexedAugmentedFile)> {
    let change = match leaf_info.change {
        Some(change) => change,
        None => {
            // Leaves-only conflict: parents have the same file with identical
            // content but different filenodes. The resolver itself reads no
            // blobstore; only the step-parent-only case needs a write, to mint
            // a fresh parentless filenode (mirroring `resolve_conflict`).
            let leaf =
                match check_content_identical_at_parents(&leaf_info.path, &leaf_info.parents)? {
                    LeafConflictResolution::Reuse(leaf) => leaf,
                    LeafConflictResolution::MintFresh(src) => {
                        mint_parentless_leaf(&ctx, &blobstore, &leaf_info.path, &src).await?
                    }
                };
            return Ok((
                (),
                IndexedAugmentedFile {
                    node: leaf,
                    index: None,
                },
            ));
        }
    };

    // Pick the (p1, p2) parents using the index-aware filter, not positional
    // indexing: after `derive_manifest`'s value-only dedup the surviving entry
    // for an octopus merge can be the wrong parent positionally (e.g. (X, X, Y)
    // collapses to [p1:X, p3:Y], which is `(X, None)` for hg — not `(X, Y)`).
    let (p1_leaf, p2_leaf) = indexed_file_hg_parents(&leaf_info.parents);
    let p1 = p1_leaf.map(|p| HgFileNodeId::new(p.filenode));
    let p2 = p2_leaf.map(|p| HgFileNodeId::new(p.filenode));

    let copy_from =
        expected_hg_copy_from(copy_from_filenodes.as_ref(), parent_bonsai_csids, &change);

    // store_file_change is independent of get_metadata (the latter only
    // needs change.content_id()), so run them concurrently. store_file_change
    // also returns the file header metadata so we don't have to re-load the
    // freshly-stored filenode just to read it back.
    let store_fut = crate::derive_hg_changeset::store_file_change(
        ctx.clone(),
        blobstore.clone(),
        p1,
        p2,
        &leaf_info.path,
        &change,
        copy_from,
    );
    let metadata_fut = get_metadata(
        &ctx,
        &blobstore,
        &content_metadata_cache,
        change.content_id(),
    );
    let ((file_type, filenode_id, file_header_metadata), metadata) =
        future::try_join(store_fut, metadata_fut).await?;
    let is_metadata_present = !file_header_metadata.is_empty();

    let leaf = HgAugmentedFileLeafNode {
        file_type,
        filenode: filenode_id.into_nodehash(),
        content_blake3: metadata.seeded_blake3,
        content_sha1: metadata.sha1,
        total_size: metadata.total_size,
        file_header_metadata: is_metadata_present.then_some(file_header_metadata),
    };

    // Newly created leaf: no parent index.
    Ok((
        (),
        IndexedAugmentedFile {
            node: leaf,
            index: None,
        },
    ))
}

// ---------------------------------------------------------------------------
// Tree callback: types and helpers
// ---------------------------------------------------------------------------

/// Augmented subentries built up inside `create_augmented_tree`: either a
/// directly-placed entry (`Left`) or a reused parent subtree to be spliced in
/// by `ShardedMapV2Node::from_entries_and_partial_maps` (`Right`).
type AugmentedSubentriesMap =
    TrieMap<Either<HgAugmentedManifestEntry, LoadableShardedMapV2Node<HgAugmentedManifestEntry>>>;

/// Shape of `tree_info.subentries` for the augmented-manifest tree callback.
type AugmentedTreeCallbackSubentries = TreeInfoSubentries<
    IndexedAugmentedDirectory,
    IndexedAugmentedFile,
    (),
    IndexedAugmentedTrieMap,
>;

fn augmented_entry_from_indexed(entry: IndexedAugmentedEntry) -> HgAugmentedManifestEntry {
    match entry {
        Entry::Tree(dir) => HgAugmentedManifestEntry::DirectoryNode(dir.node),
        Entry::Leaf(file) => HgAugmentedManifestEntry::FileNode(file.node),
    }
}

pub(crate) fn validate_augmented_manifest_element(key: &[u8]) -> Result<()> {
    if key.contains(&b'\n') || key.contains(&b'\x01') {
        bail!(
            "Cannot derive Hg augmented manifest for a path element containing newline ('\\n') or the '\\x01' control code"
        );
    }
    Ok(())
}

fn collect_augmented_subentries(
    subentries: AugmentedTreeCallbackSubentries,
) -> Result<AugmentedSubentriesMap> {
    let mut direct = AugmentedSubentriesMap::default();
    for (key, value) in subentries {
        match value {
            Either::Left((_ctx, entry)) => {
                validate_augmented_manifest_element(&key)?;
                let elem = MPathElement::from_smallvec(key)?;
                direct.insert(elem, Either::Left(augmented_entry_from_indexed(entry)));
            }
            Either::Right(map) => {
                direct.insert(key, Either::Right(map.into_inner()));
            }
        }
    }
    Ok(direct)
}

/// Given the fully-merged subentries for one tree node, attempt parent reuse
/// (mirroring `create_hg_manifest`) or otherwise compute a fresh `hg_node_id`
/// and store the envelope. Records the envelope's id in the restricted-paths
/// manifest-id store when the JK is on and the path is a restriction root.
/// Returns the resulting rich directory node.
async fn finalize_envelope(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    tree_path: &MPath,
    parents: &[IndexedAugmentedDirectory],
    subentries: ShardedMapV2Node<HgAugmentedManifestEntry>,
    dir_acl_id: Option<AclManifestId>,
    restricted_paths: &ArcRestrictedPathsConfigBased,
    restricted_paths_enabled: bool,
) -> Result<((), IndexedAugmentedDirectory)> {
    let is_root = tree_path.is_root();
    if is_root {
        assert_root_acl_pointer_invariant(&dir_acl_id)?;
    }
    // Select p1/p2 by original Bonsai parent index, not by position after
    // `derive_manifest` deduplication. For octopus parent trees `(X, X, Y)`,
    // the deduped list can be `[p1:X, p3:Y]`; positional indexing would
    // incorrectly treat p3 as Mercurial p2.
    let (p1_dir, p2_dir) = indexed_directory_hg_parents(parents);
    let p1 = p1_dir.map(indexed_augmented_manifest_id);
    let p2 = p2_dir.map(indexed_augmented_manifest_id);
    let p1_hash = p1.map(HgAugmentedManifestId::into_nodehash);
    let p2_hash = p2.map(HgAugmentedManifestId::into_nodehash);

    let hash_parents = HgParents::new(p1_hash, p2_hash);

    // Mirror `create_hg_manifest` parent reuse: if merged contents match an
    // hg-relevant parent, reuse that parent's node. A reused root keeps a
    // client-supplied `hg_node_id` because Hg reuses that root id too.
    let computed_node_id = if parents.len() > 1 {
        let ParentEnvelopeReuse {
            reusable_parent,
            fresh_computed_node_id: computed_node_id,
        } = try_reuse_parent_envelope(ctx, blobstore, subentries.clone(), p1, p2, hash_parents)
            .await?;

        if let Some(reused_id) = reusable_parent {
            // Keep the parent's entry for this directory, ACL pointer included,
            // as `derive_from_hg_manifest_and_parents` copies it for a
            // directory that Hg reuses. The parent's stored envelope can carry
            // an older pointer than its entry. `finalize_augmented_manifest_root`
            // stores a reused root with the commit's pointer.
            let reused_dir = [p1_dir, p2_dir]
                .into_iter()
                .flatten()
                .find(|dir| indexed_augmented_manifest_id(dir) == reused_id)
                .context("reused directory should be p1 or p2")?;
            return Ok((
                (),
                IndexedAugmentedDirectory {
                    node: reused_dir.node.clone(),
                    index: None,
                },
            ));
        }

        computed_node_id
    } else {
        compute_hg_node_id(subentries.clone(), ctx, blobstore, &hash_parents).await?
    };

    let augmented_manifest = ShardedHgAugmentedManifest {
        hg_node_id: computed_node_id,
        p1: p1_hash,
        p2: p2_hash,
        computed_node_id,
        subentries,
        acl_manifest_directory_id: dir_acl_id,
    };

    let (augmented_manifest_id, augmented_manifest_size) = augmented_manifest
        .clone()
        .compute_content_addressed_digest(ctx, blobstore)
        .await?;

    let envelope = HgAugmentedManifestEnvelope {
        augmented_manifest_id,
        augmented_manifest_size,
        augmented_manifest,
    };

    let hg_augmented_manifest_id = envelope.store(ctx, blobstore).await?;

    if restricted_paths_enabled
        && let Some(non_root_path) = tree_path.clone().into_optional_non_root_path()
        && restricted_paths.should_record_manifest_id_entry(&non_root_path)
    {
        let entry = RestrictedPathManifestIdEntry::new(
            ManifestType::HgAugmented,
            hg_augmented_manifest_id.to_string().into(),
            RepoPath::DirectoryPath(non_root_path),
        )?;
        if let Err(e) = restricted_paths
            .manifest_id_store()
            .add_entry(ctx, entry)
            .await
            .with_context(|| format!("Failed to track restricted path at {tree_path}"))
        {
            error!(path = %tree_path, error = %e, "Failed to track restricted path");
            maybe_propagate_manifest_id_store_write_error(
                e,
                ManifestIdStoreWriteCallsite::FinalizeEnvelope,
            )?;
        }
    }

    Ok((
        (),
        IndexedAugmentedDirectory {
            node: HgAugmentedDirectoryNode {
                treenode: hg_augmented_manifest_id.into_nodehash(),
                augmented_manifest_id,
                augmented_manifest_size,
                acl_manifest_directory_id: dir_acl_id,
            },
            index: None,
        },
    ))
}

/// Tree callback for `derive_manifest`: build and store one augmented-manifest
/// directory.
///
/// `derive_manifest` may deduplicate equal parent entries. Parents carry
/// `ParentIndex` in `IndexedAugmentedDirectory` so we can still identify the
/// original Bonsai p1/p2 when computing Mercurial manifest parents; p3+ do not
/// participate because Hg manifest nodes record at most two parents.
async fn create_augmented_tree(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    restricted_paths: ArcRestrictedPathsConfigBased,
    restricted_paths_enabled: bool,
    acl_map: Arc<HashMap<MPath, AclManifestId>>,
    tree_info: TreeInfo<
        IndexedAugmentedDirectory,
        IndexedAugmentedFile,
        (),
        IndexedAugmentedTrieMap,
    >,
) -> Result<((), IndexedAugmentedDirectory)> {
    let dir_acl_id = acl_map.get(&tree_info.path).copied();
    let direct = collect_augmented_subentries(tree_info.subentries)?;
    let subentries =
        ShardedMapV2Node::from_entries_and_partial_maps(&ctx, &blobstore, direct).await?;

    finalize_envelope(
        &ctx,
        &blobstore,
        &tree_info.path,
        &tree_info.parents,
        subentries,
        dir_acl_id,
        &restricted_paths,
        restricted_paths_enabled,
    )
    .await
}

pub(crate) async fn finalize_augmented_manifest_root<Store>(
    ctx: &CoreContext,
    blobstore: &Store,
    root: Option<HgAugmentedManifestEntry>,
    parents: Vec<Option<HgAugmentedManifestEntry>>,
    restricted_paths: &ArcRestrictedPathsConfigBased,
    acl_root_overlay: Option<AclManifestId>,
) -> Result<HgAugmentedDirectoryNode>
where
    Store: KeyedBlobstore + Clone + 'static,
{
    let root_parents: Vec<_> = parents
        .into_iter()
        .enumerate()
        .map(|(index, parent)| {
            let node = match parent {
                Some(HgAugmentedManifestEntry::DirectoryNode(node)) => node,
                Some(HgAugmentedManifestEntry::FileNode(_)) => {
                    bail!("root augmented manifest parent {index} is a file")
                }
                None => bail!("root augmented manifest parent {index} is absent"),
            };
            Ok(IndexedAugmentedDirectory {
                node,
                index: Some(ParentIndex(index)),
            })
        })
        .collect::<Result<_>>()?;

    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(blobstore.clone());
    let restricted_paths = Arc::clone(restricted_paths);
    match root {
        Some(HgAugmentedManifestEntry::DirectoryNode(root_dir)) => {
            let root_id = HgAugmentedManifestId::new(root_dir.treenode);
            let mut root_envelope = root_id.load(ctx, &blobstore).await?;
            if root_envelope.augmented_manifest.acl_manifest_directory_id == acl_root_overlay {
                Ok(root_dir)
            } else {
                // The reused root was stored with another ACL pointer, for
                // example before ACL pointers existed. Keep its id, as Hg
                // does, and make the write `derive_from_hg_manifest_and_parents`
                // makes: the root under that id with the commit's pointer.
                // Neither the key nor the digest covers the pointer, so with
                // the default `IfAbsent` put behaviour the stored pointer stays.
                assert_root_acl_pointer_invariant(&acl_root_overlay)?;
                root_envelope.augmented_manifest.acl_manifest_directory_id = acl_root_overlay;
                root_envelope.store(ctx, &blobstore).await?;
                Ok(HgAugmentedDirectoryNode {
                    acl_manifest_directory_id: acl_root_overlay,
                    ..root_dir
                })
            }
        }
        Some(HgAugmentedManifestEntry::FileNode(_)) => {
            bail!("root augmented manifest entry must be a directory")
        }
        None => {
            let restricted_paths_enabled = justknobs::eval(
                "scm/mononoke:enabled_restricted_paths_access_logging",
                None,
                Some("hg_augmented_manifest_write"),
            );
            let acl_map = Arc::new(
                acl_root_overlay
                    .into_iter()
                    .map(|id| (MPath::ROOT, id))
                    .collect(),
            );
            let tree_info = TreeInfo {
                path: MPath::ROOT,
                parents: root_parents,
                subentries: Default::default(),
            };
            let (_, root_dir) = create_augmented_tree(
                ctx.clone(),
                Arc::clone(&blobstore),
                restricted_paths,
                restricted_paths_enabled,
                acl_map,
                tree_info,
            )
            .await?;
            Ok(root_dir.node)
        }
    }
}

/// Prepare the Bonsai-specific inputs and derive an augmented manifest directly,
/// bypassing HgManifest construction entirely.
pub async fn derive_augmented_manifest_from_bonsai_changeset<Store>(
    ctx: &CoreContext,
    blobstore: &Store,
    bonsai: &mononoke_types::BonsaiChangeset,
    parents: Vec<HgAugmentedManifestId>,
    restricted_paths: &ArcRestrictedPathsConfigBased,
    acl_root_overlay: Option<AclManifestId>,
) -> Result<HgAugmentedManifestId>
where
    Store: KeyedBlobstore + Clone + 'static,
{
    let mut content_ids = HashSet::<ContentId>::new();
    let file_changes = bonsai
        .file_changes()
        .map(|(path, file_change)| {
            Ok((
                path.clone(),
                match file_change {
                    FileChange::Change(change) => {
                        content_ids.insert(change.content_id());
                        Some(change.clone())
                    }
                    FileChange::Deletion => None,
                    FileChange::UntrackedChange(_) | FileChange::UntrackedDeletion => {
                        bail!("Can't derive manifest for snapshot")
                    }
                },
            ))
        })
        .collect::<Result<Vec<(NonRootMPath, Option<TrackedFileChange>)>>>()?;

    let parent_bonsai_csids = {
        let mut parents = bonsai.parents();
        (parents.next(), parents.next())
    };

    let content_metadata = prefetch_content_metadata(ctx, blobstore, content_ids).await?;

    derive_augmented_manifest_from_bonsai(
        ctx,
        blobstore,
        parents,
        file_changes,
        parent_bonsai_csids,
        &content_metadata,
        restricted_paths,
        acl_root_overlay,
    )
    .await
}

/// Derive the augmented manifest entry at `stage_path` directly from prepared
/// Bonsai inputs, bypassing HgManifest construction entirely.
///
/// `parents` are positional in Bonsai parent order and may be absent at the
/// stage path. `known_entries` contains child-stage results keyed by absolute
/// path; those entries are reused without traversing below them.
pub async fn derive_augmented_manifest_entry_from_bonsai<Store>(
    ctx: &CoreContext,
    blobstore: &Store,
    stage_path: MPath,
    parents: Vec<Option<HgAugmentedManifestEntry>>,
    known_entries: HashMap<MPath, Option<HgAugmentedManifestEntry>>,
    file_changes: Vec<(NonRootMPath, Option<TrackedFileChange>)>,
    parent_bonsai_csids: (Option<ChangesetId>, Option<ChangesetId>),
    external_copy_parent_roots: &HashMap<ChangesetId, HgAugmentedManifestId>,
    content_metadata_cache: &HashMap<ContentId, ContentMetadataV2>,
    restricted_paths: &ArcRestrictedPathsConfigBased,
    acl_root_overlay: Option<AclManifestId>,
) -> Result<Option<HgAugmentedManifestEntry>>
where
    Store: KeyedBlobstore + Clone + 'static,
{
    let restricted_paths_enabled = justknobs::eval(
        "scm/mononoke:enabled_restricted_paths_access_logging",
        None,
        Some("hg_augmented_manifest_write"),
    );

    let acl_root_path = stage_path.clone();
    let (acl_map, merge_acl_root_overlay) = match acl_root_overlay {
        None => (Arc::new(HashMap::new()), None),
        // A merge can rebuild a directory where its parents diverge even when no
        // file change touches that path: `derive_manifest_inner`'s bonsai
        // semantics recurse into and rebuild a tree when "all parents entries
        // are trees" with no change on the path. Such directories are not
        // reachable from `file_changes`, so each merge tree callback loads the
        // ACL pointer for the directory it actually rebuilds plus that
        // directory's immediate child pointers.
        Some(root_id) if parents.len() >= 2 => (Arc::new(HashMap::new()), Some(root_id)),
        // Single-parent and root commits rebuild exactly the directories on the
        // ancestor paths of `file_changes`. Scope the overlay load to that
        // frontier so the cost is O(commit size), not O(total ACL tree size).
        Some(root_id) => {
            let target_dirs = changed_ancestor_dirs(&file_changes);
            (
                Arc::new(
                    targeted_acl_overlay_map(ctx, blobstore, &acl_root_path, root_id, &target_dirs)
                        .await?,
                ),
                None,
            )
        }
    };

    let copy_from_filenodes = resolve_copy_from_filenodes(
        ctx,
        blobstore,
        &stage_path,
        &file_changes,
        parent_bonsai_csids,
        &parents,
        external_copy_parent_roots,
    )
    .await?;

    let parents: Vec<_> = parents
        .into_iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            entry.map(|entry| indexed_augmented_entry(entry, Some(ParentIndex(index))))
        })
        .collect();
    let known_entries = known_entries
        .into_iter()
        .map(|(path, entry)| {
            (
                path,
                entry.map(|entry| indexed_augmented_entry(entry, None)),
            )
        })
        .collect();
    let blobstore_arc: Arc<dyn KeyedBlobstore> = Arc::new(blobstore.clone());
    let content_metadata_arc = Arc::new(content_metadata_cache.clone());
    let copy_from_arc = Arc::new(copy_from_filenodes);
    let restricted_paths_arc = Arc::clone(restricted_paths);

    let input = ManifestDerivation {
        parents,
        known_entries,
        path: stage_path,
    };
    let entry = input
        .derive(
            ctx.clone(),
            blobstore.clone(),
            file_changes,
            {
                cloned!(
                    ctx,
                    blobstore_arc,
                    restricted_paths_arc,
                    acl_map,
                    merge_acl_root_overlay,
                    acl_root_path
                );
                move |tree_info| {
                    cloned!(
                        ctx,
                        blobstore_arc,
                        restricted_paths_arc,
                        acl_map,
                        merge_acl_root_overlay,
                        acl_root_path
                    );
                    async move {
                        let acl_map = match merge_acl_root_overlay {
                            Some(root_id) => Arc::new(
                                rebuilt_tree_acl_overlay_map(
                                    &ctx,
                                    &blobstore_arc,
                                    &acl_root_path,
                                    root_id,
                                    &tree_info.path,
                                )
                                .await?,
                            ),
                            None => acl_map,
                        };
                        create_augmented_tree(
                            ctx,
                            blobstore_arc,
                            restricted_paths_arc,
                            restricted_paths_enabled,
                            acl_map,
                            tree_info,
                        )
                        .await
                    }
                }
            },
            {
                cloned!(ctx, blobstore_arc, content_metadata_arc, copy_from_arc);
                move |leaf_info| {
                    cloned!(ctx, blobstore_arc, content_metadata_arc, copy_from_arc);
                    create_augmented_leaf(
                        ctx,
                        blobstore_arc,
                        content_metadata_arc,
                        copy_from_arc,
                        parent_bonsai_csids,
                        leaf_info,
                    )
                }
            },
        )
        .await?;

    Ok(entry.map(augmented_entry_from_indexed))
}

/// Derive an augmented manifest directly from prepared Bonsai inputs and parent
/// augmented manifests, bypassing HgManifest construction entirely.
pub async fn derive_augmented_manifest_from_bonsai<Store>(
    ctx: &CoreContext,
    blobstore: &Store,
    parents: Vec<HgAugmentedManifestId>,
    file_changes: Vec<(NonRootMPath, Option<TrackedFileChange>)>,
    parent_bonsai_csids: (Option<ChangesetId>, Option<ChangesetId>),
    content_metadata_cache: &HashMap<ContentId, ContentMetadataV2>,
    restricted_paths: &ArcRestrictedPathsConfigBased,
    acl_root_overlay: Option<AclManifestId>,
) -> Result<HgAugmentedManifestId>
where
    Store: KeyedBlobstore + Clone + 'static,
{
    let root_parents_indexed: Vec<_> = stream::iter(parents.into_iter().enumerate())
        .map(|(index, parent)| {
            indexed_augmented_directory_from_id(ctx, blobstore, parent, Some(ParentIndex(index)))
        })
        .buffered(100)
        .try_collect()
        .await?;
    let parent_entries: Vec<Option<HgAugmentedManifestEntry>> = root_parents_indexed
        .iter()
        .map(|parent| Some(HgAugmentedManifestEntry::DirectoryNode(parent.node.clone())))
        .collect();

    let root = derive_augmented_manifest_entry_from_bonsai(
        ctx,
        blobstore,
        MPath::ROOT,
        parent_entries.clone(),
        HashMap::new(),
        file_changes,
        parent_bonsai_csids,
        &HashMap::new(),
        content_metadata_cache,
        restricted_paths,
        acl_root_overlay,
    )
    .await?;
    let root = finalize_augmented_manifest_root(
        ctx,
        blobstore,
        root,
        parent_entries,
        restricted_paths,
        acl_root_overlay,
    )
    .await?;
    Ok(HgAugmentedManifestId::new(root.treenode))
}

/// Try to reuse a parent's augmented-manifest envelope if its subentries
/// match `merged_subentries`. Checks `p1` first, then `p2`; the first parent
/// whose content matches wins (Mercurial's first-parent-wins convention).
///
/// Mirrors `derive_hg_manifest.rs::create_hg_manifest`'s reuse logic. The
/// content-equality test is done by streaming `merged_subentries` under the
/// candidate parent's own `(p1, p2)`: if that hash equals the parent's stored
/// `computed_node_id`, then the merged content serialises identically to the
/// parent's content and the parent's node can be reused.
///
/// Like Hg, only content decides: the parent's stored ACL pointer is not
/// compared.
///
/// Computes the fresh-envelope node id in the same stream pass as the reuse
/// checks, so callers do not need a second sharded-map traversal when reuse is
/// rejected.
pub async fn try_reuse_parent_envelope(
    ctx: &CoreContext,
    blobstore: &impl KeyedBlobstore,
    merged_subentries: ShardedMapV2Node<HgAugmentedManifestEntry>,
    p1: Option<HgAugmentedManifestId>,
    p2: Option<HgAugmentedManifestId>,
    fresh_node_parents: HgParents,
) -> Result<ParentEnvelopeReuse> {
    let mut reuse_candidates = Vec::new();
    let mut hash_parent_sets = Vec::new();

    for parent_id in [p1, p2].into_iter().flatten() {
        let parent_env = parent_id.load(ctx, blobstore).await?;
        hash_parent_sets.push(HgParents::new(
            parent_env.augmented_manifest.p1,
            parent_env.augmented_manifest.p2,
        ));
        reuse_candidates.push((parent_id, parent_env.augmented_manifest.computed_node_id));
    }
    hash_parent_sets.push(fresh_node_parents);

    let lines =
        merged_subentries
            .into_entries(ctx, blobstore)
            .and_then(|(path, entry)| async move {
                let path = MPathElement::from_smallvec(path)?;
                serialize_manifest_entry(&path, &entry)
            });
    let mut node_ids_by_parent_set = calculate_hg_node_ids_multi(lines, &hash_parent_sets).await?;
    let fresh_computed_node_id = node_ids_by_parent_set
        .pop()
        .context("missing fresh hg node-id hash")?;

    let reusable_parent = reuse_candidates
        .into_iter()
        .zip(node_ids_by_parent_set)
        .find_map(
            |((parent_id, parent_computed_node_id), candidate_node_id)| {
                (candidate_node_id == parent_computed_node_id).then_some(parent_id)
            },
        );

    Ok(ParentEnvelopeReuse {
        reusable_parent,
        fresh_computed_node_id,
    })
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Load an ACL manifest node by ID and build a map of direct child directory
/// name -> child AclManifestId. Only directory entries (waypoints and restriction
/// roots) are included; leaf entries (.slacl files) are skipped.
async fn load_acl_child_directory_map(
    ctx: &CoreContext,
    blobstore: &(impl KeyedBlobstore + 'static),
    acl_id: &AclManifestId,
) -> Result<HashMap<MPathElement, AclManifestId>> {
    let acl_manifest: AclManifest = blobstore::Loadable::load(acl_id, ctx, blobstore).await?;
    acl_manifest
        .into_subentries(ctx, blobstore)
        .try_filter_map(|(name, entry)| async move {
            match entry {
                AclManifestEntry::Directory(dir) => Ok(Some((name, dir.id))),
                AclManifestEntry::AclFile(_) => Ok(None),
            }
        })
        .try_collect()
        .await
}

/// The set of directories `derive_manifest` rebuilds for a single-parent (or
/// root) commit: every ancestor directory of every changed file path, plus the
/// root. `create_augmented_tree` only ever looks up the ACL overlay for a
/// rebuilt directory and that directory's immediate children, so this set bounds
/// the part of the ACL tree that `targeted_acl_overlay_map` must cover.
///
/// NOT valid for merges: those can additionally rebuild directories where the
/// parents diverge, which are not reachable from `file_changes` (see the merge
/// fallback in `derive_augmented_manifest_from_bonsai`).
fn changed_ancestor_dirs(
    file_changes: &[(NonRootMPath, Option<TrackedFileChange>)],
) -> HashSet<MPath> {
    let mut dirs = HashSet::new();
    dirs.insert(MPath::ROOT);
    for (path, _change) in file_changes {
        // `into_ancestors` yields the path itself then each ancestor directory
        // up to the root; `skip(1)` drops the file leaf, leaving only its
        // ancestor directories.
        for dir in MPath::from(path.clone()).into_ancestors().skip(1) {
            dirs.insert(dir);
        }
    }
    dirs
}

async fn acl_manifest_id_at_path(
    ctx: &CoreContext,
    blobstore: &(impl KeyedBlobstore + 'static),
    root_acl_id: AclManifestId,
    path: &MPath,
) -> Result<Option<AclManifestId>> {
    let mut acl_id = root_acl_id;
    for component in path.iter() {
        let children = load_acl_child_directory_map(ctx, blobstore, &acl_id).await?;
        let Some(child_id) = children.get(component).copied() else {
            return Ok(None);
        };
        acl_id = child_id;
    }
    Ok(Some(acl_id))
}

/// Build the `MPath -> AclManifestId` overlay map for one rebuilt directory.
///
/// Merge derivation discovers rebuilt directories inside `derive_manifest`, so
/// it cannot compute a complete target frontier up front from `file_changes`.
/// The tree callback only needs the rebuilt directory's own ACL pointer plus
/// immediate child directory pointers that will be stamped on freshly-built
/// child entries.
async fn rebuilt_tree_acl_overlay_map(
    ctx: &CoreContext,
    blobstore: &(impl KeyedBlobstore + 'static),
    acl_root_path: &MPath,
    root_acl_id: AclManifestId,
    tree_path: &MPath,
) -> Result<HashMap<MPath, AclManifestId>> {
    if !acl_root_path.is_prefix_of(tree_path) {
        bail!("ACL root path {acl_root_path} is not a prefix of rebuilt tree path {tree_path}");
    }
    let relative_tree_path = tree_path.remove_prefix_component(acl_root_path);
    let Some(tree_acl_id) =
        acl_manifest_id_at_path(ctx, blobstore, root_acl_id, &relative_tree_path).await?
    else {
        return Ok(HashMap::new());
    };

    let children = load_acl_child_directory_map(ctx, blobstore, &tree_acl_id).await?;
    let mut entries = HashMap::with_capacity(children.len() + 1);
    entries.insert(tree_path.clone(), tree_acl_id);
    for (name, child_id) in children {
        entries.insert(tree_path.join_element(Some(&name)), child_id);
    }
    Ok(entries)
}

/// Build the `MPath -> AclManifestId` overlay map, scoped to the ACL directories
/// on the paths `derive_manifest` will rebuild (`target_dirs`). `acl_root_path`
/// is the absolute path represented by `root_acl_id`.
///
/// Unlike `pre_walk_acl_tree`, this descends only the branches that lead to a
/// rebuilt directory, so its cost is O(changed paths) rather than O(total ACL
/// tree size). For each visited (rebuilt) directory it records the directory's
/// own pointer and every immediate child's pointer — the directory itself is
/// what `create_augmented_tree` queries as `dir_acl_id`. Children outside
/// `target_dirs` are recorded but not descended into. Directories in
/// `target_dirs` that have no
/// ACL node (no `.slacl` in their subtree) simply never surface as children and
/// are correctly absent from the map (`acl_map.get` -> `None`).
///
/// Only correct when the rebuilt set is fully determined by `target_dirs` — i.e.
/// single-parent or root commits, NOT merges (see the merge fallback in
/// `derive_augmented_manifest_from_bonsai`).
pub async fn targeted_acl_overlay_map(
    ctx: &CoreContext,
    blobstore: &(impl KeyedBlobstore + 'static),
    acl_root_path: &MPath,
    root_acl_id: AclManifestId,
    target_dirs: &HashSet<MPath>,
) -> Result<HashMap<MPath, AclManifestId>> {
    let nested: Vec<Vec<(MPath, AclManifestId)>> = bounded_traversal::bounded_traversal_stream(
        100,
        std::iter::once((acl_root_path.clone(), root_acl_id)),
        move |(path, acl_id): (MPath, AclManifestId)| {
            async move {
                let children = load_acl_child_directory_map(ctx, blobstore, &acl_id).await?;
                let mut entries = Vec::with_capacity(children.len() + 1);
                // The rebuilt directory's own ACL pointer.
                entries.push((path.clone(), acl_id));
                let mut to_descend = Vec::new();
                for (name, child_id) in children {
                    let child_path = path.join(std::iter::once(&name));
                    // Record every immediate child: a rebuilt parent stamps an
                    // `acl_manifest_directory_id` for each of its children.
                    entries.push((child_path.clone(), child_id));
                    // Descend only into children that are themselves rebuilt.
                    if target_dirs.contains(&child_path) {
                        to_descend.push((child_path, child_id));
                    }
                }
                Ok::<_, anyhow::Error>((entries, to_descend))
            }
            .boxed()
        },
    )
    .try_collect::<Vec<_>>()
    .await?;
    Ok(nested.into_iter().flatten().collect())
}

/// Get-or-walk the *full* ACL overlay map for a changeset, memoised on
/// `AclManifestId`. Pass the same `cache` across a batch to dedupe walks for
/// changesets whose `RootAclManifestId` resolves to the same node. Unlike
/// [`targeted_acl_overlay_map`], this materialises every directory in the ACL
/// tree, so callers that genuinely need the complete `MPath -> AclManifestId`
/// map (e.g. the `verify-aug-direct` admin tool, which enumerates all ACL paths
/// to compare against) use this rather than the path-scoped descent that
/// `derive_augmented_manifest_from_bonsai` uses internally.
pub async fn cached_acl_overlay_map(
    ctx: &CoreContext,
    blobstore: &(impl KeyedBlobstore + 'static),
    acl_root: Option<AclManifestId>,
    cache: &mut HashMap<AclManifestId, Arc<HashMap<MPath, AclManifestId>>>,
) -> Result<Arc<HashMap<MPath, AclManifestId>>> {
    let Some(root_id) = acl_root else {
        return Ok(Arc::new(HashMap::new()));
    };
    if let Some(cached) = cache.get(&root_id) {
        return Ok(cached.clone());
    }
    let map = Arc::new(pre_walk_acl_tree(ctx, blobstore, root_id).await?);
    cache.insert(root_id, map.clone());
    Ok(map)
}

/// Walk the full ACL tree from the root and build a map of
/// `MPath -> AclManifestId` for every directory node. ACL nodes within the
/// walk are loaded concurrently via `bounded_traversal_stream`; sequential
/// load-per-node would otherwise make the walk's wall time scale with
/// `N * per-blob-latency`.
///
/// This loads the entire ACL tree regardless of commit size, so it is used only
/// by callers that need the complete map (via [`cached_acl_overlay_map`]).
/// Direct augmented-manifest derivation uses path-scoped overlay loading instead.
pub(crate) async fn pre_walk_acl_tree(
    ctx: &CoreContext,
    blobstore: &(impl KeyedBlobstore + 'static),
    root_acl_id: AclManifestId,
) -> Result<HashMap<MPath, AclManifestId>> {
    bounded_traversal::bounded_traversal_stream(
        100,
        std::iter::once((MPath::ROOT, root_acl_id)),
        move |(path, acl_id): (MPath, AclManifestId)| {
            async move {
                let children = load_acl_child_directory_map(ctx, blobstore, &acl_id).await?;
                let child_nodes: Vec<(MPath, AclManifestId)> = children
                    .into_iter()
                    .map(|(name, child_id)| (path.join(std::iter::once(&name)), child_id))
                    .collect();
                Ok::<_, anyhow::Error>(((path, acl_id), child_nodes))
            }
            .boxed()
        },
    )
    .try_collect::<HashMap<_, _>>()
    .await
}

/// Assert that the root ACL pointer invariant holds: the root pointer must
/// never be `Some(canonical_empty_acl_manifest_id)`. Valid states are `None`
/// (no restrictions) or `Some(non_empty_id)` (root is a waypoint or restriction root).
pub(crate) fn assert_root_acl_pointer_invariant(pointer: &Option<AclManifestId>) -> Result<()> {
    if let Some(id) = pointer {
        anyhow::ensure!(
            *id != AclManifest::empty_id(),
            "Root acl_manifest_directory_id must never be Some(canonical_empty_acl_manifest_id). \
             This indicates broken sparse-tree normalization."
        );
    }
    Ok(())
}

/// Records in the manifest id store the HgAugmentedManifest id of every
/// non-read-only restricted root in the commit's tree.
///
/// Incremental derivation records a restricted root only when it stores a new
/// envelope for it, so a commit pays only for the roots it changed. This is used
/// where the augmented manifest was not derived from its parents' and which roots
/// changed is unknown, for example backfilling with `derive_from_predecessor`, or
/// mapping an uploaded commit to the root tree upload already stored. It walks to
/// every restricted root instead; the store ignores entries it already has.
///
/// Each call walks to every non-read-only restricted root and does one
/// `INSERT IGNORE`, even when the commit changed nothing restricted. As of
/// 14 Oct 2026 every production restricted root is read-only, so this is free
/// today.
pub(crate) async fn track_all_restricted_paths(
    ctx: &CoreContext,
    restricted_paths: ArcRestrictedPathsConfigBased,
    hg_cs_id: HgChangesetId,
    root_hg_aug_mfid: HgAugmentedManifestId,
    blobstore: Arc<dyn KeyedBlobstore>,
) -> Result<()> {
    // Check if restricted paths tracking is enabled via justknobs
    let restricted_paths_enabled = justknobs::eval(
        "scm/mononoke:enabled_restricted_paths_access_logging",
        None, // hashing
        // Adding a switch value to be able to disable writes only
        Some("hg_augmented_manifest_write"),
    );

    if !restricted_paths_enabled {
        return Ok(());
    }

    // Get the configured restricted paths. Skip read-only roots: derivation must
    // not record new manifest-id-store entries for them.
    let restricted_dirs: Vec<PathOrPrefix> = restricted_paths
        .config()
        .path_restriction_metadata
        .iter()
        .filter(|(_, metadata)| !metadata.read_only)
        .map(|(non_root_mpath, _)| PathOrPrefix::Path(non_root_mpath.clone().into()))
        .collect();

    if restricted_dirs.is_empty() {
        return Ok(());
    }

    // Find all directory entries for the restricted paths and store their
    // HgAugmentedManifest IDs in the manifest id store
    let entries: Vec<RestrictedPathManifestIdEntry> = root_hg_aug_mfid
        .find_entries(ctx.clone(), blobstore, restricted_dirs)
        .try_filter_map(|(path, manif_entry)| async move {
            match manif_entry {
                Entry::Tree(hg_aug_manifest_id) => {
                    let repo_path = RepoPath::dir(path).context("Expected NonRootMPath")?;

                    let entry = RestrictedPathManifestIdEntry::new(
                        ManifestType::HgAugmented,
                        hg_aug_manifest_id.to_string().into(),
                        repo_path,
                    )?;
                    Ok(Some(entry))
                }
                Entry::Leaf(_) => bail!("Path {path} is not a directory"),
            }
        })
        .try_collect::<Vec<RestrictedPathManifestIdEntry>>()
        .await
        .context("Building restricted path manifest entries")?;

    if entries.is_empty() {
        return Ok(());
    }

    if let Err(e) = restricted_paths
        .manifest_id_store()
        .add_entries(ctx, &entries)
        .await
        .with_context(|| format!("Failed to track restricted paths for changeset {hg_cs_id}"))
    {
        error!(changeset = %hg_cs_id, error = %e, "Failed to track restricted paths");
        maybe_propagate_manifest_id_store_write_error(
            e,
            ManifestIdStoreWriteCallsite::TrackAllRestrictedPaths,
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;
    use mononoke_types::FileType;
    use mononoke_types::hash::Sha1 as ContentSha1;
    use mononoke_types::sha1_hash::Sha1 as NodeSha1;

    use super::*;

    fn leaf(
        file_type: FileType,
        filenode_byte: u8,
        content_byte: u8,
        size: u64,
    ) -> HgAugmentedFileLeafNode {
        HgAugmentedFileLeafNode {
            file_type,
            filenode: HgNodeHash::new(NodeSha1::from_byte_array([filenode_byte; 20])),
            content_blake3: Blake3::from_byte_array([content_byte; 32]),
            content_sha1: ContentSha1::from_byte_array([content_byte; 20]),
            total_size: size,
            file_header_metadata: None,
        }
    }

    fn indexed_leaf(index: usize, node: HgAugmentedFileLeafNode) -> IndexedAugmentedFile {
        IndexedAugmentedFile {
            node,
            index: Some(ParentIndex(index)),
        }
    }

    fn expect_reuse(resolution: LeafConflictResolution) -> Result<HgAugmentedFileLeafNode> {
        match resolution {
            LeafConflictResolution::Reuse(leaf) => Ok(leaf),
            other => Err(anyhow!("expected Reuse, got {other:?}")),
        }
    }

    fn expect_mint_fresh(resolution: LeafConflictResolution) -> Result<HgAugmentedFileLeafNode> {
        match resolution {
            LeafConflictResolution::MintFresh(leaf) => Ok(leaf),
            other => Err(anyhow!("expected MintFresh, got {other:?}")),
        }
    }

    /// `changed_ancestor_dirs` yields exactly the ancestor directories of every
    /// changed file (plus the root) and never the file leaf itself — this is the
    /// frontier the targeted ACL descent must cover for single-parent commits.
    #[mononoke::test]
    fn test_changed_ancestor_dirs() -> Result<()> {
        let dir = |s: &str| -> Result<MPath> { Ok(MPath::from(NonRootMPath::new(s)?)) };
        let change = |s: &str| -> Result<(NonRootMPath, Option<TrackedFileChange>)> {
            Ok((NonRootMPath::new(s)?, None))
        };

        // Nested file: every ancestor directory, root included, but not the leaf.
        let dirs = changed_ancestor_dirs(&[change("a/b/c.rs")?]);
        assert_eq!(
            dirs,
            [MPath::ROOT, dir("a")?, dir("a/b")?]
                .into_iter()
                .collect::<HashSet<_>>()
        );
        assert!(
            !dirs.contains(&dir("a/b/c.rs")?),
            "the changed file leaf must not be in the directory frontier"
        );

        // Root-level file: only the root is rebuilt.
        let dirs = changed_ancestor_dirs(&[change("README")?]);
        assert_eq!(dirs, [MPath::ROOT].into_iter().collect::<HashSet<_>>());

        // Multiple files (including a deletion): union of all ancestor dirs.
        let dirs =
            changed_ancestor_dirs(&[change("a/b/c.rs")?, change("a/x.rs")?, change("d/e.rs")?]);
        assert_eq!(
            dirs,
            [MPath::ROOT, dir("a")?, dir("a/b")?, dir("d")?]
                .into_iter()
                .collect::<HashSet<_>>()
        );

        Ok(())
    }

    #[mononoke::test]
    fn test_identical_content_returns_first_parent() -> Result<()> {
        // Given: p1 and p2 have the same content with different filenodes.
        let path = NonRootMPath::new("foo")?;
        let p1 = leaf(FileType::Regular, 0xAA, 0x42, 100);
        let p2 = leaf(FileType::Regular, 0xBB, 0x42, 100);
        let parents = vec![indexed_leaf(0, p1.clone()), indexed_leaf(1, p2)];

        // When: resolving the leaves-only conflict.
        let resolved = expect_reuse(check_content_identical_at_parents(&path, &parents)?)?;

        // Then: the first hg-relevant parent leaf is reused.
        assert_eq!(resolved.filenode, p1.filenode, "should reuse p1's filenode");
        assert_eq!(resolved.content_blake3, p1.content_blake3);
        Ok(())
    }

    #[mononoke::test]
    fn test_different_content_bails() -> Result<()> {
        // Given: p1 and p2 disagree on content.
        let path = NonRootMPath::new("foo")?;
        let parents = vec![
            indexed_leaf(0, leaf(FileType::Regular, 0xAA, 0x42, 100)),
            indexed_leaf(1, leaf(FileType::Regular, 0xBB, 0x99, 100)),
        ];

        // When: resolving the leaves-only conflict.
        let err = check_content_identical_at_parents(&path, &parents)
            .expect_err("different content must be unresolved");
        let msg = format!("{err:#}");

        // Then: the resolver rejects the content conflict.
        assert!(
            msg.contains("content conflict"),
            "expected content-conflict error, got: {msg}"
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_different_file_type_bails() -> Result<()> {
        // Given: parents have the same content but different file types.
        let path = NonRootMPath::new("foo")?;
        let parents = vec![
            indexed_leaf(0, leaf(FileType::Regular, 0xAA, 0x42, 100)),
            indexed_leaf(1, leaf(FileType::Executable, 0xBB, 0x42, 100)),
        ];

        // When: resolving the leaves-only conflict.
        let err = check_content_identical_at_parents(&path, &parents)
            .expect_err("different file_type must be unresolved");
        let msg = format!("{err:#}");

        // Then: the resolver rejects the observable manifest-entry conflict.
        assert!(
            msg.contains("file type conflict"),
            "expected file-type-conflict error, got: {msg}"
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_only_p2_present_returns_p2_leaf() -> Result<()> {
        // Given: only p2 contains the file.
        let path = NonRootMPath::new("foo")?;
        let p2 = leaf(FileType::Regular, 0xBB, 0x42, 100);
        let parents = vec![indexed_leaf(1, p2.clone())];

        // When: resolving the leaves-only conflict.
        let resolved = expect_reuse(check_content_identical_at_parents(&path, &parents)?)?;

        // Then: p2 is still an hg-relevant parent and can be reused.
        assert_eq!(resolved.filenode, p2.filenode);
        Ok(())
    }

    #[mononoke::test]
    fn test_only_step_parents_mint_fresh() -> Result<()> {
        // Given: only step-parents carry the path, and their file content matches.
        let path = NonRootMPath::new("foo")?;
        let sp3 = leaf(FileType::Regular, 0xCC, 0x42, 100);
        let sp4 = leaf(FileType::Regular, 0xDD, 0x42, 100);
        let parents = vec![indexed_leaf(2, sp3.clone()), indexed_leaf(3, sp4)];

        // When: resolving the leaves-only conflict.
        let src = expect_mint_fresh(check_content_identical_at_parents(&path, &parents)?)?;

        // Then: the caller is told to mint a parentless filenode using the
        // step-parent content as the source.
        assert_eq!(
            src.content_sha1, sp3.content_sha1,
            "minted leaf must carry the step-parent content"
        );
        assert_eq!(src.file_type, sp3.file_type);
        Ok(())
    }

    #[mononoke::test]
    fn test_step_parents_disagree_bails() -> Result<()> {
        // Given: only step-parents carry the path, but they disagree on content.
        let path = NonRootMPath::new("foo")?;
        let parents = vec![
            indexed_leaf(2, leaf(FileType::Regular, 0xCC, 0x42, 100)),
            indexed_leaf(3, leaf(FileType::Regular, 0xDD, 0x99, 100)),
        ];

        // When: resolving the leaves-only conflict.
        let err = check_content_identical_at_parents(&path, &parents)
            .expect_err("step-parents with differing content must be unresolved");
        let msg = format!("{err:#}");

        // Then: the resolver rejects the content conflict instead of minting.
        assert!(
            msg.contains("content conflict"),
            "expected content-conflict error, got: {msg}"
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_step_parent_disagreement_bails() -> Result<()> {
        // Given: p1 and p2 agree on content, but a step-parent disagrees.
        let path = NonRootMPath::new("foo")?;
        let parents = vec![
            indexed_leaf(0, leaf(FileType::Regular, 0xAA, 0x42, 100)),
            indexed_leaf(1, leaf(FileType::Regular, 0xBB, 0x42, 100)),
            indexed_leaf(2, leaf(FileType::Regular, 0xCC, 0x99, 200)),
        ];

        // When: resolving the leaves-only conflict.
        let err = check_content_identical_at_parents(&path, &parents)
            .expect_err("step-parent disagreement must be unresolved");
        let msg = format!("{err:#}");

        // Then: the resolver checks all contributing parents before reusing p1.
        assert!(
            msg.contains("content conflict"),
            "expected content-conflict error, got: {msg}"
        );
        Ok(())
    }
}
