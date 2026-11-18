/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use bytes::Bytes;
use cloned::cloned;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use futures::StreamExt;
use futures::TryFutureExt;
use futures::TryStreamExt;
use futures::future;
use manifest::ManifestOps;
use manifest::find_intersection_of_diffs;
use mononoke_macros::mononoke;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::FileUnodeId;
use mononoke_types::NonRootMPath;
use mononoke_types::blame_v2::BlameParent;
use mononoke_types::blame_v2::BlameParentId;
use mononoke_types::blame_v2::BlameV2;
use mononoke_types::blame_v2::BlameV2Id;
use mononoke_types::blame_v2::load_blame_with_prefix;
use mononoke_types::blame_v2::store_blame_with_prefix;
use unodes::RootUnodeManifestId;
use unodes::UnodeRenameSource;
use unodes::UnodeRenameSources;
use unodes::fetch_pipeline_unode_root;
use unodes::find_unode_rename_sources;

use crate::DEFAULT_BLAME_FILESIZE_LIMIT;
use crate::fetch::FetchOutcome;
use crate::fetch::fetch_content_for_blame_with_limit;

pub(crate) async fn derive_blame_v2(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: BonsaiChangeset,
    root_manifest: RootUnodeManifestId,
) -> Result<(), Error> {
    let csid = bonsai.get_changeset_id();
    let parent_manifests = bonsai.parents().map(|csid| async move {
        let parent_root_mf_id = derivation_ctx
            .fetch_dependency::<RootUnodeManifestId>(ctx, csid)
            .await?;
        Ok::<_, Error>(parent_root_mf_id.manifest_unode_id().clone())
    });

    let (parent_manifests, renames) = future::try_join(
        future::try_join_all(parent_manifests).err_into(),
        find_unode_rename_sources(ctx, derivation_ctx, &bonsai),
    )
    .await?;

    let filesize_limit = derivation_ctx
        .config()
        .blame_filesize_limit
        .unwrap_or(DEFAULT_BLAME_FILESIZE_LIMIT);
    let renames = Arc::new(renames);
    let blobstore = derivation_ctx.blobstore();
    find_intersection_of_diffs(
        ctx.clone(),
        blobstore.clone(),
        root_manifest.manifest_unode_id().clone(),
        parent_manifests,
    )
    .map_ok(|(path, entry)| Some((Option::<NonRootMPath>::from(path)?, entry.into_leaf()?)))
    .try_filter_map(future::ok)
    .map(move |path_and_file_unode| {
        cloned!(ctx, derivation_ctx, blobstore, renames);
        async move {
            let (path, file_unode) = path_and_file_unode?;
            mononoke::spawn_task(async move {
                create_blame_v2(
                    &ctx,
                    &derivation_ctx,
                    &blobstore,
                    renames,
                    csid,
                    path,
                    file_unode,
                    filesize_limit,
                    "",
                )
                .await
            })
            .await?
        }
    })
    .buffered(256)
    .try_for_each(|_| future::ok(()))
    .await?;

    Ok(())
}

