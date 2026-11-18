/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use basename_suffix_skeleton_manifest_v3::RootBssmV3DirectoryId;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::Storable;
use cloned::cloned;
use content_manifest_derivation::RootContentManifestId;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStageOutput;
use filestore::fetch_concat;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use itertools::Either;
use itertools::EitherOrBoth;
use manifest::Entry;
use manifest::LeafInfo;
use manifest::ManifestOps;
use manifest::PathOrPrefix;
use manifest::TreeInfo;
use manifest::TreeInfoSubentries;
use manifest::derive_manifest;
use metaconfig_types::RestrictedPathsAclFile;
use mononoke_types::BlobstoreValue;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ContentId;
use mononoke_types::FileContents;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use mononoke_types::TrieMap;
use mononoke_types::acl_manifest::AclManifest;
use mononoke_types::acl_manifest::AclManifestDirectoryEntry;
use mononoke_types::acl_manifest::AclManifestDirectoryRestriction;
use mononoke_types::acl_manifest::AclManifestEntry;
use mononoke_types::acl_manifest::AclManifestEntryBlob;
use mononoke_types::acl_manifest::AclManifestRestriction;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
use mononoke_types::sharded_map_v2::ShardedMapV2Node;
use mononoke_types::typed_hash::AclManifestEntryBlobId;
use mononoke_types::typed_hash::AclManifestId;
use restricted_paths_acl_file::parse_acl_file;
use scuba_ext::MononokeScubaSampleBuilder;
use vec1::vec1;

use crate::RootAclManifestId;

/// Explicit ACL file change type.
#[derive(Clone, Debug)]
pub(crate) enum AclFileChange {
    AddOrModify(ContentId),
    Delete,
}

/// Context carried by `derive_manifest` from child `create_tree`/`create_leaf`
/// to the parent's `create_tree`, providing pre-computed flags for
/// `AclManifestDirectoryEntry` without requiring a blobstore load.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AclManifestNodeInfo {
    pub(crate) is_restricted: bool,
    pub(crate) has_restricted_descendants: bool,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Derive an AclManifest using the `derive_manifest` primitive.
///
/// Handles all cases: root commits, single-parent, and merges.
/// Only `.slacl` file changes (explicit + implicit deletes) are fed to
/// `derive_manifest`; unchanged subtrees are reused automatically.
pub(crate) async fn derive_single(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: BonsaiChangeset,
    parents: Vec<RootAclManifestId>,
) -> Result<RootAclManifestId> {
    let blobstore = derivation_ctx.blobstore();
    let acl_file_name = derivation_ctx
        .restricted_paths()
        .config()
        .acl_file_name()
        .to_string();

    let parent_ids: Vec<AclManifestId> = parents.iter().map(|p| p.0).collect();

    // Collect .slacl file changes (explicit + implicit deletes).
    let changes = get_acl_file_changes(
        ctx,
        blobstore,
        &bonsai,
        parent_ids
            .iter()
            .copied()
            .map(|id| ManifestStageOutput::Entry(Some(Entry::Tree(id))))
            .collect(),
        &acl_file_name,
        &MPath::ROOT,
    )
    .await?;

    let derive_changes = prepare_acl_file_changes(ctx, blobstore, changes, &acl_file_name).await?;

    let result = inner_derive(ctx, blobstore, parent_ids, derive_changes, &acl_file_name).await?;

    match result {
        Some(id) => Ok(RootAclManifestId(id)),
        None => empty_root_acl_manifest_id(ctx, blobstore).await,
    }
}

