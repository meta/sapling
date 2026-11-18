/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Error;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use async_trait::async_trait;
use blobrepo_common::changed_files::compute_changed_files;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use borrowed::borrowed;
use bytes::Bytes;
use cloned::cloned;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use derived_data_manager::BonsaiDerivable;
use derived_data_manager::ManifestStageOutput;
use derived_data_manager::StageId;
use derived_data_manager::manifest_partitions::create_sorted_subentries;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future::try_join;
use futures::future::try_join_all;
use futures::stream;
use manifest::Entry;
use manifest::LeafInfo;
use manifest::ManifestChanges;
use manifest::Traced;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::HgParents;
use mercurial_types::blobs::ChangesetMetadata;
use mercurial_types::blobs::ContentBlobMeta;
use mercurial_types::blobs::File;
use mercurial_types::blobs::HgBlobChangeset;
use mercurial_types::blobs::HgChangesetContent;
use mercurial_types::blobs::UploadHgFileContents;
use mercurial_types::blobs::UploadHgFileEntry;
use mercurial_types::blobs::UploadHgNodeHash;
use mercurial_types::subtree::HgSubtreeChanges;
use mononoke_macros::mononoke;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::FileChange;
use mononoke_types::FileType;
use mononoke_types::NonRootMPath;
use mononoke_types::TrackedFileChange;
use mononoke_types::path::MPath;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataRef;
use restricted_paths_common::ArcRestrictedPathsConfigBased;
use stats::prelude::*;

use crate::derive_hg_manifest::HgManifestInput;
use crate::derive_hg_manifest::HgManifestOutput;
use crate::derive_hg_manifest::ParentIndex;
use crate::derive_hg_manifest::create_hg_file;
use crate::derive_hg_manifest::create_hg_manifest;
use crate::derive_hg_manifest::derive_simple_hg_manifest_stack_without_copy_info;
use crate::derive_hg_manifest::prepare_manifest_input;
use crate::mapping::HgChangesetDeriveOptions;
use crate::mapping::MappedHgChangesetId;
use crate::mapping::RootHgAugmentedManifestId;

define_stats! {
    prefix = "mononoke.blobrepo";
    get_hg_from_bonsai_changeset: timeseries(Rate, Sum),
    generate_hg_from_bonsai_changeset: timeseries(Rate, Sum),
    generate_hg_from_bonsai_total_latency_ms: histogram(100, 0, 10_000, Average; P 50; P 75; P 90; P 95; P 99),
    generate_hg_from_bonsai_single_latency_ms: histogram(100, 0, 10_000, Average; P 50; P 75; P 90; P 95; P 99),
    generate_hg_from_bonsai_generated_commit_num: histogram(1, 0, 20, Average; P 50; P 75; P 90; P 95; P 99),
}

async fn can_reuse_filenode(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    parent: HgFileNodeId,
    change: &TrackedFileChange,
) -> Result<Option<(HgFileNodeId, Bytes)>, Error> {
    let parent_envelope = parent.load(ctx, blobstore).await?;
    let parent_copyfrom_path = File::extract_copied_from(parent_envelope.metadata())?.map(|t| t.0);
    let parent_content_id = parent_envelope.content_id();

    if parent_content_id == change.content_id()
        && change.copy_from().map(|t| &t.0) == parent_copyfrom_path.as_ref()
    {
        // Return the parent's metadata along with the filenode so callers
        // don't need to re-load the envelope just to read it.
        Ok(Some((parent, parent_envelope.metadata().clone())))
    } else {
        Ok(None)
    }
}

