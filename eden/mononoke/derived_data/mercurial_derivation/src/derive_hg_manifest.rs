/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Error;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::format_err;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use cloned::cloned;
use context::CoreContext;
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStageOutput;
use derived_data_manager::StageId;
use futures::future;
use futures::future::TryFutureExt;
use futures::future::try_join_all;
use manifest::Entry;
use manifest::LeafInfo;
use manifest::ManifestChanges;
use manifest::Traced;
use manifest::TreeInfo;
use manifest::derive_manifests_for_simple_stack_of_commits;
use manifest::flatten_subentries;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::blobs::ContentBlobMeta;
use mercurial_types::blobs::UploadHgFileContents;
use mercurial_types::blobs::UploadHgFileEntry;
use mercurial_types::blobs::UploadHgNodeHash;
use mercurial_types::blobs::UploadHgTreeEntry;
use mercurial_types::manifest::Type as HgManifestType;
use mononoke_types::ChangesetId;
use mononoke_types::FileType;
use mononoke_types::NonRootMPath;
use mononoke_types::RepoPath;
use mononoke_types::SortedVectorTrieMap;
use mononoke_types::TrackedFileChange;
use restricted_paths_common::ArcRestrictedPathsConfigBased;
use restricted_paths_common::ManifestIdStoreWriteCallsite;
use restricted_paths_common::ManifestType;
use restricted_paths_common::RestrictedPathManifestIdEntry;
use restricted_paths_common::maybe_propagate_manifest_id_store_write_error;
use sorted_vector_map::SortedVectorMap;
use tracing::error;

use crate::derive_hg_changeset::store_file_change;

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub struct ParentIndex(pub usize);

pub async fn derive_simple_hg_manifest_stack_without_copy_info(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    manifest_changes: Vec<ManifestChanges<TrackedFileChange>>,
    parent: Option<HgManifestId>,
    restricted_paths: ArcRestrictedPathsConfigBased,
) -> Result<HashMap<ChangesetId, HgManifestId>, Error> {
    let res = derive_manifests_for_simple_stack_of_commits(
        ctx.clone(),
        blobstore.clone(),
        parent.map(|p| Traced::assign(ParentIndex(0), p)),
        manifest_changes,
        {
            cloned!(blobstore, ctx);
            move |mut tree_info, _cs_id| {
                cloned!(blobstore, ctx, restricted_paths);
                async move {
                    tree_info.parents = tree_info
                        .parents
                        .into_iter()
                        .map(|p| Traced::assign(ParentIndex(0), p.into_untraced()))
                        .collect();
                    create_hg_manifest(ctx.clone(), blobstore.clone(), tree_info, restricted_paths).await
                }
            }
        },
        {
            cloned!(blobstore, ctx);
            move |leaf_info, _cs_id| {
                cloned!(blobstore, ctx);
                async move {
                    let LeafInfo {
                        change,
                        path,
                        parents,
                    } = leaf_info;

                    let parents: Vec<_> = parents
                        .into_iter()
                        .map(|p| Traced::assign(ParentIndex(0), p.into_untraced()))
                        .collect();
                    match change {
                        Some(change) => {
                            if change.copy_from().is_some() {
                                return Err(
                                    format_err!(
                                        "unsupported generation of stack of hg manifests: leaf {} has copy info {:?}",
                                        path,
                                        change.copy_from(),
                                    )
                                );
                            }
                            store_file_change(
                                ctx,
                                blobstore,
                                parents.first().map(|p| p.untraced().1),
                                None,
                                &path,
                                &change,
                                None, // copy_from should be empty
                            )
                            .map_ok(|(file_type, filenode_id, _metadata)| {
                                ((), Traced::generate((file_type, filenode_id)))
                            })
                            .await
                        }
                        None => {
                            let (file_type, filenode) =
                                resolve_conflict(ctx, blobstore, path, &parents).await?;
                            Ok(((), Traced::generate((file_type, filenode))))
                        }
                    }
                }
            }
        },
    )
    .await?;

    Ok(res
        .into_iter()
        .map(|(key, value)| (key, value.into_untraced()))
        .collect())
}