/// Derive an AclManifest from scratch using BSSMV3 to find all `.slacl` files.
///
/// Used for backfilling via `DerivableUntopologically`. Finds all ACL files
/// in the repo and feeds them as synthetic additions to `derive_manifest`
/// with empty parents.
pub(crate) async fn derive_from_scratch(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: BonsaiChangeset,
) -> Result<RootAclManifestId> {
    let blobstore = derivation_ctx.blobstore();
    let cs_id = bonsai.get_changeset_id();

    let acl_file_name = derivation_ctx
        .restricted_paths()
        .config()
        .acl_file_name()
        .to_string();

    // Derive dependencies (fetch both in parallel -- independent blobstore lookups).
    let (bssm_v3, root_content_manifest_id) = futures::try_join!(
        derivation_ctx.fetch_dependency::<RootBssmV3DirectoryId>(ctx, cs_id),
        derivation_ctx.fetch_dependency::<RootContentManifestId>(ctx, cs_id),
    )?;

    // Find all ACL files via BSSMV3 and resolve their content IDs via ContentManifests.
    let acl_paths: Vec<NonRootMPath> = bssm_v3
        .find_files_filter_basenames(
            ctx,
            blobstore.clone(),
            vec![MPath::ROOT],
            EitherOrBoth::Left(vec1![acl_file_name.clone()]),
            None,
        )
        .await?
        .try_filter_map(|path| async move { Ok(NonRootMPath::try_from(path).ok()) })
        .try_collect()
        .await?;

    // Resolve content IDs for each .slacl file via ContentManifests and parse them
    // before calling derive_manifest. ContentManifests/blobstore failures remain hard
    // failures; parse failures are logged and omitted from from-scratch output.
    let derive_changes: Vec<(NonRootMPath, Option<RestrictedPathsAclFile>)> =
        stream::iter(acl_paths)
            .map(|path| {
                cloned!(ctx, blobstore, root_content_manifest_id);
                let acl_file_name = acl_file_name.clone();
                async move {
                    let content_id = resolve_content_id(
                        &ctx,
                        &blobstore,
                        &root_content_manifest_id,
                        &MPath::from(path.clone()),
                    )
                    .await?;
                    let acl_file = fetch_and_parse_acl_file(
                        &ctx,
                        &blobstore,
                        content_id,
                        &path,
                        &acl_file_name,
                    )
                    .await?;
                    Ok::<_, anyhow::Error>(acl_file.map(|acl_file| (path, Some(acl_file))))
                }
            })
            .buffer_unordered(100)
            .try_filter_map(|change| async move { Ok(change) })
            .try_collect()
            .await?;

    // Call derive_manifest with empty parents.
    let result = inner_derive(ctx, blobstore, vec![], derive_changes, &acl_file_name).await?;

    match result {
        Some(id) => Ok(RootAclManifestId(id)),
        None => empty_root_acl_manifest_id(ctx, blobstore).await,
    }
}

// ---------------------------------------------------------------------------
// Core derivation via derive_manifest
// ---------------------------------------------------------------------------

type AclLeaf = AclManifestRestriction;
type AclLeafChange = RestrictedPathsAclFile;
type AclTreeId = AclManifestId;
type AclCtx = AclManifestNodeInfo;

/// The `create_tree` callback for `derive_manifest`: build an `AclManifest`
/// node from the subentries produced by the traversal.
fn acl_create_tree_callback(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    acl_file_bytes: Vec<u8>,
) -> impl Fn(
    TreeInfo<AclTreeId, AclLeaf, AclCtx, LoadableShardedMapV2Node<AclManifestEntry>>,
) -> futures::future::BoxFuture<'static, Result<(AclCtx, AclTreeId)>>
+ Clone {
    move |info| {
        cloned!(ctx, blobstore, acl_file_bytes);
        Box::pin(async move {
            create_acl_manifest(ctx, blobstore, &acl_file_bytes, info.subentries).await
        })
    }
}

/// The `create_leaf` callback for `derive_manifest`: store the parsed `.slacl`
/// file as an entry blob and return its restriction.
fn acl_create_leaf_callback(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
) -> impl Fn(
    LeafInfo<AclLeaf, AclLeafChange>,
) -> futures::future::BoxFuture<'static, Result<(AclCtx, AclLeaf)>>
+ Clone {
    move |leaf_info| {
        cloned!(ctx, blobstore);
        Box::pin(async move {
            let acl_file = match leaf_info.change {
                Some(acl_file) => acl_file,
                None => {
                    anyhow::bail!("ACL file leaf with no change and no parents")
                }
            };
            let entry_blob_id = store_acl_entry_from_acl_file(&ctx, &blobstore, &acl_file).await?;
            let restriction = AclManifestRestriction { entry_blob_id };
            Ok((AclManifestNodeInfo::default(), restriction))
        })
    }
}