pub(crate) async fn store_file_change<'a>(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    p1: Option<HgFileNodeId>,
    p2: Option<HgFileNodeId>,
    path: &'a NonRootMPath,
    change: &'a TrackedFileChange,
    copy_from: Option<(NonRootMPath, HgFileNodeId)>,
) -> Result<(FileType, HgFileNodeId, Bytes), Error> {
    // If we produced a hg change that has copy info, then the Bonsai should have copy info
    // too. However, we could have Bonsai copy info without having copy info in the hg change
    // if we stripped it out to produce a hg changeset for an Octopus merge and the copy info
    // references a step-parent (i.e. neither p1, not p2).
    if copy_from.is_some() {
        assert!(change.copy_from().is_some());
    }

    // Mercurial has complicated logic of finding file parents, especially
    // if a file was also copied/moved.
    // See mercurial/localrepo.py:_filecommit().
    // Mononoke uses simpler rules, which still produce a usable result

    // Simplify parents. This aims to reduce the amount of work done in reuse, and avoids
    // semantically unhelpful duplicate parents.
    let (p1, p2) = match (p1, p2) {
        (Some(p1), None) => (Some(p1), None),
        (None, Some(p2)) => (Some(p2), None),
        (p1, p2) if p1 == p2 => (p1, None),
        (p1, p2) => (p1, p2),
    };

    // we can reuse same HgFileNodeId if we have a filenode that has
    // same content and copyfrom information
    let maybe_filenode = match (p1, p2) {
        (Some(parent), None) | (None, Some(parent)) => {
            can_reuse_filenode(&ctx, &blobstore, parent, change).await?
        }
        (Some(p1), Some(p2)) => {
            let (reuse_p1, reuse_p2) = try_join(
                can_reuse_filenode(&ctx, &blobstore, p1, change),
                can_reuse_filenode(&ctx, &blobstore, p2, change),
            )
            .await?;
            reuse_p1.or(reuse_p2)
        }
        // No filenode to reuse
        (None, None) => None,
    };

    let (filenode_id, metadata) = match maybe_filenode {
        Some((filenode, metadata)) => (filenode, metadata),
        None => {
            let p1 = if p1.is_some()
                && p2.is_none()
                && copy_from.is_some()
                && copy_from.as_ref().map(|c| &c.0) != Some(path)
            {
                // Mercurial special-cases the "file copied over existing file" case, and does not
                // put in a parent in that situation - `hg log` then looks down the copyfrom
                // information instead. This is not the best decision, but we should keep it
                // For example:
                // ```
                // echo 1 > 1 && echo 2 > 2 && hg ci -A -m first
                // hg cp 2 1 --force && hg ci -m second
                // # File '1' has both p1 and copy from.
                // ```
                // In this case, Mercurial and Mononoke both drop the p1 information in the filenode,
                // instead relying on the copyfrom for history.
                None
            } else {
                p1
            };

            let upload_entry = UploadHgFileEntry {
                upload_node_id: UploadHgNodeHash::Generate,
                contents: UploadHgFileContents::ContentUploaded(ContentBlobMeta {
                    id: change.content_id(),
                    size: change.size(),
                    copy_from: copy_from.clone(),
                }),
                p1,
                p2,
            };

            // UploadHgFileEntry::upload doesn't expose the metadata it computes,
            // so we re-load the envelope to extract it. Eliminating this read
            // would require changing the public upload() return type and it's
            // probably not worth it, since it will be in the cache anyway.
            let filenode_id = upload_entry
                .upload(ctx.clone(), blobstore.clone(), Some(path))
                .await?;
            let envelope = filenode_id.load(&ctx, &blobstore).await?;
            (filenode_id, envelope.metadata().clone())
        }
    };

    Ok((change.file_type(), filenode_id, metadata))
}

/// Root-stage wrapper around `get_manifest_entry_from_bonsai` that returns the
/// root `HgManifestId` directly. Used by the non-pipelined derivation path.
pub async fn get_manifest_from_bonsai(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    restricted_paths: ArcRestrictedPathsConfigBased,
    bcs: BonsaiChangeset,
    parent_manifests: Vec<HgManifestId>,
) -> Result<HgManifestId, Error> {
    let parents = parent_manifests
        .into_iter()
        .map(|id| ManifestStageOutput::Entry(Some(Entry::Tree(Traced::generate(id)))))
        .collect();
    let input = prepare_manifest_input(&StageId::Manifest(MPath::ROOT), parents, HashMap::new())?;
    let entry =
        get_manifest_entry_from_bonsai(ctx, blobstore, restricted_paths, bcs, input, |_| async {
            Ok(None)
        })
        .await?
        .into_entry()?;
    // At the root stage, the widened primitive always returns Some(Entry::Tree(_))
    // (synthesizing an empty root manifest when all files are deleted).
    entry
        .and_then(|e| e.into_tree())
        .ok_or_else(|| anyhow!("root manifest derivation produced no tree entry"))
        .map(|t| t.into_untraced())
}