/// Compute and store blame for a single file.  `renames` is the rename map for
/// the changeset; the source for `path` (if any) is looked up internally.
/// `blame_key_prefix` namespaces the stored blame blob and the parent blame
/// reads (empty for the canonical key).
pub(crate) async fn create_blame_v2(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    renames: Arc<UnodeRenameSources>,
    csid: ChangesetId,
    path: NonRootMPath,
    file_unode_id: FileUnodeId,
    filesize_limit: u64,
    blame_key_prefix: &str,
) -> Result<BlameV2Id, Error> {
    let file_unode = file_unode_id.load(ctx, blobstore).await?;

    let mut blame_parents = Vec::new();
    for (parent_index, &unode_id) in file_unode.parents().iter().enumerate() {
        blame_parents.push(fetch_blame_parent(
            ctx,
            derivation_ctx,
            blobstore,
            BlameParentSource::ChangesetParent {
                parent_index,
                unode_id,
            },
            path.clone(),
            filesize_limit,
            blame_key_prefix,
        ));
    }
    if let Some(source) = renames.get(&path) {
        // If the file was copied from another path, then we ignore its
        // contents in the parents, even if it existed there, and just use the
        // copy-from source as a parent.  This matches the Mercurial blame
        // implementation.
        match source {
            UnodeRenameSource::CopyInfo(source) => {
                blame_parents.clear();
                blame_parents.push(fetch_blame_parent(
                    ctx,
                    derivation_ctx,
                    blobstore,
                    BlameParentSource::ChangesetParent {
                        parent_index: source.parent_index,
                        unode_id: source.unode_id,
                    },
                    source.from_path.clone(),
                    filesize_limit,
                    blame_key_prefix,
                ));
            }
            UnodeRenameSource::SubtreeCopy(copy) => {
                blame_parents.clear();
                blame_parents.push(fetch_blame_parent(
                    ctx,
                    derivation_ctx,
                    blobstore,
                    BlameParentSource::ReplacementParent(copy.parent),
                    copy.from_path
                        .into_optional_non_root_path()
                        .ok_or_else(|| anyhow!("Copy source must be a file"))?,
                    filesize_limit,
                    blame_key_prefix,
                ));
            }
            UnodeRenameSource::SubtreeMerge(merge) => {
                // Merges do merge with the original branch, so leave those parents intact.
                blame_parents.push(fetch_blame_parent(
                    ctx,
                    derivation_ctx,
                    blobstore,
                    BlameParentSource::ReplacementParent(merge.parent),
                    merge
                        .from_path
                        .into_optional_non_root_path()
                        .ok_or_else(|| anyhow!("Merge source must be a file"))?,
                    filesize_limit,
                    blame_key_prefix,
                ));
            }
        }
    }

    let (content, blame_parents) = future::try_join(
        fetch_content_for_blame_with_limit(ctx, blobstore, file_unode_id, filesize_limit),
        future::try_join_all(blame_parents),
    )
    .await?;

    let blame_parents = blame_parents.into_iter().flatten().collect();

    let blame = match content {
        FetchOutcome::Rejected(rejected) => BlameV2::rejected(rejected),
        FetchOutcome::Fetched(content) => BlameV2::new(csid, path, content, blame_parents)?,
    };

    store_blame_with_prefix(ctx, &blobstore, file_unode_id, blame, blame_key_prefix).await
}

enum BlameParentSource {
    /// The source of this blame parent is a real parent of the changeset.
    ChangesetParent {
        parent_index: usize,
        unode_id: FileUnodeId,
    },
    /// The source of this blame parent is a replacement parent (e.g. due
    /// to a mutable rename or subtree operation).
    ReplacementParent(ChangesetId),
}

/// Fetch a blame parent.  Result may be None in the case of a subtree
/// copy/merge where the specific file was actually added.
async fn fetch_blame_parent(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    parent_info: BlameParentSource,
    path: NonRootMPath,
    filesize_limit: u64,
    blame_key_prefix: &str,
) -> Result<Option<BlameParent<Bytes>>, Error> {
    let (parent, unode_id) = match parent_info {
        BlameParentSource::ChangesetParent {
            parent_index,
            unode_id,
        } => (BlameParentId::ChangesetParent(parent_index), unode_id),
        BlameParentSource::ReplacementParent(csid) => {
            let root = if blame_key_prefix.is_empty() {
                *derivation_ctx
                    .fetch_dependency::<RootUnodeManifestId>(ctx, csid)
                    .await?
                    .manifest_unode_id()
            } else {
                fetch_pipeline_unode_root(ctx, derivation_ctx, csid).await?
            };
            let entry = root
                .find_entry(ctx.clone(), blobstore.clone(), path.clone().into())
                .await?;
            let leaf = match entry {
                Some(entry) => entry.into_leaf(),
                None => return Ok(None),
            };
            let unode_id = match leaf {
                Some(leaf) => leaf,
                None => return Ok(None),
            };
            (BlameParentId::ReplacementParent(csid), unode_id)
        }
    };

    let (content, blame) = future::try_join(
        fetch_content_for_blame_with_limit(ctx, blobstore, unode_id, filesize_limit),
        load_blame_with_prefix(ctx, blobstore, unode_id, blame_key_prefix),
    )
    .await?;

    // Fall back to the canonical key for ancestors derived without the pipeline
    // prefix, mirroring `create_new_batch_with_prefix`.
    let blame = match blame {
        Some(blame) => Some(blame),
        None if !blame_key_prefix.is_empty() => {
            load_blame_with_prefix(ctx, blobstore, unode_id, "").await?
        }
        None => None,
    };

    // Parent blame must exist (canonical and namespaced parents are derived
    // before children); a missing blob is an invariant violation.
    let blame = blame.ok_or_else(|| {
        anyhow!(
            "blame parent missing for {parent}:{path} unode {unode_id} (prefix {blame_key_prefix:?})",
            parent = csid_for_error(&parent),
        )
    })?;

    Ok(Some(BlameParent::new(
        parent,
        path,
        content.into_bytes().ok(),
        blame,
    )))
}

fn csid_for_error(parent: &BlameParentId<ChangesetId>) -> String {
    match parent {
        BlameParentId::ChangesetParent(index) => format!("parent#{index}"),
        BlameParentId::ReplacementParent(csid) => csid.to_string(),
    }
}