async fn inner_derive(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    parents: Vec<AclManifestId>,
    changes: Vec<(NonRootMPath, Option<RestrictedPathsAclFile>)>,
    acl_file_name: &str,
) -> Result<Option<AclManifestId>> {
    let acl_file_bytes: Vec<u8> = acl_file_name.as_bytes().to_vec();

    derive_manifest(
        ctx.clone(),
        blobstore.clone(),
        parents,
        changes,
        acl_create_tree_callback(ctx.clone(), blobstore.clone(), acl_file_bytes),
        acl_create_leaf_callback(ctx.clone(), blobstore.clone()),
    )
    .await
}

pub(crate) async fn derive_acl_manifest_entry(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    input: ManifestDerivationInput<
        AclManifestId,
        AclManifestRestriction,
        LoadableShardedMapV2Node<AclManifestEntry>,
    >,
    changes: Vec<(NonRootMPath, AclFileChange)>,
    acl_file_name: &str,
) -> Result<
    ManifestStageOutput<
        AclManifestId,
        AclManifestRestriction,
        LoadableShardedMapV2Node<AclManifestEntry>,
    >,
> {
    let acl_file_bytes: Vec<u8> = acl_file_name.as_bytes().to_vec();
    let prepared: HashMap<_, _> = prepare_acl_file_changes(
        ctx,
        blobstore,
        changes
            .iter()
            .filter(|(path, _)| input.contains_change(path) || path.is_prefix_of(input.path()))
            .cloned()
            .collect(),
        acl_file_name,
    )
    .await?
    .into_iter()
    .collect();
    let changes = changes
        .into_iter()
        .map(|(path, change)| {
            let change = match change {
                AclFileChange::AddOrModify(id) if !matches!(prepared.get(&path), Some(None)) => {
                    Some(id)
                }
                _ => None,
            };
            (path, change)
        })
        .collect();
    let create_leaf = acl_create_leaf_callback(ctx.clone(), blobstore.clone());

    input
        .derive(
            ctx.clone(),
            blobstore.clone(),
            changes,
            acl_create_tree_callback(ctx.clone(), blobstore.clone(), acl_file_bytes),
            move |LeafInfo {
                      path,
                      change,
                      parents,
                  }| {
                let change = change
                    .map(|_: ContentId| {
                        prepared
                            .get(&path)
                            .and_then(Option::as_ref)
                            .cloned()
                            .with_context(|| format!("missing prepared ACL file at {path}"))
                    })
                    .transpose();
                let create_leaf = create_leaf.clone();
                async move {
                    create_leaf(LeafInfo {
                        path,
                        change: change?,
                        parents,
                    })
                    .await
                }
            },
            |subentries| async move {
                Ok(LoadableShardedMapV2Node::Inlined(
                    create_acl_manifest_subentries(ctx.clone(), blobstore.clone(), subentries)
                        .await?,
                ))
            },
        )
        .await
}

// ---------------------------------------------------------------------------
// create_tree callback
// ---------------------------------------------------------------------------

pub(crate) async fn create_acl_manifest_subentries(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    subentries: TreeInfoSubentries<
        AclManifestId,
        AclManifestRestriction,
        AclManifestNodeInfo,
        LoadableShardedMapV2Node<AclManifestEntry>,
    >,
) -> Result<ShardedMapV2Node<AclManifestEntry>> {
    // Convert subentries, loading child manifests for boundary-reuse entries.
    let acl_subentries: TrieMap<
        Either<AclManifestEntry, LoadableShardedMapV2Node<AclManifestEntry>>,
    > = stream::iter(subentries)
        .then(|(prefix, entry_or_map)| {
            cloned!(ctx, blobstore);
            async move {
                let converted = match entry_or_map {
                    Either::Left((maybe_info, entry)) => {
                        let acl_entry =
                            convert_manifest_entry(&ctx, &blobstore, maybe_info, entry).await?;
                        Either::Left(acl_entry)
                    }
                    Either::Right(map) => Either::Right(map),
                };
                Ok::<_, anyhow::Error>((prefix, converted))
            }
        })
        .try_collect()
        .await?;

    // Build the ShardedMapV2.
    let subentries =
        ShardedMapV2Node::from_entries_and_partial_maps(&ctx, &blobstore, acl_subentries).await?;

    Ok(subentries)
}