/// Derive the stage using its ordered parent outputs and a lazy parent-root loader.
pub async fn get_manifest_entry_from_bonsai<F, Fut>(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    restricted_paths: ArcRestrictedPathsConfigBased,
    bcs: BonsaiChangeset,
    input: HgManifestInput,
    load_parent_root: F,
) -> Result<HgManifestOutput, Error>
where
    F: Fn(ChangesetId) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Option<Traced<ParentIndex, HgManifestId>>, Error>> + Send,
{
    let stage = input.stage_id();
    let stage_path = input.path().clone();
    let parent_outputs = input.parent_outputs();
    // Mercurial tracks file parents and copies only through p1 and p2.
    let (p1, p2) = {
        let mut parents = bcs.parents();
        let p1 = parents.next();
        let p2 = parents.next();
        (p1, p2)
    };

    // Ancestor additions must reach derive_manifest to trigger implicit deletion.
    let file_changes = bcs
        .file_changes()
        .filter(|(path, change)| {
            stage.covers(path) || (change.is_changed() && path.is_prefix_of(&stage_path))
        })
        .map(|(path, fc)| {
            Ok((
                path.clone(),
                match fc {
                    FileChange::Change(tc) => Some(tc.clone()),
                    FileChange::Deletion => None,
                    FileChange::UntrackedChange(_) | FileChange::UntrackedDeletion => {
                        bail!("Can't derive manifest for snapshot")
                    }
                },
            ))
        })
        .collect::<Result<Vec<_>, Error>>()?;

    let mut p1_paths = Vec::new();
    let mut p2_paths = Vec::new();
    for (path, file_change) in &file_changes {
        if !input.contains_change(path) {
            continue;
        }
        let Some(file_change) = file_change else {
            continue;
        };
        if let Some((copy_path, bcsid)) = file_change.copy_from() {
            if Some(bcsid) == p1.as_ref() {
                p1_paths.push(copy_path.clone());
            }
            if Some(bcsid) == p2.as_ref() {
                p2_paths.push(copy_path.clone());
            }
        }
        p1_paths.push(path.clone());
        p2_paths.push(path.clone());
    }
    let resolve_parent = |index: usize, paths: Vec<NonRootMPath>| {
        let parent_output = parent_outputs.get(index).cloned();
        let parent_csid = [p1, p2][index];
        let ctx = ctx.clone();
        let blobstore = blobstore.clone();
        let stage = &stage;
        let load_parent_root = &load_parent_root;
        async move {
            let Some(csid) = parent_csid else {
                return Ok(HashMap::new());
            };
            let parent_output = parent_output
                .ok_or_else(|| anyhow!("missing Hg stage output for parent {csid}"))?;
            Ok::<HashMap<_, _>, Error>(
                parent_output
                    .find_parent_entries(ctx, blobstore, stage, paths, || load_parent_root(csid))
                    .await?
                    .into_iter()
                    .filter_map(|(path, entry)| {
                        entry.into_leaf().map(|file| (path, file.into_untraced().1))
                    })
                    .collect(),
            )
        }
    };
    let (p1s, p2s) = try_join(resolve_parent(0, p1_paths), resolve_parent(1, p2_paths)).await?;

    let files_to_store = file_changes
        .iter()
        .filter_map(|(path, change)| {
            input
                .contains_change(path)
                .then_some((path.clone(), change.as_ref()?.clone()))
        })
        .collect::<Vec<_>>();
    let p1s = Arc::new(p1s);
    let p2s = Arc::new(p2s);
    let files = stream::iter(files_to_store)
        .map(|(path, file_change)| {
            cloned!(ctx, blobstore, p1s, p2s);
            let task = mononoke::spawn_task(async move {
                let copy_from = file_change.copy_from().and_then(|(copy_path, bcsid)| {
                    let parent = if Some(bcsid) == p1.as_ref() {
                        &p1s
                    } else if Some(bcsid) == p2.as_ref() {
                        &p2s
                    } else {
                        return None;
                    };
                    parent.get(copy_path).map(|id| (copy_path.clone(), *id))
                });
                let (file_type, filenode_id, _) = store_file_change(
                    ctx,
                    blobstore,
                    p1s.get(&path).copied(),
                    p2s.get(&path).copied(),
                    &path,
                    &file_change,
                    copy_from,
                )
                .await?;
                Ok::<_, Error>((path, (file_type, filenode_id)))
            });
            async move { task.await? }
        })
        .buffer_unordered(1000)
        .try_collect::<HashMap<_, _>>()
        .await?;
    let files = Arc::new(files);
    input
        .derive(
            ctx.clone(),
            blobstore.clone(),
            file_changes,
            {
                cloned!(ctx, blobstore, restricted_paths);
                move |info| {
                    create_hg_manifest(
                        ctx.clone(),
                        blobstore.clone(),
                        info,
                        restricted_paths.clone(),
                    )
                }
            },
            {
                cloned!(ctx, blobstore);
                move |LeafInfo {
                          path,
                          change,
                          parents,
                      }| {
                    cloned!(ctx, blobstore, files);
                    async move {
                        if change.is_some() {
                            let entry = files.get(&path).copied().ok_or_else(|| {
                                anyhow!("missing prepared Mercurial file at {path}")
                            })?;
                            Ok(((), Traced::generate(entry)))
                        } else {
                            create_hg_file(
                                ctx,
                                blobstore,
                                LeafInfo {
                                    path,
                                    change: None,
                                    parents,
                                },
                            )
                            .await
                        }
                    }
                }
            },
            |subentries| create_sorted_subentries(&ctx, subentries),
        )
        .await
}

pub(crate) async fn derive_from_parents(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    bonsai: BonsaiChangeset,
    parents: Vec<MappedHgChangesetId>,
    subtree_change_sources: HashMap<ChangesetId, HgChangesetId>,
    options: &HgChangesetDeriveOptions,
    restricted_paths: ArcRestrictedPathsConfigBased,
) -> Result<(MappedHgChangesetId, HgManifestId), Error> {
    let parents = {
        borrowed!(ctx);
        try_join_all(
            parents
                .into_iter()
                .map(|id| async move { id.hg_changeset_id().load(ctx, blobstore).await }),
        )
        .await?
    };

    let subtree_changes = HgSubtreeChanges::from_bonsai_subtree_changes(
        bonsai.subtree_changes(),
        subtree_change_sources,
    )?;

    let parent_manifests = parents.iter().map(|p| p.manifestid()).collect();
    let manifest_id = get_manifest_from_bonsai(
        ctx.clone(),
        blobstore.clone(),
        restricted_paths,
        bonsai.clone(),
        parent_manifests,
    )
    .await?;

    let parent_hg_cs_ids: Vec<HgChangesetId> =
        parents.iter().map(|p| p.get_changeset_id()).collect();
    let files = compute_changed_files(
        ctx.clone(),
        blobstore.clone(),
        manifest_id,
        parents.first().map(|p| p.manifestid()),
        parents.get(1).map(|p| p.manifestid()),
    )
    .await?;
    let (hg_cs_id, _) = generate_hg_changeset(
        ctx,
        blobstore,
        bonsai,
        manifest_id,
        parent_hg_cs_ids,
        files,
        subtree_changes,
        options,
    )
    .await?;
    Ok((MappedHgChangesetId::new(hg_cs_id), manifest_id))
}

