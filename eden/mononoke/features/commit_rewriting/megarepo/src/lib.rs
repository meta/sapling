/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(trait_alias)]
use std::collections::BTreeMap;
use std::num::NonZeroU64;

use anyhow::Error;
use anyhow::anyhow;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bookmarks::BookmarksRef;
use commit_graph::CommitGraphRef;
use commit_graph::CommitGraphWriterRef;
use context::CoreContext;
use futures::Stream;
use futures::TryStreamExt;
use futures::future;
use itertools::Itertools;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_types::HgChangesetId;
use mercurial_types::NonRootMPath;
use mercurial_types::blobs::HgBlobChangeset;
use mercurial_types::blobs::HgBlobEnvelope;
use mercurial_types::sharded_augmented_manifest::list_hg_leaf_entries;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::FileChange;
use mononoke_types::FileType;
use mononoke_types::GitLfs;
use movers::Mover;
use phases::PhasesRef;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use tracing::info;

pub mod chunking;
pub mod commit_sync_config_utils;
pub mod common;
pub mod history_fixup_delete;
pub mod pre_merge_delete;
pub mod working_copy;

use crate::common::ChangesetArgs;
use crate::common::ChangesetArgsFactory;
use crate::common::StackPosition;
use crate::common::create_save_and_generate_hg_changeset;

// NOTE: Occurrences of Option<NonRootMPath> in this file have not been replaced with MPath since such a
// replacement is only possible in cases where Option<NonRootMPath> is used to represent a path that can also
// be root. However, in this case the Some(_) and None variant of Option<NonRootMPath> are used to represent
// conditional logic, i.e. the code either does something or skips it based on None or Some.

const BUFFER_SIZE: usize = 100;
const REPORTING_INTERVAL_FILES: usize = 10000;

pub trait Repo = BonsaiHgMappingRef
    + RepoBlobstoreRef
    + RepoDerivedDataRef
    + CommitGraphRef
    + CommitGraphWriterRef
    + PhasesRef
    + BookmarksRef
    + RepoIdentityRef
    + Send
    + Sync
    + Clone;

struct FileMove {
    old_path: NonRootMPath,
    maybe_new_path: Option<NonRootMPath>, // None means that the file should be deleted hence not replacing with MPath
    file_type: FileType,
    file_size: u64,
    content_id: ContentId,
}

fn get_all_file_moves<'a>(
    ctx: &'a CoreContext,
    repo: &'a impl Repo,
    hg_cs: HgBlobChangeset,
    path_converter: &'a dyn Mover,
) -> impl Stream<Item = Result<FileMove, Error>> + 'a {
    list_hg_leaf_entries(
        ctx,
        repo.repo_blobstore().clone(),
        repo.repo_identity().name(),
        hg_cs.manifestid(),
    )
    .try_filter_map(move |(old_path, (file_type, filenode_id))| async move {
        let maybe_new_path = path_converter.move_path(&old_path).unwrap();
        if Some(&old_path) == maybe_new_path.as_ref() {
            // path does not need to be changed, drop from the stream
            Ok(None)
        } else {
            // path needs to be changed (or deleted), keep in the stream
            Ok(Some((old_path, maybe_new_path, file_type, filenode_id)))
        }
    })
    .map_ok({
        move |(old_path, maybe_new_path, file_type, filenode_id)| {
            async move {
                let file_envelope = filenode_id.load(ctx, repo.repo_blobstore()).await?;

                // Note: it is always safe to unwrap here, since
                // `HgFileEnvelope::get_size()` always returns `Some()`
                // The return type is `Option` to accommodate `HgManifestEnvelope`
                // which returns `None`.
                let file_size = file_envelope.get_size().unwrap();

                let file_move = FileMove {
                    old_path,
                    maybe_new_path,
                    file_type,
                    file_size,
                    content_id: file_envelope.content_id(),
                };
                Ok(file_move)
            }
        }
    })
    .try_buffer_unordered(BUFFER_SIZE)
}

