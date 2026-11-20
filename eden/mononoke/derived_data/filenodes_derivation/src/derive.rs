/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::format_err;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use derived_data_manager::DerivationStagePayload;
use derived_data_manager::ManifestStageDependencies;
use derived_data_manager::ManifestStagePayload;
use filenodes::FilenodeInfo;
use filenodes::FilenodeResult;
use filenodes::PreparedFilenode;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use futures::TryFutureExt;
use futures::TryStreamExt;
use futures::future::try_join_all;
use futures::pin_mut;
use futures::stream;
use itertools::Either;
use itertools::Itertools;
use manifest::Entry;
use manifest::Manifest;
use manifest::ManifestLeaf;
use manifest::TrieMapOps;
use manifest::find_intersection_of_diffs_and_parents_pruned;
use mercurial_derivation::MappedHgChangesetId;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileEnvelope;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::HgNodeHash;
use mercurial_types::blobs::File;
use mercurial_types::blobs::hg_manifests_read_from_augmented;
use mercurial_types::fetch_manifest_envelope;
use mercurial_types::nodehash::NULL_HASH;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::NonRootMPath;
use mononoke_types::RepoPath;
use mononoke_types::path::MPath;

use crate::mapping::FilenodesOnlyPublic;
use crate::mapping::PreparedRootFilenode;

pub(crate) async fn derive_filenodes(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bcs: BonsaiChangeset,
) -> Result<FilenodesOnlyPublic> {
    let (_, public_filenode, non_roots) =
        prepare_filenodes_for_cs(ctx, derivation_ctx, bcs).await?;
    if !non_roots.is_empty() {
        if let FilenodeResult::Disabled = derivation_ctx
            .filenodes()?
            .add_filenodes(ctx, non_roots)
            .await?
        {
            return Ok(FilenodesOnlyPublic::Disabled);
        }
    }
    Ok(public_filenode)
}

pub(crate) async fn derive_filenodes_in_batch(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    batch: Vec<BonsaiChangeset>,
) -> Result<Vec<(ChangesetId, FilenodesOnlyPublic, Vec<PreparedFilenode>)>> {
    stream::iter(
        batch
            .clone()
            .into_iter()
            .map(|bcs| async move { prepare_filenodes_for_cs(ctx, derivation_ctx, bcs).await }),
    )
    .buffered(100)
    .try_collect()
    .await
}

pub(crate) async fn prepare_filenodes_for_cs(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bcs: BonsaiChangeset,
) -> Result<(ChangesetId, FilenodesOnlyPublic, Vec<PreparedFilenode>)> {
    let filenodes = generate_all_filenodes(ctx, derivation_ctx, &bcs).await?;
    if filenodes.is_empty() {
        // This commit didn't create any new filenodes, and it's root manifest is the
        // same as one of the parents (that can happen if this commit is empty).
        // In that case we don't need to insert a root filenode - it will be inserted
        // when parent is derived.
        Ok((
            bcs.get_changeset_id(),
            FilenodesOnlyPublic::Present {
                root_filenode: None,
            },
            filenodes,
        ))
    } else {
        let (roots, non_roots): (Vec<_>, Vec<_>) =
            filenodes.into_iter().partition_map(classify_filenode);
        let roots_num = roots.len();
        let mut roots = roots.into_iter();

        match (roots.next(), roots.next()) {
            (Some(root_filenode), None) => Ok((
                bcs.get_changeset_id(),
                FilenodesOnlyPublic::Present {
                    root_filenode: Some(root_filenode),
                },
                non_roots,
            )),
            _ => Err(format_err!(
                "expected exactly one root, found {} for cs_id {}",
                roots_num,
                bcs.get_changeset_id()
            )),
        }
    }
}

/// Pre-condition: HgChangeset for this bcs has already been derived by this point.
/// This is enforced when interacting with this through this crate's API:
/// the BonsaiDerivable implementation in mapping.rs
///
/// With `scm/mononoke:hgmanifest_read_from_augmented` on for the repo, the
/// augmented manifests of this commit and its parents must exist too. Hg
/// changesets depend on augmented manifests, so deriving filenodes through the
/// derivation manager derives them first.
pub(crate) async fn generate_all_filenodes(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bcs: &BonsaiChangeset,
) -> Result<Vec<PreparedFilenode>> {
    let blobstore = derivation_ctx.blobstore();
    let hg_id = derivation_ctx
        .fetch_dependency::<MappedHgChangesetId>(ctx, bcs.get_changeset_id())
        .await?
        .hg_changeset_id();
    let root_mf = hg_id.load(ctx, &blobstore).await?.manifestid();
    // In case of non-existent manifest (that's created by hg if the first commit in the repo
    // is empty) it's fine to return the empty list of filenodes.
    if root_mf.clone().into_nodehash() == NULL_HASH {
        return Ok(vec![]);
    }
    // Bonsai might have > 2 parents, while mercurial supports at most 2.
    // That's fine for us - we just won't generate filenodes for paths that came from
    // stepparents. That means that linknode for these filenodes will point to a stepparent
    let parents = try_join_all(
        derivation_ctx
            .fetch_parents::<MappedHgChangesetId>(ctx, bcs)
            .await?
            .into_iter()
            .map(|id| async move {
                Result::<_>::Ok(
                    id.hg_changeset_id()
                        .load(ctx, &blobstore)
                        .await?
                        .manifestid(),
                )
            }),
    )
    .await?;
    generate_filenodes_from_manifests(ctx, derivation_ctx, root_mf, parents, hg_id, None).await
}

pub(crate) async fn generate_filenodes_from_manifests(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    root_mf: HgManifestId,
    parents: Vec<HgManifestId>,
    linknode: HgChangesetId,
    scope: Option<DerivationStagePayload>,
) -> Result<Vec<PreparedFilenode>> {
    if root_mf.into_nodehash() == NULL_HASH {
        return Ok(vec![]);
    }
    let blobstore = derivation_ctx.blobstore();
    if hg_manifests_read_from_augmented(derivation_ctx.repo_name()) {
        filenodes_from_augmented_manifests(ctx, blobstore, root_mf, parents, linknode, scope).await
    } else {
        filenodes_from_hg_manifests(ctx, blobstore, root_mf, parents, linknode, scope).await
    }
}