pub async fn derive_simple_hg_changeset_stack_without_copy_info(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    bonsais: Vec<BonsaiChangeset>,
    parent: Option<MappedHgChangesetId>,
    options: &HgChangesetDeriveOptions,
    restricted_paths: ArcRestrictedPathsConfigBased,
) -> Result<HashMap<ChangesetId, MappedHgChangesetId>, Error> {
    let parent = match parent {
        Some(parent) => Some(parent.hg_changeset_id().load(ctx, blobstore).await?),
        None => None,
    };
    let file_changes = bonsais
        .iter()
        .map(|bonsai| {
            ensure!(
                !bonsai.has_subtree_changes(),
                "simple derivation doesn't support subtree changes"
            );
            let per_commit_file_changes: Result<Vec<_>, Error> = bonsai
                .file_changes()
                .map(|(path, fc)| {
                    use FileChange::*;
                    let tracked_file_change = match fc {
                        Change(tracked_file_change) => Some(tracked_file_change.clone()),
                        Deletion => None,
                        UntrackedChange(_) | UntrackedDeletion => {
                            bail!(
                                "unexpected untracked file change while deriving {}",
                                bonsai.get_changeset_id()
                            );
                        }
                    };
                    Ok((path.clone(), tracked_file_change))
                })
                .collect();
            let per_commit_file_changes = per_commit_file_changes?;

            let mf_changes = ManifestChanges {
                cs_id: bonsai.get_changeset_id(),
                changes: per_commit_file_changes,
            };

            Ok(mf_changes)
        })
        .collect::<Result<Vec<_>, Error>>();
    let file_changes = file_changes?;

    let mf_ids = derive_simple_hg_manifest_stack_without_copy_info(
        ctx.clone(),
        blobstore.clone(),
        file_changes,
        parent.clone().map(|p| p.manifestid()),
        restricted_paths,
    )
    .await?;
    let mut parents = parent.into_iter().collect::<Vec<_>>();

    let mut res = HashMap::with_capacity(bonsais.len());
    for bonsai in bonsais {
        let cs_id = bonsai.get_changeset_id();
        let mf_id = mf_ids.get(&cs_id).ok_or_else(|| {
            anyhow!("not found manifest for {cs_id} but should have derived it in this function")
        })?;
        let parent_hg_cs_ids: Vec<HgChangesetId> =
            parents.iter().map(|p| p.get_changeset_id()).collect();
        let files = compute_changed_files(
            ctx.clone(),
            blobstore.clone(),
            *mf_id,
            parents.first().map(|p| p.manifestid()),
            parents.get(1).map(|p| p.manifestid()),
        )
        .await?;
        let (hg_changeset_id, hg_cs) = generate_hg_changeset(
            ctx,
            blobstore,
            bonsai,
            *mf_id,
            parent_hg_cs_ids,
            files,
            None,
            options,
        )
        .await?;
        res.insert(cs_id, MappedHgChangesetId::new(hg_changeset_id));
        parents = vec![hg_cs];
    }

    Ok(res)
}

pub(crate) async fn generate_hg_changeset(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    bcs: BonsaiChangeset,
    manifest_id: HgManifestId,
    parents: Vec<HgChangesetId>,
    files: Vec<NonRootMPath>,
    subtree_changes: Option<HgSubtreeChanges>,
    options: &HgChangesetDeriveOptions,
) -> Result<(HgChangesetId, HgBlobChangeset), Error> {
    let start_timestamp = Instant::now();

    // NOTE: We're special-casing the first 2 parents here, since that's all Mercurial
    // supports. Producing the Manifest (in get_manifest_from_bonsai) will consider all
    // parents, but everything else is only presented with the first 2 parents, because that's
    // all Mercurial knows about for now. This lets us produce a meaningful Hg changeset for a
    // Bonsai changeset with > 2 parents (which might be one we imported from Git).
    let mut parents = parents.into_iter();
    let p1 = parents.next();
    let p2 = parents.next();

    let hg_parents = HgParents::new(p1.map(|h| h.into_nodehash()), p2.map(|h| h.into_nodehash()));

    // Keep a record of any parents for now (i.e. > 2 parents). We'll store those in extras.
    let step_parents = parents;

    let mut metadata = ChangesetMetadata {
        user: bcs.author().to_string(),
        time: *bcs.author_date(),
        extra: bcs
            .hg_extra()
            .map(|(k, v)| {
                (
                    Bytes::copy_from_slice(k.as_bytes()),
                    Bytes::copy_from_slice(v),
                )
            })
            .collect(),
        message: bcs.message().to_string(),
    };
    metadata.record_step_parents(step_parents);
    if options.set_committer_field {
        match (bcs.committer(), bcs.committer_date()) {
            (Some(committer), Some(date)) => {
                // Do not record committer if it's the same as author
                if committer != bcs.author() || date != bcs.author_date() {
                    metadata.record_committer(committer, date)?;
                }
            }
            (None, None) => {}
            _ => {
                bail!("invalid committer/committer date in bonsai changeset");
            }
        };
    }
    if let Some(subtree_changes) = subtree_changes {
        metadata.record_subtree_changes(subtree_changes)?;
    }

    let content = HgChangesetContent::new_from_parts(hg_parents, manifest_id, metadata, files);
    let cs = HgBlobChangeset::new(content)?;
    let csid = cs.get_changeset_id();

    cs.save(ctx, blobstore).await?;

    STATS::generate_hg_from_bonsai_single_latency_ms
        .add_value(start_timestamp.elapsed().as_millis() as i64);
    STATS::generate_hg_from_bonsai_generated_commit_num.add_value(1);

    Ok((csid, cs))
}