pub async fn perform_move<'a>(
    ctx: &'a CoreContext,
    repo: &'a impl Repo,
    parent_bcs_id: ChangesetId,
    path_converter: &'a dyn Mover,
    resulting_changeset_args: ChangesetArgs,
) -> Result<HgChangesetId, Error> {
    let mut stack = perform_stack_move_impl(
        ctx,
        repo,
        parent_bcs_id,
        path_converter,
        |file_moves| vec![file_moves],
        move |_| resulting_changeset_args.clone(),
    )
    .await?;

    if stack.len() == 1 {
        stack
            .pop()
            .ok_or_else(|| anyhow!("not a single commit was created"))
    } else {
        Err(anyhow!(
            "wrong number of commits was created. Expected 1, created {}: {:?}",
            stack.len(),
            stack
        ))
    }
}

/// Move files according to path_converter in a stack of commits.
/// Each commit won't have more than `max_num_of_moves_in_commit` files.
/// Creating a stack of commits might be desirable if we want to keep each commit smaller.
pub async fn perform_stack_move<'a>(
    ctx: &'a CoreContext,
    repo: &'a impl Repo,
    parent_bcs_id: ChangesetId,
    path_converter: &'a dyn Mover,
    max_num_of_moves_in_commit: NonZeroU64,
    resulting_changeset_args: impl ChangesetArgsFactory + 'a,
) -> Result<Vec<HgChangesetId>, Error> {
    perform_stack_move_impl(
        ctx,
        repo,
        parent_bcs_id,
        path_converter,
        |file_changes| {
            file_changes
                .into_iter()
                .chunks(max_num_of_moves_in_commit.get() as usize)
                .into_iter()
                .map(|chunk| chunk.collect())
                .collect()
        },
        resulting_changeset_args,
    )
    .await
}

async fn perform_stack_move_impl<'a, Chunker>(
    ctx: &'a CoreContext,
    repo: &'a impl Repo,
    mut parent_bcs_id: ChangesetId,
    path_converter: &'a dyn Mover,
    chunker: Chunker,
    resulting_changeset_args: impl ChangesetArgsFactory + 'a,
) -> Result<Vec<HgChangesetId>, Error>
where
    Chunker: Fn(Vec<FileMove>) -> Vec<Vec<FileMove>> + 'a,
{
    let parent_hg_cs_id = repo.derive_hg_changeset(ctx, parent_bcs_id).await?;

    let parent_hg_cs = parent_hg_cs_id.load(ctx, repo.repo_blobstore()).await?;

    let mut file_changes = get_all_file_moves(ctx, repo, parent_hg_cs, path_converter)
        .try_fold(vec![], {
            move |mut collected, file_move| {
                collected.push(file_move);
                if collected.len() % REPORTING_INTERVAL_FILES == 0 {
                    info!("Processed {} files", collected.len());
                }
                future::ready(Ok(collected))
            }
        })
        .await?;

    let mut res = vec![];
    file_changes.sort_unstable_by(|first, second| first.old_path.cmp(&second.old_path));
    let chunks_iter = chunker(file_changes);

    for (idx, chunk) in chunks_iter.into_iter().enumerate() {
        let mut file_changes = BTreeMap::new();
        for file_move in chunk {
            file_changes.insert(file_move.old_path.clone(), FileChange::Deletion);
            if let Some(to) = file_move.maybe_new_path {
                let fc = FileChange::tracked(
                    file_move.content_id,
                    file_move.file_type,
                    file_move.file_size,
                    Some((file_move.old_path, parent_bcs_id)),
                    GitLfs::FullContent,
                );
                file_changes.insert(to, fc);
            }
        }

        let hg_cs_id = create_save_and_generate_hg_changeset(
            ctx,
            repo,
            vec![parent_bcs_id],
            file_changes.into(),
            resulting_changeset_args(StackPosition(idx)),
        )
        .await?;

        parent_bcs_id = repo
            .bonsai_hg_mapping()
            .get_bonsai_from_hg(ctx, hg_cs_id)
            .await?
            .ok_or_else(|| anyhow!("not found bonsai commit for {hg_cs_id}"))?;
        res.push(hg_cs_id)
    }

    Ok(res)
}

#[cfg(test)]
mod test {
    use std::str::FromStr;
    use std::sync::Arc;