/// Build an `AclManifest` node from `derive_manifest`'s subentries.
///
/// 1. Convert each subentry from `Entry<AclManifestId, AclManifestRestriction>`
///    to `AclManifestEntry`, using `Ctx` for directory flags when available
///    and loading the child manifest for boundary-reuse entries.
/// 2. Build the ShardedMapV2 via `from_entries_and_partial_maps`.
/// 3. Look up the ACL file in the built map to determine the restriction.
/// 4. Compute `has_restricted_descendants` by checking for Directory entries.
/// 5. Store the manifest blob and return the ID with node info.
pub(crate) async fn create_acl_manifest(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    acl_file_name: &[u8],
    subentries: TreeInfoSubentries<
        AclManifestId,
        AclManifestRestriction,
        AclManifestNodeInfo,
        LoadableShardedMapV2Node<AclManifestEntry>,
    >,
) -> Result<(AclManifestNodeInfo, AclManifestId)> {
    // Compute has_restricted_descendants from the original subentries before
    // consuming them. This avoids loading the built ShardedMapV2 entries.
    // - Either::Left with Entry::Tree = a directory child → restricted descendants
    // - Either::Right = reused map chunk → check its rollup data
    let has_restricted_descendants = subentries.values().any(|entry_or_map| match entry_or_map {
        Either::Left((_, Entry::Tree(_))) => true,
        Either::Left((_, Entry::Leaf(_))) => false,
        Either::Right(reused_map) => reused_map.rollup_data().has_restricted,
    });

    let subentries =
        create_acl_manifest_subentries(ctx.clone(), blobstore.clone(), subentries).await?;

    // Look up the ACL file to determine this directory's restriction.
    let restriction = match subentries.lookup(&ctx, &blobstore, acl_file_name).await? {
        Some(AclManifestEntry::AclFile(restriction)) => {
            AclManifestDirectoryRestriction::Restricted(restriction)
        }
        _ => AclManifestDirectoryRestriction::Unrestricted,
    };

    let is_restricted = matches!(restriction, AclManifestDirectoryRestriction::Restricted(_));

    // Store and return.
    let manifest = AclManifest {
        restriction,
        subentries,
    };
    let id = manifest.into_blob().store(&ctx, &blobstore).await?;

    Ok((
        AclManifestNodeInfo {
            is_restricted,
            has_restricted_descendants,
        },
        id,
    ))
}