#[async_trait]
pub trait DeriveHgChangeset {
    async fn derive_hg_changeset(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<HgChangesetId, Error>;
}

/// Best-effort derive the augmented manifest alongside the hg changeset.
/// Skips bubbles (their manager lacks the deps) and swallows errors
/// so it never regresses hg-changeset derivation.
pub async fn derive_hg_augmented_manifest_at_creation(
    ctx: &CoreContext,
    derived_data: &RepoDerivedData,
    cs_id: ChangesetId,
) {
    if derived_data.manager().bubble_id().is_some()
        || !derived_data
            .config()
            .is_enabled(RootHgAugmentedManifestId::VARIANT)
        || !justknobs::eval(
            "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset",
            ctx.metadata()
                .client_request_info()
                .map(|c| c.correlator.as_str()),
            Some(derived_data.repo_name()),
        )
    {
        return;
    }
    if let Err(err) = derived_data
        .derive::<RootHgAugmentedManifestId>(ctx, cs_id, DerivationPriority::LOW)
        .await
    {
        ctx.scuba().clone().log_with_msg(
            "Augmented Hg manifest derivation alongside hg changeset failed",
            Some(format!("cs_id: {cs_id:?}, error: {err:?}")),
        );
    }
}

pub async fn derive_hg_changeset(
    ctx: &CoreContext,
    derived_data: &RepoDerivedData,
    cs_id: ChangesetId,
) -> Result<HgChangesetId, Error> {
    STATS::get_hg_from_bonsai_changeset.add_value(1);
    let start_timestamp = Instant::now();
    let result = match derived_data
        .derive::<MappedHgChangesetId>(ctx, cs_id, DerivationPriority::LOW)
        .await
    {
        Ok(id) => Ok(id.hg_changeset_id()),
        Err(err) => Err(err.into()),
    };
    // Record before the augmented derivation below so this stat excludes it.
    STATS::generate_hg_from_bonsai_total_latency_ms
        .add_value(start_timestamp.elapsed().as_millis() as i64);
    if result.is_ok() {
        derive_hg_augmented_manifest_at_creation(ctx, derived_data, cs_id).await;
    }
    result
}

#[async_trait]
impl<Repo: RepoDerivedDataRef + Send + Sync> DeriveHgChangeset for Repo {
    async fn derive_hg_changeset(
        &self,
        ctx: &CoreContext,
        cs_id: ChangesetId,
    ) -> Result<HgChangesetId, Error> {
        derive_hg_changeset(ctx, self.repo_derived_data(), cs_id).await
    }
}

#[cfg(test)]
mod pipeline_equivalence_tests {
    //! Byte-for-byte equivalence tests between `get_manifest_from_bonsai`
    //! (non-pipelined, root-only) and chained `get_manifest_entry_from_bonsai`
    //! calls (pipelined, per-subtree stages then assembled at root via
    //! `known_entries`). The mercurial filenode hash includes the parent
    //! filenodes by ancestry, so any divergence in copy-from handling,
    //! parent indexing, or sub-manifest derivation produces a different
    //! `HgManifestId` and fails the assertion.
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use manifest::Entry as ManifestEntry;
    use manifest::ManifestOps;
    use mononoke_macros::mononoke;
    use repo_blobstore::RepoBlobstore;
    use repo_blobstore::RepoBlobstoreRef;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;
    use tests_utils::CreateCommitContext;

    use super::*;
    use crate::DeriveHgChangeset;

    #[derive(Clone)]
    #[facet::container]
    struct TestRepo {
        #[facet]
        bonsai_hg_mapping: dyn BonsaiHgMapping,
        #[facet]
        bookmarks: dyn Bookmarks,
        #[facet]
        repo_blobstore: RepoBlobstore,
        #[facet]
        repo_derived_data: RepoDerivedData,
        #[facet]
        filestore_config: FilestoreConfig,
        #[facet]
        commit_graph: CommitGraph,
        #[facet]
        commit_graph_writer: dyn CommitGraphWriter,
        #[facet]
        repo_identity: RepoIdentity,
    }

