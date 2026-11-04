/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Low-level restriction lookup primitives.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use acl_manifest::RootAclManifestId;
use anyhow::Context;
use anyhow::Result;
use blobstore::Loadable;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use futures::StreamExt;
use futures::TryStreamExt;
use manifest::Entry;
use manifest::ManifestOps;
use manifest::PathOrPrefix;
use mercurial_types::HgAugmentedManifestEnvelope;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::fetch_augmented_manifest_envelope_opt;
use metaconfig_types::AclManifestMode;
use metaconfig_types::PathRestrictionMetadata;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use mononoke_types::acl_manifest::AclManifest;
use mononoke_types::acl_manifest::AclManifestDirectoryRestriction;
use mononoke_types::acl_manifest::AclManifestEntryBlob;
use mononoke_types::acl_manifest::AclManifestRestriction;
use mononoke_types::typed_hash::AclManifestId;
use permission_checker::MononokeIdentity;

use crate::ManifestType;
use crate::PermissionRequestGroup;
use crate::RestrictedManifestId;
use crate::RestrictedPaths;

#[cfg(test)]
mod tests;

/// Core restriction information for a path.
/// Does not include access check results; that is the API layer's concern
/// (see `mononoke_api::PathAccessInfo`).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct PathRestrictionInfo {
    /// The root path of this restriction (directory containing `.slacl`).
    pub restriction_root: NonRootMPath,
    /// The repo region ACL string, e.g. "REPO_REGION:repos/hg/fbsource/=project1".
    pub repo_region_acl: String,
    /// Permission request group to show users. Defaults to repo_region_acl if not configured.
    pub permission_request_group: PermissionRequestGroup,
    /// AMP group temporarily allowlisted to read this restriction during
    /// rollout. `None` means this restriction has no allowlist, and callers are
    /// never rollout-allowlisted for it.
    pub rollout_allowlist_group: Option<MononokeIdentity>,
}

/// Core restriction information for a manifest access.
#[derive(Clone, Debug, PartialEq)]
pub struct ManifestRestrictionInfo {
    /// The matched restriction root when it is known from the source.
    pub restriction_root: Option<NonRootMPath>,
    /// The repo region ACL string, e.g. "REPO_REGION:repos/hg/fbsource/=project1".
    pub repo_region_acl: String,
    /// Permission request group to show users. Defaults to repo_region_acl if not configured.
    pub permission_request_group: PermissionRequestGroup,
    /// AMP group temporarily allowlisted to read this restriction during
    /// rollout. `None` means this restriction has no allowlist, and callers are
    /// never rollout-allowlisted for it.
    pub rollout_allowlist_group: Option<MononokeIdentity>,
}

/// Get config-backed restriction info for paths that are themselves restriction roots.
pub(crate) fn get_path_restriction_root_info_from_config(
    restricted_paths: &RestrictedPaths,
    paths: &[NonRootMPath],
) -> Vec<PathRestrictionInfo> {
    paths
        .iter()
        .filter_map(|path| {
            restricted_paths
                .config_based()
                .get_metadata_for_path(path)
                .map(|metadata| build_config_path_restriction_info(path.clone(), metadata))
        })
        .collect()
}

/// Get config-backed restriction info for one or more paths, considering ancestors.
pub(crate) fn get_path_restriction_info_from_config(
    restricted_paths: &RestrictedPaths,
    paths: &[NonRootMPath],
) -> Vec<PathRestrictionInfo> {
    paths
        .iter()
        .flat_map(|path| get_config_path_restriction_info_for_path(restricted_paths, path))
        .collect()
}

/// Find config-backed restriction roots that are descendants of the given roots.
pub(crate) fn find_restricted_descendants_from_config(
    restricted_paths: &RestrictedPaths,
    roots: &[MPath],
) -> Vec<PathRestrictionInfo> {
    let mut results: Vec<PathRestrictionInfo> = restricted_paths
        .config()
        .path_restriction_metadata
        .iter()
        .filter(|(root, _)| {
            roots
                .iter()
                .any(|query_root| query_root.is_prefix_of(*root))
        })
        .map(|(root, metadata)| build_config_path_restriction_info(root.clone(), metadata))
        .collect();
    results.sort_by(|left, right| left.restriction_root.cmp(&right.restriction_root));
    results.dedup_by(|left, right| left.restriction_root == right.restriction_root);
    results
}