/// The filenodes that the commit with root manifest `root_mf` adds to its
/// parents' root manifests `parents`, read from stored Hg manifests.
async fn filenodes_from_hg_manifests(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    root_mf: HgManifestId,
    parents: Vec<HgManifestId>,
    linknode: HgChangesetId,
    scope: Option<DerivationStagePayload>,
) -> Result<Vec<PreparedFilenode>> {
    (async_stream::stream! {
        let s = filenode_entries(ctx.clone(), blobstore.clone(), root_mf, parents, scope)
        .map_ok(move |(path, entry)| {
            match entry {
                Entry::Tree(hg_mf_id) => fetch_manifest_envelope(ctx, blobstore, hg_mf_id)
                    .map_ok(move |envelope| {
                        create_manifest_filenode(path, envelope.node_id(), envelope.parents(), linknode)
                    })
                    .left_future(),
                Entry::Leaf((_, hg_filenode_id)) => async move {
                    let envelope = hg_filenode_id.load(ctx, blobstore).await?;
                    create_file_filenode(path, envelope, linknode)
                }
                    .right_future(),
            }
        })
        .try_buffer_unordered(100);

        pin_mut!(s);
        while let Some(value) = s.next().await {
            yield value;
        }
    })
    .try_collect()
    .await
}

/// Like `filenodes_from_hg_manifests`, but reads the augmented manifest stored
/// under each Hg tree id instead of the Hg tree. Directory records take the
/// tree id and Hg parents from the augmented manifest; file records still come
/// from Hg file versions. A missing augmented manifest fails the derivation.
async fn filenodes_from_augmented_manifests(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    root_mf: HgManifestId,
    parents: Vec<HgManifestId>,
    linknode: HgChangesetId,
    scope: Option<DerivationStagePayload>,
) -> Result<Vec<PreparedFilenode>> {
    // The null manifest has no augmented manifest. It is an empty tree, so a
    // null parent rules out no entry.
    let parents = parents
        .into_iter()
        .filter(|id| id.into_nodehash() != NULL_HASH)
        .map(HgAugmentedManifestId::from)
        .collect();

    filenode_entries(
        ctx.clone(),
        blobstore.clone(),
        HgAugmentedManifestId::from(root_mf),
        parents,
        scope,
    )
    .map_ok(|(path, entry)| async move {
        match entry {
            Entry::Tree(augmented_mf_id) => {
                let envelope = augmented_mf_id.load(ctx, blobstore).await?;
                let manifest = envelope.augmented_manifest();
                Ok(create_manifest_filenode(
                    path,
                    manifest.hg_node_id(),
                    (manifest.p1(), manifest.p2()),
                    linknode,
                ))
            }
            Entry::Leaf(leaf) => {
                let envelope = HgFileNodeId::new(leaf.filenode)
                    .load(ctx, blobstore)
                    .await?;
                create_file_filenode(path, envelope, linknode)
            }
        }
    })
    .try_buffer_unordered(100)
    .try_collect()
    .await
    .context("While deriving filenodes from augmented manifests")
}

fn filenode_entries<TreeId, Leaf>(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    root: TreeId,
    parents: Vec<TreeId>,
    scope: Option<DerivationStagePayload>,
) -> impl Stream<Item = Result<(MPath, Entry<TreeId, Leaf>)>>
where
    TreeId: manifest::StoreLoadable<Arc<dyn KeyedBlobstore>>
        + Clone
        + Send
        + Sync
        + Eq
        + Unpin
        + 'static,
    TreeId::Value: Manifest<Arc<dyn KeyedBlobstore>, TreeId = TreeId, Leaf = Leaf> + Send + Sync,
    Leaf: ManifestLeaf + Clone + Send + Sync + Unpin + 'static,
    <TreeId::Value as Manifest<Arc<dyn KeyedBlobstore>>>::TrieMapType:
        TrieMapOps<Arc<dyn KeyedBlobstore>, Entry<TreeId, Leaf>> + Eq,
{
    let entries = if let Some(DerivationStagePayload::Manifest(ManifestStagePayload {
        path,
        deps: ManifestStageDependencies::Partitions(_),
    })) = scope.clone()
    {
        async move {
            if parents.contains(&root) {
                return Ok(stream::iter(Vec::new()));
            }
            let lookup = |root: TreeId| {
                let (ctx, blobstore, path) = (ctx.clone(), blobstore.clone(), path.clone());
                async move {
                    let mut entry = Some(Entry::Tree(root));
                    for name in &path {
                        let Some(Entry::Tree(tree)) = entry else {
                            return Ok(None);
                        };
                        entry = tree
                            .load(&ctx, &blobstore)
                            .await?
                            .lookup(&ctx, &blobstore, name)
                            .await?;
                    }
                    Ok::<_, anyhow::Error>(entry)
                }
            };
            let Some(entry) = lookup(root).await? else {
                return Ok(stream::iter(Vec::new()));
            };
            let parent_entries = stream::iter(parents)
                .map(lookup)
                .buffered(100)
                .try_collect::<Vec<_>>()
                .await?
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            let entries = if parent_entries.contains(&entry) {
                vec![]
            } else {
                vec![Ok((path, entry, parent_entries))]
            };
            Ok(stream::iter(entries))
        }
        .try_flatten_stream()
        .left_stream()
    } else {
        let recurse_scope = scope.clone();
        find_intersection_of_diffs_and_parents_pruned(ctx, blobstore, root, parents, move |diff| {
            recurse_scope.as_ref().is_none_or(|scope| {
                scope.owns(diff.path())
                    || scope
                        .path()
                        .is_some_and(|prefix| diff.path().is_prefix_of(prefix))
            })
        })
        .right_stream()
    };
    entries.try_filter_map(move |(path, entry, parents)| {
        let owned = scope.as_ref().is_none_or(|scope| scope.owns(&path));
        async move {
            Ok((owned && !reuses_parent_filenode(&entry, &parents)).then_some((path, entry)))
        }
    })
}

/// Whether `entry` is a file with the same filenode as in one of its parents.
/// Such a file gets no new filenode, even if its file type changed.
fn reuses_parent_filenode<TreeId, Leaf: ManifestLeaf>(
    entry: &Entry<TreeId, Leaf>,
    parent_entries: &[Entry<TreeId, Leaf>],
) -> bool {
    let Entry::Leaf(leaf) = entry else {
        return false;
    };
    parent_entries.iter().any(
        |parent_entry| matches!(parent_entry, Entry::Leaf(parent_leaf) if leaf.reuses(parent_leaf)),
    )
}

pub(crate) fn classify_filenode(
    filenode: PreparedFilenode,
) -> Either<PreparedRootFilenode, PreparedFilenode> {
    if filenode.path == RepoPath::RootPath {
        let FilenodeInfo {
            filenode,
            p1,
            p2,
            copyfrom,
            linknode,
        } = filenode.info;

        Either::Left(PreparedRootFilenode {
            filenode,
            p1,
            p2,
            copyfrom,
            linknode,
        })
    } else {
        Either::Right(filenode)
    }
}