    /// Look up the parent's entry at a path. Returns `None` if absent.
    async fn parent_subtree_at(
        ctx: &CoreContext,
        blobstore: &RepoBlobstore,
        parent_root: HgManifestId,
        path: MPath,
    ) -> Result<Option<ManifestEntry<HgManifestId, (FileType, HgFileNodeId)>>, Error> {
        parent_root
            .find_entry(ctx.clone(), blobstore.clone(), path)
            .await
    }

    /// Derive the non-pipelined root `HgManifestId` for `child_cs_id` given
    /// the parent's already-derived root manifest.
    async fn derive_root_non_pipelined(
        ctx: &CoreContext,
        repo: &TestRepo,
        child_cs_id: ChangesetId,
        parent_root_manifest: HgManifestId,
    ) -> Result<HgManifestId, Error> {
        let bcs = child_cs_id.load(ctx, repo.repo_blobstore()).await?;
        let restricted_paths = repo
            .repo_derived_data()
            .manager()
            .derivation_context(None)
            .restricted_paths();
        get_manifest_from_bonsai(
            ctx.clone(),
            std::sync::Arc::new(repo.repo_blobstore().clone()),
            restricted_paths,
            bcs,
            vec![parent_root_manifest],
        )
        .await
    }

    async fn stage_entry(
        ctx: &CoreContext,
        repo: &TestRepo,
        child_cs_id: ChangesetId,
        parent_entries: Vec<Option<ManifestEntry<HgManifestId, (FileType, HgFileNodeId)>>>,
        stage_path: MPath,
        known_entries: HashMap<
            MPath,
            Option<
                ManifestEntry<
                    Traced<ParentIndex, HgManifestId>,
                    Traced<ParentIndex, (FileType, HgFileNodeId)>,
                >,
            >,
        >,
    ) -> Result<
        Option<
            ManifestEntry<
                Traced<ParentIndex, HgManifestId>,
                Traced<ParentIndex, (FileType, HgFileNodeId)>,
            >,
        >,
        Error,
    > {
        let bcs = child_cs_id.load(ctx, repo.repo_blobstore()).await?;
        let restricted_paths = repo
            .repo_derived_data()
            .manager()
            .derivation_context(None)
            .restricted_paths();
        get_manifest_entry_from_bonsai(
            ctx.clone(),
            std::sync::Arc::new(repo.repo_blobstore().clone()),
            restricted_paths,
            bcs,
            prepare_manifest_input(
                &StageId::Manifest(stage_path),
                parent_entries
                    .into_iter()
                    .map(|entry| {
                        ManifestStageOutput::Entry(entry.map(crate::pipeline::wrap_entry_traced))
                    })
                    .collect(),
                known_entries
                    .into_iter()
                    .map(|(path, entry)| {
                        (StageId::Manifest(path), ManifestStageOutput::Entry(entry))
                    })
                    .collect(),
            )?,
            |_| async { Ok(None) },
        )
        .await?
        .into_entry()
    }

    /// Like `stage_entry`, but supplies the completed parent root for external copies.
    async fn stage_entry_with_cross_stage_sources(
        ctx: &CoreContext,
        repo: &TestRepo,
        child_cs_id: ChangesetId,
        parent_entries: Vec<Option<ManifestEntry<HgManifestId, (FileType, HgFileNodeId)>>>,
        stage_path: MPath,
        parent_root: HgManifestId,
    ) -> Result<
        Option<
            ManifestEntry<
                Traced<ParentIndex, HgManifestId>,
                Traced<ParentIndex, (FileType, HgFileNodeId)>,
            >,
        >,
        Error,
    > {
        let bcs = child_cs_id.load(ctx, repo.repo_blobstore()).await?;
        let restricted_paths = repo
            .repo_derived_data()
            .manager()
            .derivation_context(None)
            .restricted_paths();
        get_manifest_entry_from_bonsai(
            ctx.clone(),
            std::sync::Arc::new(repo.repo_blobstore().clone()),
            restricted_paths,
            bcs,
            prepare_manifest_input(
                &StageId::Manifest(stage_path),
                parent_entries
                    .into_iter()
                    .map(|entry| {
                        ManifestStageOutput::Entry(entry.map(crate::pipeline::wrap_entry_traced))
                    })
                    .collect(),
                HashMap::new(),
            )?,
            |_| async { Ok(Some(Traced::generate(parent_root))) },
        )
        .await?
        .into_entry()
    }

    /// Get the parent's already-derived root `HgManifestId` (deriving on
    /// demand via the standard path).
    async fn parent_root_manifest(
        ctx: &CoreContext,
        repo: &TestRepo,
        parent_cs_id: ChangesetId,
    ) -> Result<HgManifestId, Error> {
        let hg_cs_id = repo.derive_hg_changeset(ctx, parent_cs_id).await?;
        Ok(hg_cs_id
            .load(ctx, repo.repo_blobstore())
            .await?
            .manifestid())
    }

