/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Error;
use anyhow::Result;
use anyhow::ensure;
use blobstore::Loadable;
use cloned::cloned;
use context::CoreContext;
use edenapi_types::AnyFileContentId;
use edenapi_types::AnyId;
use edenapi_types::BonsaiFileChange;
use edenapi_types::Extra;
use edenapi_types::FileContentTokenMetadata;
use edenapi_types::HgFilenodeData;
use edenapi_types::IdenticalChangesetContent;
use edenapi_types::Parents;
use edenapi_types::RepoPathBuf;
use edenapi_types::UploadToken;
use edenapi_types::UploadTokenMetadata;
use edenapi_types::UploadTreeEntry;
use edenapi_types::commit::BonsaiExtra;
use edenapi_types::commit::BonsaiParents;
use edenapi_types::commit::GitExtraHeader;
use edenapi_types::commit::HgInfo;
use edenapi_types::commit::SubtreeChange as EdenapiSubtreeChange;
use futures::TryStreamExt;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::HgNodeHash;
use mercurial_types::blobs::HgBlobChangeset;
use mercurial_types::blobs::hg_manifests_read_from_augmented;
use mercurial_types::fetch_manifest_envelope;
use mercurial_types::preloaded_augmented_manifest::serialize_manifest;
use mononoke_types::BonsaiChangeset;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::ChangesetId;
use mononoke_types::FileChange;
use mononoke_types::NonRootMPath;
use mononoke_types::SubtreeChange;
use repo_blobstore::RepoBlobstore;
use slapi_service::utils::to_hg_path;
use slapi_service::utils::to_hg_path_nonroot;
use sorted_vector_map::SortedVectorMap;

pub async fn from_tree_to_entry(
    id: HgManifestId,
    ctx: &CoreContext,
    repo_blobstore: &RepoBlobstore,
    repo_name: &str,
) -> Result<UploadTreeEntry> {
    if hg_manifests_read_from_augmented(repo_name) {
        return from_augmented_tree_to_entry(id, ctx, repo_blobstore).await;
    }
    let envelope = fetch_manifest_envelope(ctx, repo_blobstore, id).await?;
    let (p1, p2) = envelope.parents();
    Ok(UploadTreeEntry {
        node_id: envelope.node_id().into(),
        data: envelope.contents().to_vec(),
        parents: to_parents(p1, p2),
        computed_node_id: Some(envelope.computed_node_id().into()),
    })
}

/// The upload entry of the Hg tree `id`, rebuilt from the augmented manifest
/// stored under the same id.
async fn from_augmented_tree_to_entry(
    id: HgManifestId,
    ctx: &CoreContext,
    repo_blobstore: &RepoBlobstore,
) -> Result<UploadTreeEntry> {
    let manifest = HgAugmentedManifestId::from(id)
        .load(ctx, repo_blobstore)
        .await?
        .augmented_manifest;
    let (node_id, parents, computed_node_id) = (
        manifest.hg_node_id,
        to_parents(manifest.p1, manifest.p2),
        manifest.computed_node_id,
    );
    let entries = manifest
        .into_subentries(ctx, repo_blobstore)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(UploadTreeEntry {
        node_id: node_id.into(),
        data: serialize_manifest(&entries)?.to_vec(),
        parents,
        computed_node_id: Some(computed_node_id.into()),
    })
}

fn to_parents(p1: Option<HgNodeHash>, p2: Option<HgNodeHash>) -> Parents {
    match (p1, p2) {
        (None, None) => Parents::None,
        (Some(p1), None) => Parents::One(p1.into()),
        (None, Some(p2)) => Parents::One(p2.into()),
        (Some(p1), Some(p2)) => Parents::Two(p1.into(), p2.into()),
    }
}

pub async fn from_id_to_filenode(
    file_id: HgFileNodeId,
    ctx: &CoreContext,
    repo_blobstore: &RepoBlobstore,
) -> Result<HgFilenodeData> {
    let file_node = file_id.load(ctx, repo_blobstore).await?;

    // These tokens are mostly implemented to make sure client sends content before uploading filenodes
    // but they're not really verified, given we're indeed sending the content, let's use a placeholder
    let content_id = file_node.content_id();
    let token = UploadToken::new_fake_token(
        AnyId::AnyFileContentId(AnyFileContentId::ContentId(content_id.into())),
        None,
    );

    Ok(HgFilenodeData {
        node_id: file_id.into_nodehash().into(),
        parents: file_node.hg_parents().into(),
        metadata: file_node.metadata().clone().to_vec(),
        file_content_upload_token: token,
    })
}