/// Get AclManifest restriction info for paths that are themselves restriction roots.
pub(crate) async fn get_path_restriction_root_info_from_acl_manifest(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    cs_id: ChangesetId,
    paths: &[NonRootMPath],
) -> Result<Vec<PathRestrictionInfo>> {
    let root_id = derive_acl_manifest(restricted_paths, ctx, cs_id).await?;
    let blobstore = restricted_paths
        .repo_derived_data
        .manager()
        .repo_blobstore();
    find_restrictions_at_paths_from_acl_manifest(
        ctx,
        blobstore,
        root_id.into_inner_id(),
        paths.iter().cloned().map(MPath::from).collect(),
    )
    .await
}

/// Get AclManifest restriction info for one or more paths, considering ancestors.
pub(crate) async fn get_path_restriction_info_from_acl_manifest(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    cs_id: ChangesetId,
    paths: &[NonRootMPath],
) -> Result<Vec<PathRestrictionInfo>> {
    let root_id = derive_acl_manifest(restricted_paths, ctx, cs_id).await?;
    let blobstore = restricted_paths
        .repo_derived_data
        .manager()
        .repo_blobstore();
    find_restrictions_at_paths_from_acl_manifest(
        ctx,
        blobstore,
        root_id.into_inner_id(),
        collect_path_prefixes(paths),
    )
    .await
}

/// Find AclManifest restriction roots that are descendants of the given roots.
pub(crate) async fn find_restricted_descendants_from_acl_manifest(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    cs_id: ChangesetId,
    roots: Vec<MPath>,
) -> Result<Vec<PathRestrictionInfo>> {
    let root_id = derive_acl_manifest(restricted_paths, ctx, cs_id).await?;
    let blobstore = restricted_paths
        .repo_derived_data
        .manager()
        .repo_blobstore();
    find_restricted_descendants_from_acl_manifest_root(
        ctx,
        blobstore,
        root_id.into_inner_id(),
        roots,
    )
    .await
}

/// Get manifest-id-store paths for a manifest access through the config-backed source.
pub(crate) async fn get_manifest_restricted_paths_from_config(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    manifest_id: &RestrictedManifestId,
    manifest_type: &ManifestType,
) -> Result<Vec<NonRootMPath>> {
    if let Some(manifest_id_cache) = restricted_paths.config_based().manifest_id_cache() {
        let cache_guard = manifest_id_cache
            .cache()
            .read()
            .map_err(|err| anyhow::anyhow!("Failed to acquire cache read lock: {err}"))?;
        return Ok(cache_guard
            .get(manifest_type)
            .and_then(|type_map| type_map.get(manifest_id))
            .map(|paths| paths.iter().cloned().collect())
            .unwrap_or_default());
    }

    restricted_paths
        .config_based()
        .manifest_id_store()
        .get_paths_by_manifest_id(ctx, manifest_id, manifest_type)
        .await
}

/// Pair each config-backed restriction root matching a manifest-id-store path
/// with its repo-region ACL and its rollout allowlist group, so the allowlist
/// can be evaluated per tent.
pub(crate) fn get_config_restrictions_for_paths<'a>(
    restricted_paths: &'a RestrictedPaths,
    paths: &[NonRootMPath],
) -> Vec<(&'a MononokeIdentity, Option<&'a MononokeIdentity>)> {
    paths
        .iter()
        .filter_map(|path| {
            restricted_paths
                .config_based()
                .get_metadata_for_path(path)
                .map(|metadata| {
                    (
                        &metadata.repo_region_acl,
                        metadata.rollout_allowlist_group.as_ref(),
                    )
                })
        })
        .collect()
}