pub type HgManifestEntry =
    Entry<Traced<ParentIndex, HgManifestId>, Traced<ParentIndex, (FileType, HgFileNodeId)>>;
pub type HgManifestOutput = ManifestStageOutput<
    Traced<ParentIndex, HgManifestId>,
    Traced<ParentIndex, (FileType, HgFileNodeId)>,
    SortedVectorTrieMap<HgManifestEntry>,
>;
pub type HgManifestInput = ManifestDerivationInput<
    Traced<ParentIndex, HgManifestId>,
    Traced<ParentIndex, (FileType, HgFileNodeId)>,
    SortedVectorTrieMap<HgManifestEntry>,
>;

pub(crate) fn prepare_manifest_input(
    stage: &StageId,
    parents: Vec<HgManifestOutput>,
    dependencies: HashMap<StageId, HgManifestOutput>,
) -> Result<HgManifestInput, Error> {
    let parents = parents
        .into_iter()
        .enumerate()
        .map(|(index, output)| {
            let assign = move |entry: HgManifestEntry| match entry {
                Entry::Tree(tree) => {
                    Entry::Tree(Traced::assign(ParentIndex(index), tree.into_untraced()))
                }
                Entry::Leaf(leaf) => {
                    Entry::Leaf(Traced::assign(ParentIndex(index), leaf.into_untraced()))
                }
            };
            match output {
                ManifestStageOutput::Entry(entry) => ManifestStageOutput::Entry(entry.map(assign)),
                ManifestStageOutput::Subentries(map) => {
                    ManifestStageOutput::Subentries(map.map_preserving_encoding(move |map| {
                        SortedVectorTrieMap::new(
                            map.into_iter()
                                .map(|(name, entry)| (name, assign(entry)))
                                .collect(),
                        )
                    }))
                }
            }
        })
        .collect();
    ManifestDerivationInput::from_stage_outputs(stage, parents, dependencies)
}