    /// Independent stages: derive `dir1/` and `dir2/` separately, then assemble
    /// at the root via `known_entries`. The root manifest id from the pipelined
    /// assembly must match the non-pipelined root derivation byte-for-byte.
    /// This is the primary regression test for the parent-entry-type bug (#1) —
    /// before the fix, the pipeline impl passed subtree manifests to a function
    /// that did `find_entry(stage_path)` on them, silently dropping all parents.
    #[mononoke::fbinit_test]
    async fn test_pipeline_independent_dirs(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir1/a", "a1")
            .add_file("dir1/b", "b1")
            .add_file("dir2/c", "c1")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .add_file("dir1/a", "a2")
            .add_file("dir2/c", "c2")
            .add_file("dir3/d", "d1")
            .commit()
            .await?;

        let parent_root = parent_root_manifest(&ctx, &repo, parent).await?;
        let expected_root = derive_root_non_pipelined(&ctx, &repo, child, parent_root).await?;

        let dir1 = MPath::new("dir1")?;
        let parent_dir1 =
            parent_subtree_at(&ctx, repo.repo_blobstore(), parent_root, dir1.clone()).await?;
        let dir1_entry = stage_entry(
            &ctx,
            &repo,
            child,
            vec![parent_dir1],
            dir1.clone(),
            HashMap::new(),
        )
        .await?;

        let dir2 = MPath::new("dir2")?;
        let parent_dir2 =
            parent_subtree_at(&ctx, repo.repo_blobstore(), parent_root, dir2.clone()).await?;
        let dir2_entry = stage_entry(
            &ctx,
            &repo,
            child,
            vec![parent_dir2],
            dir2.clone(),
            HashMap::new(),
        )
        .await?;

        // dir3/ isn't pre-derived — root traversal computes it itself,
        // mirroring the case where a stage config doesn't cover every subtree.
        let known: HashMap<_, _> = vec![(dir1, dir1_entry), (dir2, dir2_entry)]
            .into_iter()
            .collect();
        let root_entry = stage_entry(
            &ctx,
            &repo,
            child,
            vec![Some(ManifestEntry::Tree(parent_root))],
            MPath::ROOT,
            known,
        )
        .await?
        .expect("root stage should produce an entry");
        let pipelined_root = root_entry
            .into_tree()
            .expect("root entry should be a Tree")
            .into_untraced();

        assert_eq!(
            pipelined_root, expected_root,
            "pipelined root manifest id must match non-pipelined byte-for-byte",
        );
        Ok(())
    }

    /// Nested stages: derive `dir1/sub/`, then `dir1/` using its `known_entries`,
    /// then root using its `known_entries`. Verifies recursion through multiple
    /// sub-stages preserves filenode hashes.
    #[mononoke::fbinit_test]
    async fn test_pipeline_nested_dirs(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir1/sub/x", "x1")
            .add_file("dir1/sub/y", "y1")
            .add_file("dir1/top", "t1")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .add_file("dir1/sub/x", "x2")
            .add_file("dir1/top", "t2")
            .commit()
            .await?;

        let parent_root = parent_root_manifest(&ctx, &repo, parent).await?;
        let expected_root = derive_root_non_pipelined(&ctx, &repo, child, parent_root).await?;

        let inner = MPath::new("dir1/sub")?;
        let parent_inner =
            parent_subtree_at(&ctx, repo.repo_blobstore(), parent_root, inner.clone()).await?;
        let inner_entry = stage_entry(
            &ctx,
            &repo,
            child,
            vec![parent_inner],
            inner.clone(),
            HashMap::new(),
        )
        .await?;

        let dir1 = MPath::new("dir1")?;
        let parent_dir1 =
            parent_subtree_at(&ctx, repo.repo_blobstore(), parent_root, dir1.clone()).await?;
        let dir1_known: HashMap<_, _> = vec![(inner, inner_entry)].into_iter().collect();
        let dir1_entry = stage_entry(
            &ctx,
            &repo,
            child,
            vec![parent_dir1],
            dir1.clone(),
            dir1_known,
        )
        .await?;

        let root_known: HashMap<_, _> = vec![(dir1, dir1_entry)].into_iter().collect();
        let root_entry = stage_entry(
            &ctx,
            &repo,
            child,
            vec![Some(ManifestEntry::Tree(parent_root))],
            MPath::ROOT,
            root_known,
        )
        .await?
        .expect("root stage should produce an entry");
        let pipelined_root = root_entry
            .into_tree()
            .expect("root entry should be a Tree")
            .into_untraced();

        assert_eq!(
            pipelined_root, expected_root,
            "nested-stage pipelined root must match non-pipelined byte-for-byte",
        );
        Ok(())
    }