/// Get restriction info for paths that are themselves restriction roots.
pub(crate) async fn get_path_restriction_root_info(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    cs_id: Option<ChangesetId>,
    paths: &[NonRootMPath],
) -> Result<Vec<PathRestrictionInfo>> {
    match restricted_paths.config().acl_manifest_mode {
        AclManifestMode::Disabled | AclManifestMode::Shadow => Ok(
            get_path_restriction_root_info_from_config(restricted_paths, paths),
        ),
        AclManifestMode::Both => {
            let config = get_path_restriction_root_info_from_config(restricted_paths, paths);
            let cs_id = cs_id
                .context("ChangesetId is required for ACL manifest-based restriction lookup")?;
            let acl_manifest = get_path_restriction_root_info_from_acl_manifest(
                restricted_paths,
                ctx,
                cs_id,
                paths,
            )
            .await?;
            Ok(union_path_restriction_info_with_acl_manifest_precedence(
                config,
                acl_manifest,
            ))
        }
        AclManifestMode::Authoritative => {
            let cs_id = cs_id
                .context("ChangesetId is required for ACL manifest-based restriction lookup")?;
            get_path_restriction_root_info_from_acl_manifest(restricted_paths, ctx, cs_id, paths)
                .await
        }
    }
}

/// Get restriction info for one or more paths, considering ancestor restrictions.
pub(crate) async fn get_path_restriction_info(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    cs_id: Option<ChangesetId>,
    paths: &[NonRootMPath],
) -> Result<Vec<PathRestrictionInfo>> {
    match restricted_paths.config().acl_manifest_mode {
        AclManifestMode::Disabled | AclManifestMode::Shadow => Ok(
            get_path_restriction_info_from_config(restricted_paths, paths),
        ),
        AclManifestMode::Both => {
            let config = get_path_restriction_info_from_config(restricted_paths, paths);
            let cs_id = cs_id
                .context("ChangesetId is required for ACL manifest-based restriction lookup")?;
            let acl_manifest =
                get_path_restriction_info_from_acl_manifest(restricted_paths, ctx, cs_id, paths)
                    .await?;
            Ok(union_path_restriction_info_with_acl_manifest_precedence(
                config,
                acl_manifest,
            ))
        }
        AclManifestMode::Authoritative => {
            let cs_id = cs_id
                .context("ChangesetId is required for ACL manifest-based restriction lookup")?;
            get_path_restriction_info_from_acl_manifest(restricted_paths, ctx, cs_id, paths).await
        }
    }
}

/// Check if a path is itself a restriction root.
pub(crate) async fn is_restriction_root(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    cs_id: Option<ChangesetId>,
    path: &NonRootMPath,
) -> Result<bool> {
    get_path_restriction_root_info(restricted_paths, ctx, cs_id, std::slice::from_ref(path))
        .await
        .map(|result| !result.is_empty())
}

/// Check if a path is restricted, considering ancestor directories.
pub(crate) async fn is_restricted_path(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    cs_id: Option<ChangesetId>,
    path: &NonRootMPath,
) -> Result<bool> {
    get_path_restriction_info(restricted_paths, ctx, cs_id, std::slice::from_ref(path))
        .await
        .map(|result| !result.is_empty())
}

/// Returns whether an HgAugmented manifest is a restriction root according to
/// the sources enabled by `AclManifestMode`.
///
/// `preloaded_is_restricted` is the AclManifest-backed restriction bit already
/// loaded by the caller from the HgAugmented manifest. When the config-backed
/// source is consulted, this checks the manifest-id store directly and does not
/// revalidate the returned paths against the current config. That is
/// intentional: manifests recorded while a path was configured as restricted
/// should remain restricted even if that path is later removed from config.
pub async fn is_restricted_manifest(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    manifest_id: &RestrictedManifestId,
    manifest_type: &ManifestType,
    preloaded_is_restricted: bool,
) -> Result<bool> {
    if !matches!(manifest_type, ManifestType::HgAugmented) {
        return unsupported_acl_manifest_type_error(manifest_type);
    }

    if !restricted_paths.may_have_restricted_paths() {
        return Ok(false);
    }

    match restricted_paths.config().acl_manifest_mode {
        AclManifestMode::Disabled | AclManifestMode::Shadow => {
            is_restricted_manifest_from_config(restricted_paths, ctx, manifest_id, manifest_type)
                .await
        }
        AclManifestMode::Authoritative => Ok(preloaded_is_restricted),
        AclManifestMode::Both => {
            if preloaded_is_restricted {
                Ok(true)
            } else {
                is_restricted_manifest_from_config(
                    restricted_paths,
                    ctx,
                    manifest_id,
                    manifest_type,
                )
                .await
            }
        }
    }
}