    use anyhow::Result;
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use cloned::cloned;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use derivation_queue_thrift::DerivationPriority;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use fixtures::Linear;
    use fixtures::ManyFilesDirs;
    use fixtures::TestRepoFixture;
    use futures::FutureExt;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs_async;
    use manifest::ManifestOps;
    use maplit::hashmap;
    use memblob::Memblob;
    use mercurial_derivation::RootHgAugmentedManifestId;
    use mercurial_types::HgChangesetId;
    use mononoke_macros::mononoke;
    use mononoke_types::BonsaiChangeset;
    use mononoke_types::BonsaiChangesetMut;
    use mononoke_types::DateTime;
    use phases::Phases;
    use repo_blobstore::RepoBlobstore;
    use repo_derived_data::RepoDerivedData;
    use repo_identity::RepoIdentity;
    use sorted_vector_map::sorted_vector_map;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;
    use tests_utils::hg_manifest_reads::HgManifestReadBlocker;
    use tests_utils::resolve_cs_id;

    use crate::working_copy::get_working_copy_paths;

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
        commit_graph: CommitGraph,
        #[facet]
        commit_graph_writer: dyn CommitGraphWriter,
        #[facet]
        filestore_config: FilestoreConfig,
        #[facet]
        pub phases: dyn Phases,
        #[facet]
        repo_identity: RepoIdentity,
    }

    use super::*;

    struct TestMover(Box<dyn Fn(&NonRootMPath) -> Result<Option<NonRootMPath>> + Send + Sync>);

    impl TestMover {
        fn new(
            f: impl Fn(&NonRootMPath) -> Result<Option<NonRootMPath>> + Send + Sync + 'static,
        ) -> Self {
            Self(Box::new(f))
        }
    }

    impl Mover for TestMover {
        fn move_path(&self, p: &NonRootMPath) -> Result<Option<NonRootMPath>> {
            (self.0)(p)
        }

        fn conflicts_with(&self, _path: &NonRootMPath) -> Result<bool> {
            unimplemented!()
        }
    }

    fn identity_mover(p: &NonRootMPath) -> Result<Option<NonRootMPath>> {
        Ok(Some(p.clone()))
    }

    fn skip_one(p: &NonRootMPath) -> Result<Option<NonRootMPath>> {
        if &NonRootMPath::new("dir1/file_1_in_dir1").unwrap() == p {
            return Ok(None);
        }

        Ok(Some(p.clone()))
    }

    fn shift_one(p: &NonRootMPath) -> Result<Option<NonRootMPath>> {
        if &NonRootMPath::new("dir1/file_1_in_dir1").unwrap() == p {
            return Ok(Some(
                NonRootMPath::new("newdir/dir1/file_1_in_dir1").unwrap(),
            ));
        }

        Ok(Some(p.clone()))
    }

    fn shift_one_skip_another(p: &NonRootMPath) -> Result<Option<NonRootMPath>> {
        if &NonRootMPath::new("dir1/file_1_in_dir1").unwrap() == p {
            return Ok(Some(
                NonRootMPath::new("newdir/dir1/file_1_in_dir1").unwrap(),
            ));
        }

        if &NonRootMPath::new("dir2/file_1_in_dir2").unwrap() == p {
            return Ok(None);
        }

        Ok(Some(p.clone()))
    }

    fn shift_all(p: &NonRootMPath) -> Result<Option<NonRootMPath>> {
        Ok(Some(NonRootMPath::new("moved_dir")?.join(p)))
    }

    async fn prepare(
        fb: FacebookInit,
    ) -> (
        CoreContext,
        Arc<TestRepo>,
        HgChangesetId,
        ChangesetId,
        ChangesetArgs,
    ) {
        let repo: Arc<TestRepo> = Arc::new(ManyFilesDirs::get_repo(fb).await);
        let ctx = CoreContext::test_mock(fb);

        let hg_cs_id = HgChangesetId::from_str("2f866e7e549760934e31bf0420a873f65100ad63").unwrap();
        let bcs_id: ChangesetId = repo
            .bonsai_hg_mapping()
            .get_bonsai_from_hg(&ctx, hg_cs_id)
            .await
            .unwrap()
            .unwrap();
        let changeset_args = ChangesetArgs {
            author: "user".to_string(),
            message: "I like to move it".to_string(),
            datetime: DateTime::from_rfc3339("1985-04-12T23:20:50.52Z").unwrap(),
            bookmark: None,
            mark_public: false,
        };
        (ctx, repo, hg_cs_id, bcs_id, changeset_args)
    }

    async fn get_bonsai_by_hg_cs_id(
        ctx: CoreContext,
        repo: impl Repo,
        hg_cs_id: HgChangesetId,
    ) -> BonsaiChangeset {
        let bcs_id = repo
            .bonsai_hg_mapping()
            .get_bonsai_from_hg(&ctx, hg_cs_id)
            .await
            .unwrap()
            .unwrap();
        bcs_id.load(&ctx, repo.repo_blobstore()).await.unwrap()
    }

    #[mononoke::fbinit_test]
    async fn test_do_not_move_anything(fb: FacebookInit) {
        let (ctx, repo, _hg_cs_id, bcs_id, changeset_args) = prepare(fb).await;
        let newcs = perform_move(
            &ctx,
            &repo,
            bcs_id,
            &TestMover::new(identity_mover),
            changeset_args,
        )
        .await
        .unwrap();
        let newcs = get_bonsai_by_hg_cs_id(ctx.clone(), repo.clone(), newcs).await;

        let BonsaiChangesetMut {
            parents,
            file_changes,
            ..
        } = newcs.into_mut();
        assert_eq!(parents, vec![bcs_id]);
        assert_eq!(file_changes, Default::default());
    }

    #[mononoke::fbinit_test]
    async fn test_drop_file(fb: FacebookInit) {
        let (ctx, repo, _hg_cs_id, bcs_id, changeset_args) = prepare(fb).await;
        let newcs = perform_move(
            &ctx,
            &repo,
            bcs_id,
            &TestMover::new(skip_one),
            changeset_args,
        )
        .await
        .unwrap();
        let newcs = get_bonsai_by_hg_cs_id(ctx.clone(), repo.clone(), newcs).await;

        let BonsaiChangesetMut {
            parents,
            file_changes,
            ..
        } = newcs.into_mut();
        assert_eq!(parents, vec![bcs_id]);
        assert_eq!(
            file_changes,
            sorted_vector_map! {
                NonRootMPath::new("dir1/file_1_in_dir1").unwrap() => FileChange::Deletion
            }
        );
    }

    #[mononoke::fbinit_test]
    async fn test_shift_path_by_one(fb: FacebookInit) {
        let (ctx, repo, _hg_cs_id, bcs_id, changeset_args) = prepare(fb).await;
        let newcs = perform_move(
            &ctx,
            &repo,
            bcs_id,
            &TestMover::new(shift_one),
            changeset_args,
        )
        .await
        .unwrap();
        let newcs = get_bonsai_by_hg_cs_id(ctx.clone(), repo.clone(), newcs).await;

        let BonsaiChangesetMut {
            parents,
            file_changes,
            ..
        } = newcs.into_mut();
        assert_eq!(parents, vec![bcs_id]);
        let old_path = NonRootMPath::new("dir1/file_1_in_dir1").unwrap();
        let new_path = NonRootMPath::new("newdir/dir1/file_1_in_dir1").unwrap();
        assert_eq!(file_changes[&old_path], FileChange::Deletion);
        let file_change = match &file_changes[&new_path] {
            FileChange::Change(tc) => tc,
            _ => panic!(),
        };
        assert_eq!(file_change.copy_from(), Some((old_path, bcs_id)).as_ref());
    }

    async fn get_working_copy_contents(
        ctx: CoreContext,
        repo: impl Repo,
        hg_cs_id: HgChangesetId,
    ) -> BTreeMap<NonRootMPath, (FileType, ContentId)> {
        let hg_cs = hg_cs_id.load(&ctx, repo.repo_blobstore()).await.unwrap();
        hg_cs
            .manifestid()
            .list_leaf_entries(ctx.clone(), repo.repo_blobstore().clone())
            .and_then({
                cloned!(ctx, repo);
                move |(path, (file_type, filenode_id))| {
                    cloned!(ctx, repo);
                    async move { filenode_id.load(&ctx, repo.repo_blobstore()).await }
                        .map(move |env| Ok((path, (file_type, env?.content_id()))))
                }
            })
            .try_collect()
            .await
            .unwrap()
    }

    #[mononoke::fbinit_test]
    async fn test_performed_move(fb: FacebookInit) {
        let (ctx, repo, old_hg_cs_id, old_bcs_id, changeset_args) = prepare(fb).await;
        let new_hg_cs_id = perform_move(
            &ctx,
            &repo,
            old_bcs_id,
            &TestMover::new(shift_one_skip_another),
            changeset_args,
        )
        .await
        .unwrap();
        let mut old_wc = get_working_copy_contents(ctx.clone(), repo.clone(), old_hg_cs_id).await;
        let mut new_wc = get_working_copy_contents(ctx.clone(), repo.clone(), new_hg_cs_id).await;
        let _removed_file = old_wc.remove(&NonRootMPath::new("dir2/file_1_in_dir2").unwrap());
        let old_moved_file = old_wc.remove(&NonRootMPath::new("dir1/file_1_in_dir1").unwrap());
        let new_moved_file =
            new_wc.remove(&NonRootMPath::new("newdir/dir1/file_1_in_dir1").unwrap());
        // Same file should live in both locations
        assert_eq!(old_moved_file, new_moved_file);
        // After removing renamed and removed files, both working copies should be identical
        assert_eq!(old_wc, new_wc);
    }

    #[mononoke::fbinit_test]
    async fn test_stack_move(fb: FacebookInit) -> Result<(), Error> {
        let repo: Arc<TestRepo> = Arc::new(Linear::get_repo(fb).await);
        let ctx = CoreContext::test_mock(fb);

        let old_bcs_id = resolve_cs_id(&ctx, &repo, "master").await?;
        let create_cs_args = |num: StackPosition| ChangesetArgs {
            author: "user".to_string(),
            message: format!("I like to delete it: {}", num.0),
            datetime: DateTime::from_rfc3339("1985-04-12T23:20:50.52Z").unwrap(),
            bookmark: None,
            mark_public: false,
        };

        let stack = perform_stack_move(
            &ctx,
            &repo,
            old_bcs_id,
            &TestMover::new(shift_all),
            NonZeroU64::new(1).unwrap(),
            create_cs_args,
        )
        .await?;

        // 11 files, so create 1 commit for each
        assert_eq!(stack.len(), 11);

        let stack = perform_stack_move(
            &ctx,
            &repo,
            old_bcs_id,
            &TestMover::new(shift_all),
            NonZeroU64::new(2).unwrap(),
            create_cs_args,
        )
        .await?;
        assert_eq!(stack.len(), 6);

        let last_hg_cs_id = stack.last().unwrap();
        let last_hg_cs = last_hg_cs_id.load(&ctx, repo.repo_blobstore()).await?;

        let leaf_entries = last_hg_cs
            .manifestid()
            .list_leaf_entries(ctx.clone(), repo.repo_blobstore().clone())
            .try_collect::<Vec<_>>()
            .await?;

        assert_eq!(leaf_entries.len(), 11);
        let prefix = NonRootMPath::new("moved_dir")?;
        for (leaf, _) in &leaf_entries {
            assert!(prefix.is_prefix_of(leaf));
        }

        Ok(())
    }

    /// Hg changesets are derived without their augmented manifests, so that
    /// deriving one never reads Hg manifests.
    fn read_from_augmented(on: bool) -> JustKnobsInMemory {
        JustKnobsInMemory::new(hashmap! {
            "scm/mononoke:hgmanifest_read_from_augmented".to_string() => KnobVal::Bool(on),
            "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset".to_string() => KnobVal::Bool(false),
        })
    }

    /// A repo whose Hg manifest reads fail once the blocker is armed, and a
    /// commit with an hg changeset but no augmented manifest.
    async fn repo_with_blocked_hg_manifests(
        ctx: &CoreContext,
    ) -> Result<(Arc<TestRepo>, Arc<HgManifestReadBlocker>, ChangesetId)> {
        let (blobstore, blocker) = HgManifestReadBlocker::wrap(Memblob::default());
        let repo: Arc<TestRepo> = Arc::new(
            TestRepoFactory::new(ctx.fb)?
                .with_blobstore(blobstore)
                .build()
                .await?,
        );
        let cs_id = CreateCommitContext::new_root(ctx, &repo)
            .add_file("a", "a\n")
            .add_file("dir/b", "b\n")
            .add_file_with_type("dir/c", "c\n", FileType::Executable)
            .commit()
            .await?;
        // Derivation would also derive augmented manifests, so store the Hg
        // changesets as an upload does.
        fixtures::store_hg_changesets_as_uploaded(ctx, &*repo, vec![cs_id]).await?;
        with_just_knobs_async(
            read_from_augmented(false),
            repo.derive_hg_changeset(ctx, cs_id).boxed(),
        )
        .await?;
        Ok((repo, blocker, cs_id))
    }

    type MoveSummary = (NonRootMPath, Option<NonRootMPath>, FileType, u64, ContentId);

    async fn working_copy_and_moves(
        ctx: &CoreContext,
        repo: &Arc<TestRepo>,
        cs_id: ChangesetId,
    ) -> Result<(Vec<NonRootMPath>, Vec<MoveSummary>)> {
        let paths = get_working_copy_paths(ctx, repo, cs_id).await?;
        let hg_cs = repo
            .derive_hg_changeset(ctx, cs_id)
            .await?
            .load(ctx, repo.repo_blobstore())
            .await?;
        let mover = TestMover::new(shift_all);
        let mut moves: Vec<MoveSummary> = get_all_file_moves(ctx, repo, hg_cs, &mover)
            .map_ok(|m| {
                (
                    m.old_path,
                    m.maybe_new_path,
                    m.file_type,
                    m.file_size,
                    m.content_id,
                )
            })
            .try_collect()
            .await?;
        moves.sort();
        Ok((paths, moves))
    }

    #[mononoke::fbinit_test]
    async fn test_file_moves_read_from_augmented(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, blocker, cs_id) = repo_with_blocked_hg_manifests(&ctx).await?;
        repo.repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(&ctx, cs_id, DerivationPriority::LOW)
            .await?;

        // Given: the working copy and moves read from the Hg manifest.
        let from_hg = with_just_knobs_async(
            read_from_augmented(false),
            working_copy_and_moves(&ctx, &repo, cs_id).boxed(),
        )
        .await?;
        assert_eq!(from_hg.0.len(), 3, "the commit has three files");
        assert_eq!(from_hg.1.len(), 3, "the mover moves every file");

        // When: Hg manifests can no longer be read.
        blocker.arm();

        // Then: the augmented manifest gives the same results.
        let from_augmented = with_just_knobs_async(
            read_from_augmented(true),
            working_copy_and_moves(&ctx, &repo, cs_id).boxed(),
        )
        .await?;
        assert_eq!(from_augmented, from_hg);
        assert_eq!(
            blocker.reads(),
            0,
            "no Hg manifest is read with the knob on"
        );

        // And: with the knob off, the Hg manifest is still read.
        let knob_off = with_just_knobs_async(
            read_from_augmented(false),
            working_copy_and_moves(&ctx, &repo, cs_id).boxed(),
        )
        .await;
        assert!(
            knob_off.is_err(),
            "the knob off reads the blocked Hg manifest"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_file_moves_missing_augmented(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, blocker, cs_id) = repo_with_blocked_hg_manifests(&ctx).await?;
        blocker.arm();

        let err = with_just_knobs_async(
            read_from_augmented(true),
            working_copy_and_moves(&ctx, &repo, cs_id).boxed(),
        )
        .await
        .expect_err("a missing augmented manifest is an error");
        assert!(
            format!("{err:#}").contains("hgaugmentedmanifest.sha1."),
            "the error names the missing augmented manifest: {err:#}"
        );
        assert_eq!(
            blocker.reads(),
            0,
            "there is no fallback to the Hg manifest"
        );
        Ok(())
    }
}