/// This function is used as callback from `derive_manifest` to generate and store manifest
/// object from `TreeInfo`.
pub(crate) async fn create_hg_manifest(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    tree_info: TreeInfo<
        Traced<ParentIndex, HgManifestId>,
        Traced<ParentIndex, (FileType, HgFileNodeId)>,
        (),
        SortedVectorTrieMap<
            Entry<Traced<ParentIndex, HgManifestId>, Traced<ParentIndex, (FileType, HgFileNodeId)>>,
        >,
    >,
    restricted_paths: ArcRestrictedPathsConfigBased,
) -> Result<((), Traced<ParentIndex, HgManifestId>), Error> {
    let TreeInfo {
        subentries,
        path,
        parents,
    } = tree_info;

    // See if any of the parents have the same hg manifest. If yes, then we can just reuse it
    // without recreating manifest again.
    // But note that we reuse only if manifest has more than on parent, and there are a few reasons for
    // it:
    // 1) If a commit have a single parent then create_hg_manifest function shouldn't normally be called -
    //    it can only happen if a file hasn't changed, but nevertheless there's an entry for this file
    //    in the bonsai. This should happen rarely, and recreating manifest in these cases shouldn't be
    //    a problem.
    // 2) It adds an additional read of parent manifests, and it can potentially be expensive if manifests
    //    are large.
    //    We'd rather not do it if we don't need to, and it seems that we don't really need to (see point 1)

    let subentries: BTreeMap<_, _> = flatten_subentries(&ctx, &(), subentries).await?.collect();
    if parents.len() > 1 {
        let mut subentries_vec_map = BTreeMap::new();
        for (name, (_context, subentry)) in &subentries {
            let subentry = match subentry {
                Entry::Tree(manifest_id) => Entry::Tree(*manifest_id.untraced()),
                Entry::Leaf(leaf) => Entry::Leaf(*leaf.untraced()),
            };
            subentries_vec_map.insert(name.clone(), subentry);
        }

        let subentries_vec_map = SortedVectorMap::from(subentries_vec_map);

        let (p1_parent, p2_parent) = hg_parents(&parents);
        let loaded_parents = {
            let ctx = &ctx;
            let blobstore = &blobstore;

            future::try_join_all(p1_parent.into_iter().chain(p2_parent).map(|id| async move {
                let mf = id.load(ctx, blobstore).map_err(Error::from).await?;
                Result::<_, Error>::Ok((id, mf))
            }))
            .await?
        };

        if let Some((reuse_id, _)) = loaded_parents
            .into_iter()
            .find(|(_, p)| p.content().files == subentries_vec_map)
        {
            return Ok(((), Traced::generate(reuse_id)));
        }
    }

    let mut contents = Vec::new();
    for (name, (_context, subentry)) in subentries {
        if name.contains(b'\n') || name.contains(b'\x01') {
            bail!(
                "Cannot derive Hg Manifest for a path containing newline ('\\n') or the '\\x01' control code as such paths cannot be represented by Hg"
            );
        }
        contents.extend(name.as_ref());
        let subentry: Entry<_, _> = subentry.into();
        let (tag, hash) = match subentry {
            Entry::Tree(manifest_id) => (
                HgManifestType::Tree.manifest_suffix()?,
                manifest_id.into_nodehash(),
            ),
            Entry::Leaf((file_type, filenode_id)) => {
                let tag = HgManifestType::File(file_type).manifest_suffix()?;
                (tag, filenode_id.into_nodehash())
            }
        };
        contents.push(b'\0');
        contents.extend(hash.to_hex().as_bytes());
        contents.extend(tag);
        contents.push(b'\n')
    }

    let path = match path.into_optional_non_root_path() {
        None => RepoPath::RootPath,
        Some(path) => RepoPath::DirectoryPath(path),
    };

    let (p1, p2) = hg_parents(&parents);

    let p1 = p1.map(|id| id.into_nodehash());
    let p2 = p2.map(|id| id.into_nodehash());

    let uploader = UploadHgTreeEntry {
        upload_node_id: UploadHgNodeHash::Generate,
        contents: contents.into(),
        p1,
        p2,
        path: path.clone(),
        computed_node_id: None,
    }
    .upload(ctx.clone(), blobstore);

    let (mfid, upload_fut) = match uploader {
        Ok((mfid, fut)) => (mfid, fut.map_ok(|_| ())),
        Err(e) => return Err(e),
    };

    let restricted_paths_enabled = justknobs::eval(
        "scm/mononoke:enabled_restricted_paths_access_logging",
        None, // hashing
        // Adding a switch value to be able to disable writes only
        Some("hg_manifest_write"),
    );
    // Track restricted paths by storing manifest IDs for directories that match restricted path prefixes
    if restricted_paths_enabled {
        if let path @ RepoPath::DirectoryPath(non_root_path) = &path {
            let should_record = restricted_paths.should_record_manifest_id_entry(non_root_path);
            if should_record {
                let entry = RestrictedPathManifestIdEntry::new(
                    ManifestType::Hg,
                    mfid.to_string().into(),
                    path.clone(),
                )?;

                if let Err(e) = restricted_paths
                    .manifest_id_store()
                    .add_entry(&ctx, entry)
                    .await
                    .with_context(|| format!("Failed to track restricted path at {path}"))
                {
                    error!(path = %path, error = %e, "Failed to track restricted path");
                    maybe_propagate_manifest_id_store_write_error(
                        e,
                        ManifestIdStoreWriteCallsite::CreateHgManifest,
                    )?;
                }
            }
        }
    }

    upload_fut.await?;
    Ok(((), Traced::generate(mfid)))
}