fn create_manifest_filenode(
    path: MPath,
    node_id: HgNodeHash,
    (p1, p2): (Option<HgNodeHash>, Option<HgNodeHash>),
    linknode: HgChangesetId,
) -> PreparedFilenode {
    let path = match Option::<NonRootMPath>::from(path) {
        Some(path) => RepoPath::DirectoryPath(path),
        None => RepoPath::RootPath,
    };
    let filenode = HgFileNodeId::new(node_id);
    let p1 = p1.map(HgFileNodeId::new);
    let p2 = p2.map(HgFileNodeId::new);

    PreparedFilenode {
        path,
        info: FilenodeInfo {
            filenode,
            p1,
            p2,
            copyfrom: None,
            linknode,
        },
    }
}

fn create_file_filenode(
    path: MPath,
    envelope: HgFileEnvelope,
    linknode: HgChangesetId,
) -> Result<PreparedFilenode> {
    let path = match path.into() {
        Some(path) => RepoPath::FilePath(path),
        None => {
            return Err(format_err!("unexpected empty file path"));
        }
    };
    let filenode = envelope.node_id();
    let (p1, p2) = envelope.parents();
    let copyfrom = File::extract_copied_from(envelope.metadata())?
        .map(|(path, node)| (RepoPath::FilePath(path), node));

    Ok(PreparedFilenode {
        path,
        info: FilenodeInfo {
            filenode,
            p1,
            p2,
            copyfrom,
            linknode,
        },
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;

    use anyhow::Context;
    use anyhow::Result;
    use anyhow::anyhow;
    use async_trait::async_trait;
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use cloned::cloned;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphRef;
    use commit_graph::CommitGraphWriter;
    use derivation_queue_thrift::DerivationPriority;
    use derived_data_manager::BonsaiDerivable;
    use derived_data_manager::PipelineMode;
    use derived_data_manager::StageInputs;
    use fbinit::FacebookInit;
    use filenodes::FilenodeRange;
    use filenodes::FilenodeResult;
    use filenodes::Filenodes;
    use filestore::FilestoreConfig;
    use fixtures::Linear;
    use fixtures::TestRepoFixture;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::override_just_knobs;
    use manifest::ManifestOps;
    use maplit::hashmap;
    use memblob::KeyedMemblob;
    use memblob::Memblob;
    use mercurial_derivation::DeriveHgChangeset;
    use mercurial_derivation::RootHgAugmentedManifestId;
    use mononoke_macros::mononoke;
    use mononoke_types::FileType;
    use repo_blobstore::RepoBlobstore;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;
    use tests_utils::hg_manifest_reads::HgManifestReadBlocker;
    use tests_utils::resolve_cs_id;
    use tracing::info;

    use super::*;

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
        filenodes: dyn Filenodes,
        #[facet]
        repo_identity: RepoIdentity,
    }

    async fn verify_filenodes(
        ctx: &CoreContext,
        repo: &TestRepo,
        cs_id: ChangesetId,
        expected_paths: Vec<RepoPath>,
    ) -> Result<()> {
        let bonsai = cs_id.load(ctx, &repo.repo_blobstore).await?;

        // Satisfy the precondition for this function:
        // hgchangesets was derived already
        repo.repo_derived_data()
            .manager()
            .derive::<MappedHgChangesetId>(ctx, cs_id, None, DerivationPriority::LOW)
            .await?;
        let filenodes = generate_all_filenodes(
            ctx,
            &repo.repo_derived_data().manager().derivation_context(None),
            &bonsai,
        )
        .await?;

        assert_eq!(filenodes.len(), expected_paths.len());
        for path in expected_paths {
            assert!(filenodes.iter().any(|filenode| filenode.path == path));
        }

        let linknode = repo.derive_hg_changeset(ctx, cs_id).await?;

        for filenode in filenodes {
            assert_eq!(filenode.info.linknode, linknode);
        }
        Ok(())
    }

    fn partition_stages(path: MPath) -> Vec<DerivationStagePayload> {
        let payload = ManifestStagePayload {
            path: path.clone(),
            deps: ManifestStageDependencies::Partitions(vec![b'm']),
        };
        payload
            .partitions()
            .into_iter()
            .map(|range| DerivationStagePayload::Subentries {
                path: path.clone(),
                range,
            })
            .chain(std::iter::once(DerivationStagePayload::Manifest(payload)))
            .collect()
    }

    #[mononoke::fbinit_test]
    async fn pipeline_batch_preserves_first_linknode(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        for read_from_augmented in [false, true] {
            for reverse in [false, true] {
                let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
                let canonical: TestRepo = test_repo_factory::build_empty(fb).await?;
                let mut histories = Vec::new();
                for repo in [&repo, &canonical] {
                    set_read_from_augmented(false);
                    let base = CreateCommitContext::new_root(&ctx, repo).commit().await?;
                    let first = CreateCommitContext::new(&ctx, repo, vec![base])
                        .add_file("a/file", "same")
                        .add_file("z/first", "first")
                        .commit()
                        .await?;
                    let second = CreateCommitContext::new(&ctx, repo, vec![base])
                        .add_file("a/file", "same")
                        .add_file("z/second", "second")
                        .commit()
                        .await?;
                    let batch = if reverse {
                        vec![base, second, first]
                    } else {
                        vec![base, first, second]
                    };
                    derive_hg_and_augmented(&ctx, repo, &batch).await?;
                    histories.push(batch);
                }
                assert_eq!(histories[0], histories[1]);
                let batch = histories.remove(0);
                set_read_from_augmented(read_from_augmented);
                canonical
                    .repo_derived_data
                    .manager()
                    .derive_exactly_batch::<FilenodesOnlyPublic>(&ctx, batch.clone(), None)
                    .await?;
                let manager = repo
                    .repo_derived_data
                    .manager()
                    .with_pipeline_mode(PipelineMode::Production);
                let mut outputs = Vec::new();
                for stage in partition_stages(MPath::ROOT) {
                    outputs.extend(
                        manager
                            .with_stage_inputs(StageInputs::new(&outputs)?)
                            .derive_stage_batch::<FilenodesOnlyPublic>(&ctx, batch.clone(), &stage)
                            .await?
                            .outputs,
                    );
                }
                let first_linknode = repo.derive_hg_changeset(&ctx, batch[1]).await?;
                for csid in batch.into_iter().skip(1) {
                    compare_filenodes(&ctx, &repo, &canonical, csid).await?;
                    let record = sorted_filenodes(&ctx, &repo, csid)
                        .await?
                        .into_iter()
                        .find(|r| r.path == RepoPath::file("a/file").unwrap())
                        .unwrap();
                    let stored = repo
                        .filenodes
                        .get_filenode(&ctx, &record.path, record.info.filenode)
                        .await?
                        .do_not_handle_disabled_filenodes()?
                        .unwrap();
                    assert_eq!(stored.linknode, first_linknode);
                }
            }
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn pipeline_child_write_failure_prevents_publication(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        for disabled in [false, true] {
            let rejected_csid = Arc::new(Mutex::new(None));
            let repo: TestRepo = TestRepoFactory::new(fb)?
                .with_filenodes_override({
                    cloned!(rejected_csid);
                    move |inner| {
                        Arc::new(FilenodesWrapper {
                            inner,
                            cs_id: rejected_csid.clone(),
                            disabled,
                        })
                    }
                })
                .build()
                .await?;
            let csid = CreateCommitContext::new_root(&ctx, &repo)
                .add_file("a/file", "one")
                .add_file("z/file", "two")
                .commit()
                .await?;
            let hg = repo.derive_hg_changeset(&ctx, csid).await?;
            let manager = repo
                .repo_derived_data
                .manager()
                .with_pipeline_mode(PipelineMode::Production);
            let stages = partition_stages(MPath::ROOT);
            let mut outputs = manager
                .derive_stage_batch::<FilenodesOnlyPublic>(&ctx, vec![csid], &stages[0])
                .await?
                .outputs
                .into_iter()
                .collect::<Vec<_>>();
            *rejected_csid.lock().unwrap() = Some(hg);
            let failed = manager
                .derive_stage_batch::<FilenodesOnlyPublic>(&ctx, vec![csid], &stages[1])
                .await;
            *rejected_csid.lock().unwrap() = None;
            let completed_children = outputs.clone();
            if disabled {
                outputs.extend(failed?.outputs);
            } else {
                assert!(failed.is_err());
            }
            let assembly = manager
                .with_stage_inputs(StageInputs::new(&outputs)?)
                .derive_stage_batch::<FilenodesOnlyPublic>(&ctx, vec![csid], &stages[2])
                .await;
            assert_eq!(assembly.is_ok(), disabled);
            assert!(
                FilenodesOnlyPublic::fetch(&ctx, &manager.derivation_context(None), csid)
                    .await?
                    .is_none()
            );

            let mut outputs = completed_children;
            outputs.extend(
                manager
                    .derive_stage_batch::<FilenodesOnlyPublic>(&ctx, vec![csid], &stages[1])
                    .await?
                    .outputs,
            );
            manager
                .with_stage_inputs(StageInputs::new(&outputs)?)
                .derive_stage_batch::<FilenodesOnlyPublic>(&ctx, vec![csid], &stages[2])
                .await?;
            assert!(
                FilenodesOnlyPublic::fetch(&ctx, &manager.derivation_context(None), csid)
                    .await?
                    .is_some()
            );
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn pipeline_assembly_matches_exact_entry(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        set_read_from_augmented(false);
        let commits = create_commits(&ctx, &repo).await?;
        derive_hg_and_augmented(&ctx, &repo, &commits.all).await?;
        let derivation = repo.repo_derived_data.manager().derivation_context(None);
        for csid in commits.all {
            let bonsai = csid.load(&ctx, &repo.repo_blobstore).await?;
            let linknode = repo.derive_hg_changeset(&ctx, csid).await?;
            let root = linknode
                .load(&ctx, &repo.repo_blobstore)
                .await?
                .manifestid();
            let mut parents = Vec::new();
            for parent in bonsai.parents() {
                parents.push(
                    repo.derive_hg_changeset(&ctx, parent)
                        .await?
                        .load(&ctx, &repo.repo_blobstore)
                        .await?
                        .manifestid(),
                );
            }
            set_read_from_augmented(false);
            let expected = generate_all_filenodes(&ctx, &derivation, &bonsai).await?;
            for read_from_augmented in [false, true] {
                set_read_from_augmented(read_from_augmented);
                for path in ["", "a", "a/child", "dir", "dir/sub", "absent"] {
                    let path = MPath::new(path)?;
                    let scope = partition_stages(path.clone()).pop().unwrap();
                    let actual = generate_filenodes_from_manifests(
                        &ctx,
                        &derivation,
                        root,
                        parents.clone(),
                        linknode,
                        Some(scope),
                    )
                    .await?;
                    let expected = expected
                        .iter()
                        .filter(|r| {
                            r.path
                                .mpath()
                                .cloned()
                                .map(MPath::from)
                                .unwrap_or(MPath::ROOT)
                                == path
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    assert_eq!(
                        actual, expected,
                        "commit {csid}, path {path}, augmented={read_from_augmented}"
                    );
                    if !read_from_augmented && root.into_nodehash() != NULL_HASH {
                        let ancestors = Arc::new(KeyedMemblob::default());
                        for tree in std::iter::once(root).chain(parents.iter().copied()) {
                            for ancestor in path.clone().into_ancestors().skip(1) {
                                if let Some(Entry::Tree(id)) = tree
                                    .find_entry(ctx.clone(), repo.repo_blobstore.clone(), ancestor)
                                    .await?
                                    && id.into_nodehash() != NULL_HASH
                                {
                                    let key = id.blobstore_key();
                                    let bytes = repo.repo_blobstore.get(&ctx, &key).await?.unwrap();
                                    ancestors.put(&ctx, key, bytes.into_bytes()).await?;
                                }
                            }
                        }
                        let entries = filenode_entries(
                            ctx.clone(),
                            ancestors,
                            root,
                            parents.clone(),
                            Some(partition_stages(path.clone()).pop().unwrap()),
                        )
                        .try_collect::<Vec<_>>()
                        .await?;
                        assert_eq!(
                            entries.len(),
                            expected.len(),
                            "assembly selection must not load child maps"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn pipeline_shadow_and_root_publication(fb: FacebookInit) -> Result<()> {
        for read_from_augmented in [false, true] {
            let ctx = CoreContext::test_mock(fb);
            let (repo, blocker) = repo_with_hg_manifest_blocker(fb).await?;
            set_read_from_augmented(false);
            let csid = CreateCommitContext::new_root(&ctx, &repo)
                .add_file("a/file", b"one")
                .add_file("z/file", b"two")
                .commit()
                .await?;
            derive_hg_and_augmented(&ctx, &repo, &[csid]).await?;
            let manager = repo.repo_derived_data.manager();
            let bonsai = csid.load(&ctx, &repo.repo_blobstore).await?;
            let expected =
                generate_all_filenodes(&ctx, &manager.derivation_context(None), &bonsai).await?;
            if read_from_augmented {
                blocker.arm();
                set_read_from_augmented(true);
            }
            let stages = partition_stages(MPath::ROOT);

            for mode in [PipelineMode::Shadow, PipelineMode::Production] {
                let manager = manager.with_pipeline_mode(mode);
                let mut outputs = Vec::new();
                for stage in &stages {
                    assert!(
                        FilenodesOnlyPublic::fetch(&ctx, &manager.derivation_context(None), csid)
                            .await?
                            .is_none()
                    );
                    let supplied = manager.with_stage_inputs(StageInputs::new(&outputs)?);
                    outputs.extend(
                        supplied
                            .derive_stage_batch::<FilenodesOnlyPublic>(&ctx, vec![csid], stage)
                            .await?
                            .outputs,
                    );
                }
                if mode == PipelineMode::Shadow {
                    let supplied = manager.with_stage_inputs(StageInputs::new(&outputs)?);
                    assert!(
                        supplied
                            .verify_stage_output::<FilenodesOnlyPublic>(
                                &ctx,
                                csid,
                                &stages.last().unwrap().stage_id()
                            )
                            .await?
                    );
                    for record in &expected {
                        assert!(
                            repo.filenodes
                                .get_filenode(&ctx, &record.path, record.info.filenode)
                                .await?
                                .do_not_handle_disabled_filenodes()?
                                .is_none()
                        );
                    }
                } else {
                    for record in &expected {
                        assert_eq!(
                            repo.filenodes
                                .get_filenode(&ctx, &record.path, record.info.filenode)
                                .await?
                                .do_not_handle_disabled_filenodes()?,
                            Some(record.info.clone())
                        );
                    }
                }
            }
            assert_eq!(blocker.reads(), 0);
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn generate_filenodes_simple(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let filename = "path";
        let commit = CreateCommitContext::new_root(&ctx, &repo)
            .add_file(filename, "content")
            .commit()
            .await?;

        // Two filenodes - one for root manifest, another for a file
        verify_filenodes(
            &ctx,
            &repo,
            commit,
            vec![RepoPath::RootPath, RepoPath::file(filename)?],
        )
        .await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn generate_filenodes_merge(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let first_p1 = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("path1", "content")
            .commit()
            .await?;

        let first_p2 = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("path2", "content")
            .commit()
            .await?;

        let merge = CreateCommitContext::new(&ctx, &repo, vec![first_p1, first_p2])
            .commit()
            .await?;

        // Only root filenode was added - other filenodes were reused from parents
        verify_filenodes(&ctx, &repo, merge, vec![RepoPath::RootPath]).await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn generate_filenodes_type_change(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("path", "content")
            .commit()
            .await?;

        let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .add_file_with_type("path", "content", FileType::Executable)
            .commit()
            .await?;

        // Only root filenode should be changed - change of file type doesn't change filenode
        verify_filenodes(&ctx, &repo, child, vec![RepoPath::RootPath]).await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn many_parents(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let p1 = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("path1", "content")
            .commit()
            .await?;
        let p2 = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("path2", "content")
            .commit()
            .await?;
        let p3 = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("path3", "content")
            .commit()
            .await?;

        let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2, p3])
            .commit()
            .await?;

        info!("checking filenodes for {}", p3);
        verify_filenodes(
            &ctx,
            &repo,
            p3,
            vec![RepoPath::RootPath, RepoPath::file("path3")?],
        )
        .await?;

        info!("checking filenodes for {}", merge);
        verify_filenodes(&ctx, &repo, merge, vec![RepoPath::RootPath]).await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn derive_empty_commits(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let parent_empty = CreateCommitContext::new_root(&ctx, &repo).commit().await?;

        let child_empty = CreateCommitContext::new(&ctx, &repo, vec![parent_empty])
            .add_file("file", "content")
            .commit()
            .await?;

        let manager = repo.repo_derived_data().manager();

        manager
            .derive::<FilenodesOnlyPublic>(&ctx, child_empty, None, DerivationPriority::LOW)
            .await?;

        // Make sure they are in the mapping
        let maps = manager
            .fetch_derived_batch::<FilenodesOnlyPublic>(&ctx, vec![parent_empty, child_empty], None)
            .await?;

        assert_eq!(maps.len(), 2);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn derive_only_empty_commits(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;

        let parent_empty = CreateCommitContext::new_root(&ctx, &repo).commit().await?;
        let child_empty = CreateCommitContext::new(&ctx, &repo, vec![parent_empty])
            .commit()
            .await?;

        let manager = repo.repo_derived_data().manager();
        manager
            .derive::<FilenodesOnlyPublic>(&ctx, child_empty, None, DerivationPriority::LOW)
            .await?;

        // Make sure they are in the mapping
        let maps = manager
            .fetch_derived_batch::<FilenodesOnlyPublic>(&ctx, vec![child_empty, parent_empty], None)
            .await?;
        assert_eq!(maps.len(), 2);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn verify_batch_and_sequential_derive(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo1: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        Linear::init_repo(fb, &repo1).await?;
        let repo2: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        Linear::init_repo(fb, &repo2).await?;
        let master_cs_id = resolve_cs_id(&ctx, &repo1, "master").await?;
        let mut cs_ids = repo1
            .commit_graph()
            .ancestors_difference(&ctx, vec![master_cs_id], vec![])
            .await?;
        cs_ids.reverse();

        let manager1 = repo1.repo_derived_data().manager();
        manager1
            .derive_exactly_batch::<FilenodesOnlyPublic>(&ctx, cs_ids.clone(), None)
            .await?;
        let batch = manager1
            .fetch_derived_batch::<FilenodesOnlyPublic>(&ctx, cs_ids.clone(), None)
            .await?;

        let sequential = {
            let mut res = HashMap::new();
            for cs in cs_ids.clone() {
                let root_filenode = repo2
                    .repo_derived_data()
                    .derive::<FilenodesOnlyPublic>(&ctx, cs, DerivationPriority::LOW)
                    .await?;
                res.insert(cs, root_filenode);
            }
            res
        };

        assert_eq!(batch, sequential);
        for cs in cs_ids {
            compare_filenodes(&ctx, &repo1, &repo2, cs).await?;
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn derive_parents_before_children(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let filenodes_cs_id = Arc::new(Mutex::new(None));
        let mut factory = TestRepoFactory::new(fb)?;
        let repo: TestRepo = factory
            .with_filenodes_override({
                cloned!(filenodes_cs_id);
                move |inner| {
                    Arc::new(FilenodesWrapper {
                        inner,
                        cs_id: filenodes_cs_id.clone(),
                        disabled: false,
                    })
                }
            })
            .build()
            .await?;
        Linear::init_repo(fb, &repo).await?;
        let commit8 =
            resolve_cs_id(&ctx, &repo, "a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157").await?;
        let commit8 = repo.derive_hg_changeset(&ctx, commit8).await?;
        *filenodes_cs_id.lock().unwrap() = Some(commit8);
        let master_cs_id = resolve_cs_id(&ctx, &repo, "master").await?;
        let mut cs_ids = repo
            .commit_graph()
            .ancestors_difference(&ctx, vec![master_cs_id], vec![])
            .await?;
        cs_ids.reverse();

        let manager = repo.repo_derived_data().manager();

        match manager
            .derive_exactly_batch::<FilenodesOnlyPublic>(&ctx, cs_ids.clone(), None)
            .await
        {
            Ok(_) => {}
            Err(_) => {}
        };

        // FilenodesWrapper prevents writing of root filenode for a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157 (8th commit in repo)
        // so all children (9, 10, 11) should not have root_filenodes written
        for cs_id in cs_ids.into_iter().skip(8) {
            let filenode = manager
                .fetch_derived::<FilenodesOnlyPublic>(&ctx, cs_id, None)
                .await?;
            assert_eq!(filenode, None);
        }
        Ok(())
    }

    struct FilenodesWrapper {
        inner: Arc<dyn Filenodes + Send + Sync>,
        cs_id: Arc<Mutex<Option<HgChangesetId>>>,
        disabled: bool,
    }

    #[async_trait]
    impl Filenodes for FilenodesWrapper {
        async fn add_filenodes(
            &self,
            ctx: &CoreContext,
            info: Vec<PreparedFilenode>,
        ) -> Result<FilenodeResult<()>> {
            // compare PreparedFilenode::Info::Linknode
            if let Some(cs_id) = *self.cs_id.lock().unwrap() {
                if info.iter().any(|filenode| filenode.info.linknode == cs_id) {
                    return if self.disabled {
                        Ok(FilenodeResult::Disabled)
                    } else {
                        Err(anyhow!("filenodes for {cs_id} are prohibited"))
                    };
                }
            }
            self.inner.add_filenodes(ctx, info).await
        }

        async fn add_or_replace_filenodes(
            &self,
            ctx: &CoreContext,
            info: Vec<PreparedFilenode>,
        ) -> Result<FilenodeResult<()>> {
            self.inner.add_or_replace_filenodes(ctx, info).await
        }

        async fn get_filenode(
            &self,
            ctx: &CoreContext,
            path: &RepoPath,
            filenode: HgFileNodeId,
        ) -> Result<FilenodeResult<Option<FilenodeInfo>>> {
            self.inner.get_filenode(ctx, path, filenode).await
        }

        async fn get_all_filenodes_maybe_stale(
            &self,
            ctx: &CoreContext,
            path: &RepoPath,
            limit: Option<u64>,
        ) -> Result<FilenodeResult<FilenodeRange>> {
            self.inner
                .get_all_filenodes_maybe_stale(ctx, path, limit)
                .await
        }
    }

    async fn compare_filenodes(
        ctx: &CoreContext,
        repo: &TestRepo,
        backup_repo: &TestRepo,
        cs: ChangesetId,
    ) -> Result<()> {
        let manifest = repo
            .derive_hg_changeset(ctx, cs)
            .await?
            .load(ctx, &repo.repo_blobstore)
            .await
            .with_context(|| format!("while fetching manifest from prod for cs {cs:?}"))?
            .manifestid();
        manifest
            .list_all_entries(ctx.clone(), repo.repo_blobstore.clone())
            .map_ok(|(path, entry)| {
                async move {
                    let (path, node) = match (Option::<NonRootMPath>::from(path), entry) {
                        (Some(path), Entry::Leaf((_, id))) => (RepoPath::FilePath(path), id),
                        (Some(path), Entry::Tree(id)) => (
                            RepoPath::DirectoryPath(path),
                            HgFileNodeId::new(id.into_nodehash()),
                        ),
                        (None, Entry::Leaf((_, id))) => (RepoPath::RootPath, id),
                        (None, Entry::Tree(id)) => {
                            (RepoPath::RootPath, HgFileNodeId::new(id.into_nodehash()))
                        }
                    };
                    let prod = repo.filenodes
                        .get_filenode(ctx, &path, node)
                        .await
                        .with_context(|| format!("while get prod filenode for cs {cs:?}"))?;
                    let backup = backup_repo.filenodes
                        .get_filenode(ctx, &path, node)
                        .await
                        .with_context(|| format!("while get backup filenode for cs {cs:?}"))?;
                    match (prod, backup) {
                        (FilenodeResult::Present(prod), FilenodeResult::Present(backup)) => {
                            assert!(prod == backup, "Different filenode for cs {cs} with path {path:?}\nfilenode in prod repo {prod:?}\nfilenode in backup repo {backup:?}");
                            Ok(())
                        }
                        (FilenodeResult::Disabled, FilenodeResult::Disabled) => Ok(()),
                        (_, _) => Err(anyhow!("filenodes results different for cs: {cs:?}")),
                    }
                }
            })
            .try_buffer_unordered(100)
            .try_for_each(|_| async { Ok(()) })
            .await?;
        Ok(())
    }

    /// Filenodes use a content-addressed root filenode as derivation marker.
    /// For merges with no file changes, create_hg_manifest reuses P1's manifest,
    /// so merge and P1 share the same marker. Deriving filenodes for P1 makes
    /// the merge falsely appear "derived," breaking the monotonicity assumption
    /// in ancestors_frontier_with and causing derivation failures for descendants.
    #[mononoke::fbinit_test]
    async fn derive_filenodes_shared_root_manifest(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await?;

        //    target (parents: merge, c, adds target_file)
        //   /      \
        //  merge    |
        //  |    \   |
        //  a     c -+
        //        |
        //        b
        //
        // merge(a, c) has no file changes → HG manifest reuses a's.
        // Deriving filenodes for a makes merge appear "derived."
        // Deriving target fails: boundary parent c is in ancestors(merge)
        // (assumed derived) but is NOT actually derived.
        let b = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir/f", "v")
            .commit()
            .await?;
        let c = CreateCommitContext::new(&ctx, &repo, vec![b])
            .delete_file("dir/f")
            .commit()
            .await?;
        let a = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir/f", "v")
            .commit()
            .await?;
        let merge = CreateCommitContext::new(&ctx, &repo, vec![a, c])
            .commit()
            .await?;
        let target = CreateCommitContext::new(&ctx, &repo, vec![merge, c])
            .add_file("target_file", "v")
            .commit()
            .await?;

        let manager = repo.repo_derived_data().manager();

        // Only derive filenodes for a. This makes merge falsely appear
        // "derived" via the shared root manifest, while b and c remain
        // underived.
        manager
            .derive::<FilenodesOnlyPublic>(&ctx, a, None, DerivationPriority::LOW)
            .await?;

        // Gap-filling detects the underived boundary parent and fills it.
        manager
            .derive::<FilenodesOnlyPublic>(&ctx, target, None, DerivationPriority::LOW)
            .await?;

        Ok(())
    }

    /// Sets `hgmanifest_read_from_augmented` for every task on this thread,
    /// including derivation tasks the test spawns. Hg changesets are derived
    /// without their augmented manifests, which the tests derive explicitly.
    fn set_read_from_augmented(read_from_augmented: bool) {
        override_just_knobs(JustKnobsInMemory::new(hashmap! {
            "scm/mononoke:hgmanifest_read_from_augmented".to_string() => KnobVal::Bool(read_from_augmented),
            "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset".to_string() => KnobVal::Bool(false),
        }));
    }

    /// A repo whose stored Hg manifests can no longer be read once the
    /// returned blocker is armed.
    async fn repo_with_hg_manifest_blocker(
        fb: FacebookInit,
    ) -> Result<(TestRepo, Arc<HgManifestReadBlocker>)> {
        let (blobstore, blocker) = HgManifestReadBlocker::wrap(Memblob::default());
        let repo = TestRepoFactory::new(fb)?
            .with_blobstore(blobstore)
            .build()
            .await?;
        Ok((repo, blocker))
    }

    /// Derives the Hg changeset and the augmented manifest of each commit.
    /// Both read Hg manifests, so this runs before the blocker is armed.
    async fn derive_hg_and_augmented(
        ctx: &CoreContext,
        repo: &TestRepo,
        cs_ids: &[ChangesetId],
    ) -> Result<()> {
        for cs_id in cs_ids {
            repo.derive_hg_changeset(ctx, *cs_id).await?;
            repo.repo_derived_data()
                .derive::<RootHgAugmentedManifestId>(ctx, *cs_id, DerivationPriority::LOW)
                .await?;
        }
        Ok(())
    }

    /// The filenodes generated for `cs_id`, in a stable order.
    async fn sorted_filenodes(
        ctx: &CoreContext,
        repo: &TestRepo,
        cs_id: ChangesetId,
    ) -> Result<Vec<PreparedFilenode>> {
        let bonsai = cs_id.load(ctx, &repo.repo_blobstore).await?;
        let mut filenodes = generate_all_filenodes(
            ctx,
            &repo.repo_derived_data().manager().derivation_context(None),
            &bonsai,
        )
        .await?;
        filenodes.sort_by_key(|filenode| format!("{filenode:?}"));
        Ok(filenodes)
    }

    struct Commits {
        /// Every commit, parents before children.
        all: Vec<ChangesetId>,
        type_change: ChangesetId,
        merge: ChangesetId,
    }

    /// An empty root, then commits that change files in nested directories,
    /// change a file's type but not its filenode, change nothing, merge two
    /// branches that both changed a directory, and merge three parents.
    async fn create_commits(ctx: &CoreContext, repo: &TestRepo) -> Result<Commits> {
        let empty_root = CreateCommitContext::new_root(ctx, repo).commit().await?;
        let base = CreateCommitContext::new(ctx, repo, vec![empty_root])
            .add_file("a", "a")
            .add_file("dir/b", "b")
            .add_file("dir/sub/c", "c")
            .commit()
            .await?;
        let child = CreateCommitContext::new(ctx, repo, vec![base])
            .add_file("dir/sub/c", "c2")
            .add_file("new/d", "d")
            .commit()
            .await?;
        let type_change = CreateCommitContext::new(ctx, repo, vec![child])
            .add_file_with_type("a", "a", FileType::Executable)
            .commit()
            .await?;
        let empty = CreateCommitContext::new(ctx, repo, vec![type_change])
            .commit()
            .await?;
        let left = CreateCommitContext::new(ctx, repo, vec![empty])
            .add_file("dir/b", "left")
            .commit()
            .await?;
        let right = CreateCommitContext::new(ctx, repo, vec![empty])
            .add_file("dir/b", "right")
            .add_file("dir/sub/e", "e")
            .commit()
            .await?;
        let merge = CreateCommitContext::new(ctx, repo, vec![left, right])
            .add_file("dir/b", "merged")
            .commit()
            .await?;
        let other_root = CreateCommitContext::new_root(ctx, repo)
            .add_file("other/f", "f")
            .commit()
            .await?;
        let third_root = CreateCommitContext::new_root(ctx, repo)
            .add_file("third/h", "h")
            .commit()
            .await?;
        let octopus = CreateCommitContext::new(ctx, repo, vec![merge, other_root, third_root])
            .add_file("dir/g", "g")
            .commit()
            .await?;
        Ok(Commits {
            all: vec![
                empty_root,
                base,
                child,
                type_change,
                empty,
                left,
                right,
                merge,
                other_root,
                third_root,
                octopus,
            ],
            type_change,
            merge,
        })
    }

    #[mononoke::fbinit_test]
    async fn filenodes_from_augmented_match_hg(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, blocker) = repo_with_hg_manifest_blocker(fb).await?;
        set_read_from_augmented(false);
        let commits = create_commits(&ctx, &repo).await?;
        derive_hg_and_augmented(&ctx, &repo, &commits.all).await?;

        // Given: the filenodes generated from Hg manifests.
        let mut from_hg = HashMap::new();
        for cs_id in &commits.all {
            from_hg.insert(*cs_id, sorted_filenodes(&ctx, &repo, *cs_id).await?);
        }

        // When: Hg manifests can no longer be read, and the switch is on.
        blocker.arm();
        set_read_from_augmented(true);

        // Then: the filenodes generated from augmented manifests are the same.
        for cs_id in &commits.all {
            let from_augmented = sorted_filenodes(&ctx, &repo, *cs_id).await?;
            assert_eq!(
                from_augmented, from_hg[cs_id],
                "filenodes of commit {cs_id} differ"
            );
        }
        assert_eq!(blocker.reads(), 0, "no Hg manifest is read");

        // And: the commits cover the cases they are meant to.
        assert_eq!(
            from_hg[&commits.type_change]
                .iter()
                .map(|filenode| &filenode.path)
                .collect::<Vec<_>>(),
            vec![&RepoPath::RootPath],
            "a type change that keeps the filenode adds only the root filenode"
        );
        let merged_dir = from_hg[&commits.merge]
            .iter()
            .find(|filenode| filenode.path == RepoPath::dir("dir").unwrap())
            .expect("the merge adds a filenode for dir");
        assert!(
            merged_dir.info.p1.is_some() && merged_dir.info.p2.is_some(),
            "the merged directory has two parents: {merged_dir:?}"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn filenodes_from_augmented_with_null_parent_manifest(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, blocker) = repo_with_hg_manifest_blocker(fb).await?;
        set_read_from_augmented(false);
        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a", "a")
            .add_file("dir/b", "b")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .add_file("dir/b", "b2")
            .add_file("new/c", "c")
            .commit()
            .await?;
        derive_hg_and_augmented(&ctx, &repo, &[parent, child]).await?;
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
        let blobstore = derivation_ctx.blobstore();
        let linknode = repo.derive_hg_changeset(&ctx, child).await?;
        let child_mf = linknode
            .load(&ctx, &repo.repo_blobstore)
            .await?
            .manifestid();
        let parent_mf = repo
            .derive_hg_changeset(&ctx, parent)
            .await?
            .load(&ctx, &repo.repo_blobstore)
            .await?
            .manifestid();

        // Given: the null manifest as the only, first or second parent of a
        // non-empty manifest, and the filenodes read from Hg manifests.
        let null_mf = HgManifestId::new(NULL_HASH);
        let parent_lists = [
            vec![null_mf],
            vec![null_mf, parent_mf],
            vec![parent_mf, null_mf],
        ];
        let mut from_hg = Vec::new();
        for parents in &parent_lists {
            let mut filenodes = filenodes_from_hg_manifests(
                &ctx,
                blobstore,
                child_mf,
                parents.clone(),
                linknode,
                None,
            )
            .await?;
            filenodes.sort_by_key(|filenode| format!("{filenode:?}"));
            from_hg.push(filenodes);
        }

        // When: Hg manifests can no longer be read.
        blocker.arm();

        // Then: augmented manifests give the same filenodes. The null manifest
        // has no augmented manifest; it reads as an empty tree.
        for (parents, from_hg) in parent_lists.iter().zip(from_hg) {
            let mut from_augmented = filenodes_from_augmented_manifests(
                &ctx,
                blobstore,
                child_mf,
                parents.clone(),
                linknode,
                None,
            )
            .await?;
            from_augmented.sort_by_key(|filenode| format!("{filenode:?}"));
            assert_eq!(from_augmented, from_hg, "parents {parents:?}");
        }
        assert_eq!(blocker.reads(), 0, "no Hg manifest is read");
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn filenodes_from_augmented_fail_without_augmented_manifest(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, blocker) = repo_with_hg_manifest_blocker(fb).await?;
        set_read_from_augmented(false);
        let commit = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir/a", "a")
            .commit()
            .await?;

        // Given: a commit with an Hg changeset but no augmented manifest.
        fixtures::store_hg_changesets_as_uploaded(&ctx, &repo, vec![commit]).await?;
        blocker.arm();

        // When: filenodes are generated with the switch on.
        set_read_from_augmented(true);
        let err = sorted_filenodes(&ctx, &repo, commit)
            .await
            .expect_err("a missing augmented manifest is an error");

        // Then: the error names the missing augmented manifest, and nothing
        // fell back to the Hg manifest.
        assert!(
            format!("{err:#}").contains("hgaugmentedmanifest.sha1."),
            "the error names the missing augmented manifest: {err:#}"
        );
        assert_eq!(blocker.reads(), 0, "no Hg manifest is read");
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn derive_filenodes_from_augmented_matches_hg(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        set_read_from_augmented(false);

        // Given: the same commits in two repos, one with filenodes derived
        // from Hg manifests.
        let hg_repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let commits = create_commits(&ctx, &hg_repo).await?;
        let (augmented_repo, blocker) = repo_with_hg_manifest_blocker(fb).await?;
        let augmented_commits = create_commits(&ctx, &augmented_repo).await?;
        assert_eq!(augmented_commits.all, commits.all);
        let head = *commits.all.last().expect("commits are not empty");
        hg_repo
            .repo_derived_data()
            .derive::<FilenodesOnlyPublic>(&ctx, head, DerivationPriority::LOW)
            .await?;

        // When: the other repo derives them from augmented manifests.
        derive_hg_and_augmented(&ctx, &augmented_repo, &commits.all).await?;
        blocker.arm();
        set_read_from_augmented(true);
        augmented_repo
            .repo_derived_data()
            .derive::<FilenodesOnlyPublic>(&ctx, head, DerivationPriority::LOW)
            .await?;
        assert_eq!(blocker.reads(), 0, "no Hg manifest is read");

        // Then: both repos have the same derived data and filenodes for every
        // commit, the empty ones included.
        let from_hg = hg_repo
            .repo_derived_data()
            .manager()
            .fetch_derived_batch::<FilenodesOnlyPublic>(&ctx, commits.all.clone(), None)
            .await?;
        let from_augmented = augmented_repo
            .repo_derived_data()
            .manager()
            .fetch_derived_batch::<FilenodesOnlyPublic>(&ctx, commits.all.clone(), None)
            .await?;
        assert_eq!(from_augmented.len(), commits.all.len());
        assert_eq!(from_augmented, from_hg);
        for cs_id in commits.all {
            compare_filenodes(&ctx, &hg_repo, &augmented_repo, cs_id).await?;
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn derive_filenodes_from_augmented_derives_augmented_manifests_first(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        set_read_from_augmented(false);

        // Given: the same new commits in two repos, one with filenodes derived
        // from Hg manifests, and no derived data in the other.
        let hg_repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let commits = create_commits(&ctx, &hg_repo).await?;
        let augmented_repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let augmented_commits = create_commits(&ctx, &augmented_repo).await?;
        assert_eq!(augmented_commits.all, commits.all);
        let head = *commits.all.last().expect("commits are not empty");
        hg_repo
            .repo_derived_data()
            .derive::<FilenodesOnlyPublic>(&ctx, head, DerivationPriority::LOW)
            .await?;

        // When: the other repo derives filenodes through the manager with the
        // switch on, without deriving augmented manifests first.
        set_read_from_augmented(true);
        augmented_repo
            .repo_derived_data()
            .derive::<FilenodesOnlyPublic>(&ctx, head, DerivationPriority::LOW)
            .await?;

        // Then: both repos have the same derived data and filenodes for every
        // commit.
        let from_hg = hg_repo
            .repo_derived_data()
            .manager()
            .fetch_derived_batch::<FilenodesOnlyPublic>(&ctx, commits.all.clone(), None)
            .await?;
        let from_augmented = augmented_repo
            .repo_derived_data()
            .manager()
            .fetch_derived_batch::<FilenodesOnlyPublic>(&ctx, commits.all.clone(), None)
            .await?;
        assert_eq!(from_augmented.len(), commits.all.len());
        assert_eq!(from_augmented, from_hg);
        for cs_id in commits.all {
            compare_filenodes(&ctx, &hg_repo, &augmented_repo, cs_id).await?;
        }
        Ok(())
    }
}