pub fn to_identical_changeset(
    css: (HgBlobChangeset, BonsaiChangeset),
) -> Result<IdenticalChangesetContent> {
    let (hg_cs, bcs) = css;
    let BonsaiChangesetMut {
        parents,
        author,
        author_date,
        committer,
        committer_date,
        message,
        hg_extra: _,
        git_extra_headers,
        file_changes,
        is_snapshot,
        git_tree_hash,
        git_annotated_tag,
        subtree_changes,
    } = bcs.clone().into_mut();

    let hg_info = HgInfo {
        node_id: hg_cs.get_changeset_id().into_nodehash().into(),
        manifestid: hg_cs.manifestid().into_nodehash().into(),
        extras: hg_cs
            .extra()
            .iter()
            .map(|(key, value)| Extra {
                key: key.to_vec(),
                value: value.to_vec(),
            })
            .collect(),
    };

    let mut git_extra = None;
    if let Some(headers) = git_extra_headers {
        let mut all_headers = vec![];
        for (key, value) in headers {
            all_headers.push(GitExtraHeader {
                key: key.to_vec(),
                value: value.to_vec(),
            });
        }
        git_extra = Some(all_headers);
    }

    let bonsai_parents = BonsaiParents::from_iter(parents.clone().iter().map(|p| (*p).into()));

    // Ensure items are indeed equivalent between bonsai and hg changeset
    ensure!(author.as_bytes() == hg_cs.user(), "Author mismatch");
    ensure!(author_date == *hg_cs.time(), "Time mismatch");
    ensure!(message.as_bytes() == hg_cs.message(), "Message mismatch");
    ensure!(git_tree_hash.is_none(), "Unexpected git tree hash found");
    ensure!(
        git_annotated_tag.is_none(),
        "Unexpected git annotated tag found"
    );

    // Convert subtree changes.
    let converted_subtree_changes = if subtree_changes.is_empty() {
        None
    } else {
        let mut changes = Vec::new();
        for (path, change) in subtree_changes {
            let repo_path = to_hg_path(&path)?;
            let edenapi_change = match change {
                SubtreeChange::SubtreeDeepCopy(deep_copy) => EdenapiSubtreeChange::DeepCopy {
                    from_path: to_hg_path(&deep_copy.from_path)?,
                    from_cs_id: deep_copy.from_cs_id.into(),
                },
                SubtreeChange::SubtreeMerge(merge) => EdenapiSubtreeChange::Merge {
                    from_path: to_hg_path(&merge.from_path)?,
                    from_cs_id: merge.from_cs_id.into(),
                },
                SubtreeChange::SubtreeImport(import) => EdenapiSubtreeChange::Import {
                    from_path: to_hg_path(&import.from_path)?,
                    from_commit: import.from_commit,
                    from_repo_url: import.from_repo_url,
                },
                SubtreeChange::SubtreeCrossRepoMerge(cross_repo_merge) => {
                    EdenapiSubtreeChange::CrossRepoMerge {
                        from_path: to_hg_path(&cross_repo_merge.from_path)?,
                        from_commit: cross_repo_merge.from_commit,
                        from_repo_url: cross_repo_merge.from_repo_url,
                    }
                }
            };
            changes.push((repo_path, edenapi_change));
        }
        Some(changes)
    };

    Ok(IdenticalChangesetContent {
        bcs_id: bcs.get_changeset_id().into(),
        hg_parents: hg_cs.parents().into(),
        bonsai_parents,
        author: author.to_string(),
        time: author_date.timestamp_secs(),
        tz: author_date.tz_offset_secs(),
        extras: bcs
            .hg_extra()
            .map(|(key, value)| BonsaiExtra {
                key: key.to_string(),
                value: value.to_vec(),
            })
            .collect(),
        bonsai_file_changes: to_file_change(&file_changes, parents.iter().copied())?,
        hg_file_changes: hg_cs
            .files()
            .iter()
            .map(to_hg_path_nonroot)
            .collect::<Result<Vec<RepoPathBuf>>>()?,
        message: message.to_string(),
        is_snapshot,
        hg_info,
        committer,
        committer_time: committer_date.map(|d| d.timestamp_secs()),
        committer_tz: committer_date.map(|d| d.tz_offset_secs()),
        git_extra_headers: git_extra,
        subtree_changes: converted_subtree_changes,
    })
}

fn to_file_change(
    bonsai_changes: &SortedVectorMap<NonRootMPath, FileChange>,
    parents: impl Iterator<Item = ChangesetId> + Clone,
) -> Result<Vec<(RepoPathBuf, BonsaiFileChange)>> {
    let res = bonsai_changes
        .into_iter()
        .map(|(path, fc)| {
            let repo_path = RepoPathBuf::from_string(path.clone().to_string())?;
            let bs_fc = match fc {
                FileChange::Deletion => BonsaiFileChange::Deletion,
                FileChange::UntrackedDeletion => BonsaiFileChange::UntrackedDeletion,
                FileChange::Change(tc) => {
                    let size = tc.size();
                    let token_metadata = FileContentTokenMetadata { content_size: size };
                    BonsaiFileChange::Change {
                        upload_token: UploadToken::new_fake_token_with_metadata(
                            AnyId::AnyFileContentId(AnyFileContentId::ContentId(
                                tc.content_id().into(),
                            )),
                            None,
                            UploadTokenMetadata::FileContentTokenMetadata(token_metadata),
                        ),
                        file_type: tc.file_type().try_into()?,
                        copy_info: match tc.copy_from() {
                            Some((path, cs_id)) => {
                                cloned!(mut parents);
                                let index = parents.position(|parent| parent == *cs_id).ok_or(
                                    anyhow::anyhow!("Copy from info doesn't match parents"),
                                )?;
                                Some((to_hg_path_nonroot(path)?, index))
                            }
                            None => None,
                        },
                    }
                }
                FileChange::UntrackedChange(uc) => BonsaiFileChange::UntrackedChange {
                    upload_token: UploadToken::new_fake_token(
                        AnyId::AnyFileContentId(AnyFileContentId::ContentId(
                            uc.content_id().into(),
                        )),
                        None,
                    ),
                    file_type: uc.file_type().try_into()?,
                },
            };
            Ok((repo_path, bs_fc))
        })
        .collect::<Result<Vec<(RepoPathBuf, BonsaiFileChange)>, Error>>()?;
    Ok(res)
}