/// This function is used as callback from `derive_manifest` to generate and store file entry
/// object from `LeafInfo`.
pub(crate) async fn create_hg_file(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    leaf_info: LeafInfo<Traced<ParentIndex, (FileType, HgFileNodeId)>, (FileType, HgFileNodeId)>,
) -> Result<((), Traced<ParentIndex, (FileType, HgFileNodeId)>), Error> {
    let LeafInfo {
        change,
        path,
        parents,
    } = leaf_info;

    // TODO: move `Blobrepo::store_file_changes` logic in here
    match change {
        Some(change) => Ok(((), Traced::generate(change))),
        None => {
            // Leaf was not provided, try to resolve same-content different parents leaf. Since filenode
            // hashes include ancestry, this can be necessary if two identical files were created through
            // different paths in history.
            let (file_type, filenode) = resolve_conflict(ctx, blobstore, path, &parents).await?;
            Ok(((), Traced::generate((file_type, filenode))))
        }
    }
}

async fn resolve_conflict(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    path: NonRootMPath,
    parents: &[Traced<ParentIndex, (FileType, HgFileNodeId)>],
) -> Result<(FileType, HgFileNodeId), Error> {
    let make_err = || {
        anyhow!(
            "Unresolved conflict at {} with parents: {:?}",
            path,
            parents.iter().map(|p| *p.untraced()).collect::<Vec<_>>()
        )
    };

    // First, if the file type is different across entries, we need to bail. This is a conflict.
    let file_type =
        unique_or_nothing(parents.iter().map(|p| p.untraced().0)).ok_or_else(make_err)?;

    // Assuming the file type is the same, then let's check that the contents are identical. To do
    // so, we'll load the envelopes.
    let envelopes = parents
        .iter()
        .map(|p| p.untraced().1.load(&ctx, &blobstore));

    let envelopes = try_join_all(envelopes).await?;

    let (content_id, content_size) =
        unique_or_nothing(envelopes.iter().map(|e| (e.content_id(), e.content_size())))
            .ok_or_else(make_err)?;

    // If we got here, then that means the file type and content is the same everywhere. In this
    // case, let's reuse a filenode.
    let (maybe_reuse_filenode, _) = hg_parents(parents);
    match maybe_reuse_filenode {
        Some((_ft, id)) => Ok((file_type, id)),
        None => {
            // This can only happen in the case of an octopus merge where neither p1 nor p2
            // contained this content. It would be nice if we could reuse p3 or later,
            // but Mercurial could be confused by a filenode whose linknode is not a Mercurial
            // ancestor of the commit. So don't risk it.
            let contents = ContentBlobMeta {
                id: content_id,
                size: content_size,
                copy_from: None,
            };
            let (filenode_id, _) = UploadHgFileEntry {
                upload_node_id: UploadHgNodeHash::Generate,
                contents: UploadHgFileContents::ContentUploaded(contents),
                p1: None,
                p2: None,
            }
            .upload_with_path(ctx, blobstore, path)
            .await?;
            Ok((file_type, filenode_id))
        }
    }
}

/// Extract hg-relevant parents from a set of Traced entries. This means we ignore any parents
/// except for p1 and p2.
///
/// The bound is `Clone` rather than `Copy` so callers can instantiate `T` with non-`Copy`
/// leaf types. `Copy` callers pay nothing extra: `<T as Clone>::clone` for a `Copy` type
/// lowers to a memcpy.
pub(crate) fn hg_parents<T: Clone>(parents: &[Traced<ParentIndex, T>]) -> (Option<T>, Option<T>) {
    let mut parents = parents.iter().filter_map(|t| match t.id() {
        Some(ParentIndex(0)) | Some(ParentIndex(1)) => Some(t.untraced()),
        Some(_) | None => None,
    });

    (parents.next().cloned(), parents.next().cloned())
}

/// Take an iterator, if it has just one value, return it. Otherwise, return None.
pub(crate) fn unique_or_nothing<T: PartialEq>(iter: impl Iterator<Item = T>) -> Option<T> {
    let mut ret = None;

    for e in iter {
        if ret.is_none() {
            ret = Some(e);
            continue;
        }

        if ret.as_ref().expect("We just checked") == &e {
            continue;
        }

        return None;
    }

    ret
}