async fn is_restricted_manifest_from_config(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    manifest_id: &RestrictedManifestId,
    manifest_type: &ManifestType,
) -> Result<bool> {
    // The config source can only restrict a manifest when there are configured
    // `path_acls`. For repos with none, skip the manifest-id store entirely --
    // otherwise every per-child augmented-tree lookup falls through to a
    // `SelectPathsByManifestId` SQL query, which at fleet QPS caused a SQL storm
    // (S672712 / T248658346). Mirrors the enforcement path's
    // `config_manifest_may_restrict` guard in lib.rs.
    if restricted_paths.config().is_empty() {
        return Ok(false);
    }

    Ok(!get_manifest_restricted_paths_from_config(
        restricted_paths,
        ctx,
        manifest_id,
        manifest_type,
    )
    .await?
    .is_empty())
}

/// Find all restriction roots that are descendants of any of the given root paths.
pub(crate) async fn find_restricted_descendants(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    cs_id: Option<ChangesetId>,
    roots: Vec<MPath>,
) -> Result<Vec<PathRestrictionInfo>> {
    match restricted_paths.config().acl_manifest_mode {
        AclManifestMode::Disabled | AclManifestMode::Shadow => Ok(
            find_restricted_descendants_from_config(restricted_paths, &roots),
        ),
        AclManifestMode::Both => {
            let config = find_restricted_descendants_from_config(restricted_paths, &roots);
            let cs_id = cs_id
                .context("ChangesetId is required for ACL manifest-based restriction lookup")?;
            let acl_manifest =
                find_restricted_descendants_from_acl_manifest(restricted_paths, ctx, cs_id, roots)
                    .await?;
            Ok(union_path_restriction_info_with_acl_manifest_precedence(
                config,
                acl_manifest,
            ))
        }
        AclManifestMode::Authoritative => {
            let cs_id = cs_id
                .context("ChangesetId is required for ACL manifest-based restriction lookup")?;
            find_restricted_descendants_from_acl_manifest(restricted_paths, ctx, cs_id, roots).await
        }
    }
}

/// Lookup restriction info for a manifest access using the configured mode.
pub(crate) async fn get_manifest_restriction_info(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    manifest_id: &RestrictedManifestId,
    manifest_type: &ManifestType,
) -> Result<Vec<ManifestRestrictionInfo>> {
    let mode = restricted_paths.config().acl_manifest_mode;
    let acl_manifest_supported = matches!(manifest_type, ManifestType::HgAugmented);
    match (mode, acl_manifest_supported) {
        (AclManifestMode::Disabled, _) | (AclManifestMode::Shadow, _) => {
            get_manifest_restriction_info_from_config(
                restricted_paths,
                ctx,
                manifest_id,
                manifest_type,
            )
            .await
        }
        (AclManifestMode::Authoritative, true) => {
            get_manifest_restriction_info_from_acl_manifest(
                restricted_paths,
                ctx,
                manifest_id,
                manifest_type,
            )
            .await
        }
        (AclManifestMode::Authoritative, false) => {
            unsupported_acl_manifest_type_error(manifest_type)
        }
        (AclManifestMode::Both, true) => {
            let (config, acl_manifest) = tokio::try_join!(
                get_manifest_restriction_info_from_config(
                    restricted_paths,
                    ctx,
                    manifest_id,
                    manifest_type,
                ),
                get_manifest_restriction_info_from_acl_manifest(
                    restricted_paths,
                    ctx,
                    manifest_id,
                    manifest_type,
                ),
            )?;
            Ok(union_manifest_restriction_info_with_acl_manifest_precedence(config, acl_manifest))
        }
        (AclManifestMode::Both, false) => {
            get_manifest_restriction_info_from_config(
                restricted_paths,
                ctx,
                manifest_id,
                manifest_type,
            )
            .await
        }
    }
}