/// Convert a `derive_manifest` `Entry` to an `AclManifestEntry`.
///
/// For `Entry::Leaf`: trivial conversion to `AclFile`.
/// For `Entry::Tree` with `Some(ctx)`: use pre-computed flags from child's `create_tree`.
/// For `Entry::Tree` with `None` (boundary reuse): load the child manifest.
async fn convert_manifest_entry(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    maybe_info: Option<AclManifestNodeInfo>,
    entry: Entry<AclManifestId, AclManifestRestriction>,
) -> Result<AclManifestEntry> {
    match entry {
        Entry::Leaf(restriction) => Ok(AclManifestEntry::AclFile(restriction)),
        Entry::Tree(id) => {
            let (is_restricted, has_restricted_descendants) = match maybe_info {
                Some(info) => (info.is_restricted, info.has_restricted_descendants),
                None => {
                    // Boundary reuse — load child manifest to get flags.
                    let manifest = id.load(ctx, blobstore).await?;
                    let is_restricted = matches!(
                        manifest.restriction,
                        AclManifestDirectoryRestriction::Restricted(_)
                    );
                    // Use rollup data to check for restricted descendants
                    // instead of loading all subentries.
                    let has_restricted_descendants =
                        manifest.subentries.rollup_data().has_restricted;
                    (is_restricted, has_restricted_descendants)
                }
            };
            Ok(AclManifestEntry::Directory(AclManifestDirectoryEntry {
                id,
                is_restricted,
                has_restricted_descendants,
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// File changes collection
// ---------------------------------------------------------------------------

/// Collect all `.slacl` file changes: explicit from bonsai + implicit deletes.
///
/// Implicit deletes occur when a non-ACL file addition replaces a directory
/// that contained `.slacl` files in a parent manifest.
///
/// `stage_path` is the manifest path the supplied `parent_ids` are rooted at
/// (`MPath::ROOT` for canonical derivation). Returned change paths are always
/// absolute; only the parent lookup for implicit deletes is done relative to
/// `stage_path`.
pub(crate) async fn get_acl_file_changes(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    bonsai: &BonsaiChangeset,
    parents: Vec<
        ManifestStageOutput<
            AclManifestId,
            AclManifestRestriction,
            LoadableShardedMapV2Node<AclManifestEntry>,
        >,
    >,
    acl_file_name: &str,
    stage_path: &MPath,
) -> Result<Vec<(NonRootMPath, AclFileChange)>> {
    let acl_file_bytes = acl_file_name.as_bytes();

    // Explicit .slacl changes from bonsai file_changes().
    let explicit_changes: Vec<(NonRootMPath, AclFileChange)> = bonsai
        .file_changes()
        .filter(|(path, _)| path.basename().as_ref() == acl_file_bytes)
        .map(|(path, change)| {
            let file_change = match change.simplify() {
                Some(simplified) => AclFileChange::AddOrModify(simplified.content_id()),
                None => AclFileChange::Delete,
            };
            (path.clone(), file_change)
        })
        .collect();

    // Implicit deletes: file additions (non-ACL) that replace directories
    // containing .slacl files in parent manifests. The parent manifests are
    // rooted at `stage_path`; an addition above it replaces the entire stage.
    // Resolved leaves are re-prefixed back to absolute paths.
    let stage_prefix = stage_path.clone();
    let addition_prefixes: Vec<PathOrPrefix> = bonsai
        .file_changes()
        .filter(|(path, change)| change.is_changed() && path.basename().as_ref() != acl_file_bytes)
        .map(|(path, _)| MPath::from(path.clone()))
        .filter_map(|abs_path| {
            if abs_path.is_prefix_of(&stage_prefix) {
                Some(PathOrPrefix::Prefix(MPath::ROOT))
            } else if stage_prefix.is_prefix_of(&abs_path) {
                Some(PathOrPrefix::Prefix(
                    abs_path.remove_prefix_component(&stage_prefix),
                ))
            } else {
                None
            }
        })
        .collect();

    let implicit_deletes: Vec<(NonRootMPath, AclFileChange)> =
        if addition_prefixes.is_empty() || parents.is_empty() {
            vec![]
        } else {
            // Check each parent manifest for .slacl leaves under replaced paths.
            let all_implicit: HashSet<NonRootMPath> = stream::iter(parents)
                .then(|parent| {
                    cloned!(ctx, blobstore, stage_prefix);
                    let prefixes = addition_prefixes.clone();
                    async move {
                        parent
                            .find_entries(ctx, blobstore, prefixes)
                            .try_filter_map(|(rel_path, entry)| {
                                cloned!(stage_prefix);
                                async move {
                                    match entry {
                                        Entry::Leaf(_) => Ok(NonRootMPath::try_from(
                                            stage_prefix.join(rel_path.iter()),
                                        )
                                        .ok()),
                                        Entry::Tree(_) => Ok(None),
                                    }
                                }
                            })
                            .try_collect::<Vec<_>>()
                            .await
                    }
                })
                .try_collect::<Vec<_>>()
                .await?
                .into_iter()
                .flatten()
                .collect();

            all_implicit
                .into_iter()
                .map(|path| (path, AclFileChange::Delete))
                .collect()
        };

    Ok(explicit_changes
        .into_iter()
        .chain(implicit_deletes)
        .collect())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) async fn prepare_acl_file_changes(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    changes: Vec<(NonRootMPath, AclFileChange)>,
    acl_file_name: &str,
) -> Result<Vec<(NonRootMPath, Option<RestrictedPathsAclFile>)>> {
    stream::iter(changes)
        .map(|(path, change)| {
            cloned!(ctx, blobstore);
            let acl_file_name = acl_file_name.to_string();
            async move {
                match change {
                    AclFileChange::AddOrModify(content_id) => {
                        let acl_file = fetch_and_parse_acl_file(
                            &ctx,
                            &blobstore,
                            content_id,
                            &path,
                            &acl_file_name,
                        )
                        .await?;
                        Ok::<_, anyhow::Error>((path, acl_file))
                    }
                    AclFileChange::Delete => Ok((path, None)),
                }
            }
        })
        .buffer_unordered(100)
        .try_collect()
        .await
}

/// Fetch and parse an ACL file. `Ok(None)` means it did not parse: logged and
/// treated as absent, leaving the directory unrestricted.
pub(crate) async fn fetch_and_parse_acl_file(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    content_id: ContentId,
    path: &NonRootMPath,
    acl_file_name: &str,
) -> Result<Option<RestrictedPathsAclFile>> {
    let fetch_context = format!(
        "failed to fetch {acl_file_name} content for {path:?} with content id {content_id:?}"
    );
    let bytes = fetch_concat(blobstore, ctx, content_id)
        .await
        .context(fetch_context)?;
    let file_contents = FileContents::new_bytes(bytes);

    Ok(match parse_acl_file(&file_contents) {
        Ok(acl_file) => Some(acl_file),
        Err(error) => {
            log_invalid_acl_file(ctx.scuba().clone(), acl_file_name, path, &error);
            None
        }
    })
}

fn log_invalid_acl_file(
    mut scuba: MononokeScubaSampleBuilder,
    acl_file_name: &str,
    path: &NonRootMPath,
    error: &anyhow::Error,
) {
    let log_tag = format!("Invalid {acl_file_name} file");
    let msg = format!("Invalid {acl_file_name} file at {path}: {error:#}");
    tracing::error!(
        log_tag = %log_tag,
        acl_file_name = %acl_file_name,
        acl_path = %path,
        msg = %msg,
        "Invalid ACL file"
    );
    scuba.log_with_msg(&log_tag, Some(msg));
}

/// Store a parsed ACL file as an AclManifestEntryBlob and return the blob ID.
pub(crate) async fn store_acl_entry_from_acl_file(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    acl_file: &RestrictedPathsAclFile,
) -> Result<AclManifestEntryBlobId> {
    let entry_blob = AclManifestEntryBlob {
        repo_region_acl: acl_file.repo_region_acl().to_string(),
        permission_request_group: acl_file.permission_request_group().map(|g| g.to_string()),
        rollout_allowlist_group: acl_file.rollout_allowlist_group().map(|g| g.to_string()),
    };
    entry_blob.into_blob().store(ctx, blobstore).await
}

/// Resolve a file's ContentId via ContentManifests.
/// Only used by derive_from_scratch for backfilling.
async fn resolve_content_id(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    root_content_manifest_id: &RootContentManifestId,
    acl_path: &MPath,
) -> Result<ContentId> {
    let entry = root_content_manifest_id
        .content_manifest_id()
        .find_entry(ctx.clone(), blobstore.clone(), acl_path.clone())
        .await?
        .ok_or_else(|| anyhow::anyhow!("ACL file not found in ContentManifests: {acl_path:?}"))?;

    match entry {
        Entry::Leaf(f) => Ok(f.content_id),
        Entry::Tree(_) => anyhow::bail!("expected file but found directory: {acl_path:?}"),
    }
}

pub(crate) async fn empty_root_acl_manifest_id(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
) -> Result<RootAclManifestId> {
    let empty = AclManifest::empty();
    let id = empty.into_blob().store(ctx, blobstore).await?;
    Ok(RootAclManifestId(id))
}