    /// Cross-stage `copy_from`: a file is copied into `dir1/` from a source
    /// outside `dir1/`. The sub-stage at `dir1/` resolves the source filenode
    /// from the parent's full root manifest, then assembles the root. The pipelined root
    /// must match the non-pipelined root byte-for-byte — the copied file's
    /// filenode carries the cross-stage source, so any divergence in the
    /// resolved source filenode changes the hash.
    #[mononoke::fbinit_test]
    async fn test_pipeline_cross_stage_copy_matches(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("outside/src", "payload")
            .add_file("dir1/keep", "keep")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .add_file_with_copy_info("dir1/copied", "payload", (parent, "outside/src"))
            .commit()
            .await?;

        let parent_root = parent_root_manifest(&ctx, &repo, parent).await?;
        let expected_root = derive_root_non_pipelined(&ctx, &repo, child, parent_root).await?;

        let dir1 = MPath::new("dir1")?;
        let parent_dir1 =
            parent_subtree_at(&ctx, repo.repo_blobstore(), parent_root, dir1.clone()).await?;
        let error = stage_entry_with_cross_stage_sources(
            &ctx,
            &repo,
            child,
            vec![],
            dir1.clone(),
            parent_root,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("missing Hg stage output"));
        let dir1_entry = stage_entry_with_cross_stage_sources(
            &ctx,
            &repo,
            child,
            vec![parent_dir1],
            dir1.clone(),
            parent_root,
        )
        .await?;

        let root_known: HashMap<_, _> = vec![(dir1, dir1_entry)].into_iter().collect();
        let root_entry = stage_entry(
            &ctx,
            &repo,
            child,
            vec![Some(ManifestEntry::Tree(parent_root))],
            MPath::ROOT,
            root_known,
        )
        .await?
        .expect("root stage should produce an entry");
        let pipelined_root = root_entry
            .into_tree()
            .expect("root entry should be a Tree")
            .into_untraced();

        assert_eq!(
            pipelined_root, expected_root,
            "cross-stage copy pipelined root must match non-pipelined byte-for-byte",
        );
        Ok(())
    }

    /// Step-parent cross-stage `copy_from` (a path-out-of-stage source
    /// referencing a 3rd+ parent of an octopus merge) must NOT trigger
    /// the cross-stage bail. The non-pipelined path silently drops
    /// step-parent copy info (it isn't propagated to hg copy metadata —
    /// see `store_file_change`), so the pipelined sub-stage must do the
    /// same and produce the same `HgManifestId` byte-for-byte.
    #[mononoke::fbinit_test]
    async fn test_pipeline_step_parent_cross_stage_copy_ok(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

        // Octopus merge fixture: 3 parents. p3 is the step-parent that
        // contains the cross-stage copy source.
        let p1 = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir1/keep_p1", "p1")
            .commit()
            .await?;
        let p2 = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir1/keep_p2", "p2")
            .commit()
            .await?;
        let p3 = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("outside/payload", "payload")
            .commit()
            .await?;
        // Merge with copy_from referencing p3 (a step-parent for hg purposes)
        // and a source path outside dir1/.
        let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3])
            .add_file_with_copy_info("dir1/from_step_parent", "payload", (p3, "outside/payload"))
            .commit()
            .await?;

        // Derive parent root manifests via the standard path so we can
        // compare against the non-pipelined derivation.
        let p1_root = parent_root_manifest(&ctx, &repo, p1).await?;
        let p2_root = parent_root_manifest(&ctx, &repo, p2).await?;
        let _p3_root = parent_root_manifest(&ctx, &repo, p3).await?;

        let bcs = merge.load(&ctx, repo.repo_blobstore()).await?;
        let restricted_paths = repo
            .repo_derived_data()
            .manager()
            .derivation_context(None)
            .restricted_paths();
        let expected_root = get_manifest_from_bonsai(
            ctx.clone(),
            std::sync::Arc::new(repo.repo_blobstore().clone()),
            restricted_paths,
            bcs,
            // Non-pipelined takes root manifests in bonsai-parent order.
            // p3 contributes no entry to dir1/ but its root manifest is
            // still part of the parent list at root.
            vec![
                p1_root,
                p2_root,
                parent_root_manifest(&ctx, &repo, p3).await?,
            ],
        )
        .await?;

        let dir1 = MPath::new("dir1")?;
        let p1_dir1 = parent_subtree_at(&ctx, repo.repo_blobstore(), p1_root, dir1.clone()).await?;
        let p2_dir1 = parent_subtree_at(&ctx, repo.repo_blobstore(), p2_root, dir1.clone()).await?;
        // p3 has no dir1/, so it contributes None at this stage.
        let dir1_entry = stage_entry(
            &ctx,
            &repo,
            merge,
            vec![p1_dir1, p2_dir1, None],
            dir1.clone(),
            HashMap::new(),
        )
        .await?;

        let p3_root = parent_root_manifest(&ctx, &repo, p3).await?;
        let root_known: HashMap<_, _> = vec![(dir1, dir1_entry)].into_iter().collect();
        let root_entry = stage_entry(
            &ctx,
            &repo,
            merge,
            vec![
                Some(ManifestEntry::Tree(p1_root)),
                Some(ManifestEntry::Tree(p2_root)),
                Some(ManifestEntry::Tree(p3_root)),
            ],
            MPath::ROOT,
            root_known,
        )
        .await?
        .expect("root stage should produce an entry");
        let pipelined_root = root_entry
            .into_tree()
            .expect("root entry should be a Tree")
            .into_untraced();

        assert_eq!(
            pipelined_root, expected_root,
            "step-parent cross-stage copy must derive identically to non-pipelined",
        );
        Ok(())
    }
}