/// Lookup restriction info for a manifest access through the config-backed source.
pub(crate) async fn get_manifest_restriction_info_from_config(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    manifest_id: &RestrictedManifestId,
    manifest_type: &ManifestType,
) -> Result<Vec<ManifestRestrictionInfo>> {
    let paths = get_manifest_restricted_paths_from_config(
        restricted_paths,
        ctx,
        manifest_id,
        manifest_type,
    )
    .await?;
    Ok(paths
        .into_iter()
        .filter_map(|path| {
            restricted_paths
                .config_based()
                .get_metadata_for_path(&path)
                .map(|metadata| ManifestRestrictionInfo {
                    restriction_root: Some(path),
                    repo_region_acl: metadata.repo_region_acl.to_string(),
                    permission_request_group: metadata.effective_permission_request_group(),
                    rollout_allowlist_group: metadata.rollout_allowlist_group.clone(),
                })
        })
        .collect())
}

/// Lookup restriction info for a manifest access through the AclManifest source.
pub(crate) async fn get_manifest_restriction_info_from_acl_manifest(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    manifest_id: &RestrictedManifestId,
    manifest_type: &ManifestType,
) -> Result<Vec<ManifestRestrictionInfo>> {
    let Some(acl_manifest_directory_id) = load_acl_manifest_directory_id_from_manifest(
        restricted_paths,
        ctx,
        manifest_id,
        manifest_type,
    )
    .await?
    else {
        return Ok(vec![]);
    };

    let blobstore = restricted_paths
        .repo_derived_data
        .manager()
        .repo_blobstore();
    let acl_manifest =
        load_acl_manifest_directory(ctx, blobstore, acl_manifest_directory_id).await?;

    match acl_manifest.restriction {
        AclManifestDirectoryRestriction::Restricted(ref restriction) => Ok(vec![
            load_manifest_restriction_info(ctx, blobstore, restriction).await?,
        ]),
        AclManifestDirectoryRestriction::Unrestricted => Ok(vec![]),
    }
}

fn union_path_restriction_info_with_acl_manifest_precedence(
    config: Vec<PathRestrictionInfo>,
    acl_manifest: Vec<PathRestrictionInfo>,
) -> Vec<PathRestrictionInfo> {
    // Metadata Both mode reports the union of both sources. For duplicate
    // roots, the AclManifest entry wins over config; Both enforcement still
    // denies if either source denies.
    config
        .into_iter()
        .map(|info| (info.restriction_root.clone(), info))
        .chain(
            acl_manifest
                .into_iter()
                .map(|info| (info.restriction_root.clone(), info)),
        )
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect()
}

fn union_manifest_restriction_info_with_acl_manifest_precedence(
    config: Vec<ManifestRestrictionInfo>,
    acl_manifest: Vec<ManifestRestrictionInfo>,
) -> Vec<ManifestRestrictionInfo> {
    let (rootless_acl_manifest, rooted_acl_manifest): (Vec<_>, Vec<_>) = acl_manifest
        .into_iter()
        .partition(|info| info.restriction_root.is_none());
    let (rootless_config, rooted_config): (Vec<_>, Vec<_>) = config
        .into_iter()
        .partition(|info| info.restriction_root.is_none());

    // Rootless manifest metadata cannot be safely deduplicated because there is
    // no restriction-root key to compare. Preserve those entries and only apply
    // AclManifest precedence when both sources report a known root.
    let rooted = rooted_config
        .into_iter()
        .map(|info| (info.restriction_root.clone(), info))
        .chain(
            rooted_acl_manifest
                .into_iter()
                .map(|info| (info.restriction_root.clone(), info)),
        )
        .collect::<BTreeMap<_, _>>()
        .into_values();

    rootless_acl_manifest
        .into_iter()
        .chain(rootless_config)
        .chain(rooted)
        .collect()
}

fn unsupported_acl_manifest_type_error<T>(manifest_type: &ManifestType) -> Result<T> {
    anyhow::bail!(
        "AclManifest manifest restriction lookup only supports HgAugmented manifests, got {manifest_type}"
    )
}

fn get_config_path_restriction_info_for_path(
    restricted_paths: &RestrictedPaths,
    path: &NonRootMPath,
) -> Vec<PathRestrictionInfo> {
    restricted_paths
        .config()
        .path_restriction_metadata
        .iter()
        .filter(|(prefix, _)| prefix.is_prefix_of(path))
        .map(|(prefix, metadata)| build_config_path_restriction_info(prefix.clone(), metadata))
        .collect()
}

async fn derive_acl_manifest(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    cs_id: ChangesetId,
) -> Result<RootAclManifestId> {
    restricted_paths
        .repo_derived_data
        .derive::<RootAclManifestId>(ctx, cs_id, DerivationPriority::LOW)
        .await
        .map_err(anyhow::Error::from)
}

fn collect_path_prefixes(paths: &[NonRootMPath]) -> Vec<MPath> {
    paths
        .iter()
        .flat_map(|path| {
            path.into_iter()
                .scan(None::<NonRootMPath>, |acc, element| {
                    let next = NonRootMPath::join_opt_element(acc.as_ref(), element);
                    *acc = Some(next.clone());
                    Some(MPath::from(next))
                })
                .collect::<Vec<_>>()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

async fn find_restrictions_at_paths_from_acl_manifest<S>(
    ctx: &CoreContext,
    blobstore: &S,
    root_id: AclManifestId,
    paths: Vec<MPath>,
) -> Result<Vec<PathRestrictionInfo>>
where
    S: blobstore::KeyedBlobstore + Clone + Send + Sync + 'static,
{
    let mut results = root_id
        .find_entries(
            ctx.clone(),
            blobstore.clone(),
            paths.into_iter().map(PathOrPrefix::Path),
        )
        .map(|entry_result| {
            let ctx = ctx.clone();
            let blobstore = blobstore.clone();
            async move {
                let (path, entry) = entry_result?;
                resolve_tree_entry_restriction(&ctx, &blobstore, path, entry).await
            }
        })
        .buffer_unordered(100)
        .try_filter_map(|info| async move { Ok(info) })
        .try_collect()
        .await?;
    sort_and_dedup_path_restriction_info(&mut results);
    Ok(results)
}

async fn find_restricted_descendants_from_acl_manifest_root<S>(
    ctx: &CoreContext,
    blobstore: &S,
    root_id: AclManifestId,
    roots: Vec<MPath>,
) -> Result<Vec<PathRestrictionInfo>>
where
    S: blobstore::KeyedBlobstore + Clone + Send + Sync + 'static,
{
    let mut results = root_id
        .find_entries(
            ctx.clone(),
            blobstore.clone(),
            roots.into_iter().map(PathOrPrefix::Prefix),
        )
        .map(|entry_result| {
            let ctx = ctx.clone();
            let blobstore = blobstore.clone();
            async move {
                let (path, entry) = entry_result?;
                resolve_tree_entry_restriction(&ctx, &blobstore, path, entry).await
            }
        })
        .buffer_unordered(100)
        .try_filter_map(|info| async move { Ok(info) })
        .try_collect()
        .await?;
    sort_and_dedup_path_restriction_info(&mut results);
    Ok(results)
}

fn sort_and_dedup_path_restriction_info(results: &mut Vec<PathRestrictionInfo>) {
    results.sort_by(|left, right| left.restriction_root.cmp(&right.restriction_root));
    results.dedup_by(|left, right| left.restriction_root == right.restriction_root);
}

async fn resolve_tree_entry_restriction<S, LeafId>(
    ctx: &CoreContext,
    blobstore: &S,
    path: MPath,
    entry: Entry<AclManifestId, LeafId>,
) -> Result<Option<PathRestrictionInfo>>
where
    S: blobstore::KeyedBlobstore + Clone + Send + Sync + 'static,
{
    match entry {
        Entry::Tree(manifest_id) => {
            let restriction_root = match NonRootMPath::try_from(path) {
                Ok(path) => path,
                Err(_) => return Ok(None),
            };
            let acl_manifest = load_acl_manifest_directory(ctx, blobstore, manifest_id).await?;
            match acl_manifest.restriction {
                AclManifestDirectoryRestriction::Restricted(ref restriction) => Ok(Some(
                    load_path_restriction_info(ctx, blobstore, restriction, restriction_root)
                        .await?,
                )),
                AclManifestDirectoryRestriction::Unrestricted => Ok(None),
            }
        }
        Entry::Leaf(_) => Ok(None),
    }
}

async fn load_acl_manifest_directory(
    ctx: &CoreContext,
    blobstore: &impl blobstore::KeyedBlobstore,
    acl_manifest_id: AclManifestId,
) -> Result<AclManifest> {
    acl_manifest_id
        .load(ctx, blobstore)
        .await
        .context("Failed to load AclManifest directory")
}

async fn load_acl_entry_blob_fields(
    ctx: &CoreContext,
    blobstore: &impl blobstore::KeyedBlobstore,
    restriction: &AclManifestRestriction,
) -> Result<(String, PermissionRequestGroup, Option<MononokeIdentity>)> {
    let entry_blob: AclManifestEntryBlob = restriction
        .entry_blob_id
        .load(ctx, blobstore)
        .await
        .context("Failed to load AclManifestEntryBlob")?;
    let repo_region_acl = entry_blob.repo_region_acl;
    let permission_request_group = entry_blob
        .permission_request_group
        .unwrap_or_else(|| repo_region_acl.clone())
        .parse()
        .with_context(|| "Failed to parse permission request group")?;
    // Already stored in `GROUP:<name>` form by derivation, so parse it as an
    // identity rather than re-applying the bare-name prefixing.
    let rollout_allowlist_group = entry_blob
        .rollout_allowlist_group
        .map(|group| {
            group
                .parse()
                .with_context(|| format!("Failed to parse rollout allowlist group `{group}`"))
        })
        .transpose()?;
    Ok((
        repo_region_acl,
        permission_request_group,
        rollout_allowlist_group,
    ))
}

async fn load_manifest_restriction_info(
    ctx: &CoreContext,
    blobstore: &impl blobstore::KeyedBlobstore,
    restriction: &AclManifestRestriction,
) -> Result<ManifestRestrictionInfo> {
    let (repo_region_acl, permission_request_group, rollout_allowlist_group) =
        load_acl_entry_blob_fields(ctx, blobstore, restriction).await?;
    Ok(ManifestRestrictionInfo {
        restriction_root: None,
        repo_region_acl,
        permission_request_group,
        rollout_allowlist_group,
    })
}

async fn load_path_restriction_info(
    ctx: &CoreContext,
    blobstore: &impl blobstore::KeyedBlobstore,
    restriction: &AclManifestRestriction,
    restriction_root: NonRootMPath,
) -> Result<PathRestrictionInfo> {
    let (repo_region_acl, permission_request_group, rollout_allowlist_group) =
        load_acl_entry_blob_fields(ctx, blobstore, restriction).await?;
    Ok(PathRestrictionInfo {
        restriction_root,
        repo_region_acl,
        permission_request_group,
        rollout_allowlist_group,
    })
}

fn build_config_path_restriction_info(
    restriction_root: NonRootMPath,
    metadata: &PathRestrictionMetadata,
) -> PathRestrictionInfo {
    PathRestrictionInfo {
        restriction_root,
        repo_region_acl: metadata.repo_region_acl.to_string(),
        permission_request_group: metadata.effective_permission_request_group(),
        rollout_allowlist_group: metadata.rollout_allowlist_group.clone(),
    }
}

async fn load_acl_manifest_directory_id_from_manifest(
    restricted_paths: &RestrictedPaths,
    ctx: &CoreContext,
    manifest_id: &RestrictedManifestId,
    manifest_type: &ManifestType,
) -> Result<Option<AclManifestId>> {
    let ManifestType::HgAugmented = manifest_type else {
        return unsupported_acl_manifest_type_error(manifest_type);
    };

    let hg_augmented_manifest_id =
        HgAugmentedManifestId::from_bytes(manifest_id.as_inner().as_slice())
            .with_context(|| format!("Failed to parse HgAugmented manifest id {manifest_id}"))?;
    let blobstore = restricted_paths
        .repo_derived_data
        .manager()
        .repo_blobstore();
    let Some(envelope): Option<HgAugmentedManifestEnvelope> =
        fetch_augmented_manifest_envelope_opt(ctx, blobstore, hg_augmented_manifest_id)
            .await
            .with_context(|| {
                format!(
                    "Failed to load HgAugmentedManifest envelope for manifest type {manifest_type} id {manifest_id}"
                )
            })?
    else {
        return Ok(None);
    };

    Ok(envelope.augmented_manifest.acl_manifest_directory_id)
}
