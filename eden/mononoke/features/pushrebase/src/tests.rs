/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::format_err;
use async_trait::async_trait;
use blobrepo_hg::BlobRepoHg;
use blobrepo_hg::CreateChangeset;
use blobstore::BlobstoreGetData;
use blobstore::Loadable;
use blobstore::LoadableError;
use bonsai_hg_mapping::BonsaiHgMapping;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bookmarks::BookmarkTransactionError;
use bookmarks::Bookmarks;
use cloned::cloned;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use derivation_queue_thrift::DerivationPriority;
use fbinit::FacebookInit;
use filenodes_derivation::FilenodesOnlyPublic;
use filestore::FilestoreConfig;
use fixtures::Linear;
use fixtures::ManyFilesDirs;
use fixtures::MergeEven;
use fixtures::TestRepoFixture;
use futures::future::TryFutureExt;
use futures::future::try_join_all;
use futures::stream;
use futures::stream::TryStreamExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::override_just_knobs;
use manifest::Entry;
use manifest::ManifestOps;
use maplit::btreemap;
use maplit::hashmap;
use maplit::hashset;
use memblob::Memblob;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_types::NULL_HASH;
use mercurial_types::blobs::ChangesetMetadata;
use mononoke_macros::mononoke;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::FileType;
use mononoke_types::GitLfs;
use mononoke_types::PrefixTrie;
use mononoke_types::RepoPath;
use mononoke_types::RepositoryId;
use mutable_counters::MutableCounters;
use mutable_counters::MutableCountersRef;
use mutable_counters::SqlMutableCounters;
use pushrebase_mutation_mapping::PushrebaseMutationMapping;
use pushrebase_mutation_mapping::PushrebaseMutationMappingEntry;
use pushrebase_mutation_mapping::PushrebaseMutationMappingRef;
use pushrebase_mutation_mapping::add_pushrebase_mapping;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedData;
use repo_identity::RepoIdentity;
use restricted_paths::RestrictedPaths;
use restricted_paths::RestrictedPathsArc;
use samplingblob::SamplingBlobstore;
use samplingblob::SamplingHandler;
use sql_ext::Transaction;
use sql_ext::TransactionResult;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;
use tests_utils::bookmark;
use tests_utils::drawdag::extend_from_dag_with_actions;
use tests_utils::list_working_copy;
use tests_utils::list_working_copy_with_types;
use tests_utils::resolve_cs_id;
use tokio::sync::oneshot;

use super::*;

fn init_just_knobs_for_test() {
    override_just_knobs(JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:pushrebase_enable_merge_resolution".to_string() => KnobVal::Bool(false),
        "scm/mononoke:pushrebase_merge_resolution_derive_fsnodes".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_range_diff_use_content_manifests".to_string() => KnobVal::Bool(false),
        "scm/mononoke:pushrebase_replay_from_fork_point".to_string() => KnobVal::Int(0),
    }));
}

/// The standard map plus the fork-point replay mode and the merge limits the
/// replay's content comparison reads.
fn init_just_knobs_with_replay(mode: i64) {
    override_just_knobs(JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:pushrebase_enable_merge_resolution".to_string() => KnobVal::Bool(false),
        "scm/mononoke:pushrebase_merge_resolution_derive_fsnodes".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_range_diff_use_content_manifests".to_string() => KnobVal::Bool(false),
        "scm/mononoke:pushrebase_replay_from_fork_point".to_string() => KnobVal::Int(mode),
        "scm/mononoke:pushrebase_max_merge_conflicts".to_string() => KnobVal::Int(100),
        "scm/mononoke:pushrebase_max_merge_file_size".to_string() => KnobVal::Int(1_000_000),
    }));
}

#[facet::container]
#[derive(Clone)]
pub(super) struct PushrebaseTestRepo {
    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    bookmarks: dyn Bookmarks,

    #[facet]
    sql_bookmarks: dbbookmarks::SqlBookmarks,

    #[facet]
    repo_blobstore: RepoBlobstore,

    #[facet]
    repo_derived_data: RepoDerivedData,

    #[facet]
    repo_identity: RepoIdentity,

    #[facet]
    filestore_config: FilestoreConfig,

    #[facet]
    mutable_counters: dyn MutableCounters,

    #[facet]
    commit_graph: CommitGraph,

    #[facet]
    commit_graph_writer: dyn CommitGraphWriter,
    #[facet]
    pushrebase_mutation_mapping: dyn PushrebaseMutationMapping,

    #[facet]
    restricted_paths: RestrictedPaths,
}

fn queued_pushrebase_request(
    ctx: &CoreContext,
    stack: PushrebaseStack,
    response_tx: oneshot::Sender<Result<PushrebaseOutcome, SharedError<PushrebaseError>>>,
) -> QueuedPushrebaseRequest {
    QueuedPushrebaseRequest {
        ctx: ctx.clone(),
        stack,
        retry_num: PushrebaseRetryNum(0),
        response_tx,
        enqueued_at: tokio::time::Instant::now(),
    }
}

async fn fetch_bonsai_changesets(
    ctx: &CoreContext,
    repo: &(impl Repo + BonsaiHgMappingRef),
    commit_ids: &HashSet<HgChangesetId>,
) -> Result<HashSet<BonsaiChangeset>, PushrebaseError> {
    let futs = commit_ids.iter().map(async |hg_cs_id| {
        let bcs_id = repo
            .bonsai_hg_mapping()
            .get_bonsai_from_hg(ctx, *hg_cs_id)
            .await?
            .ok_or_else(|| {
                Error::from(PushrebaseInternalError::BonsaiNotFoundForHgChangeset(
                    *hg_cs_id,
                ))
            })?;

        let bcs = bcs_id
            .load(ctx, repo.repo_blobstore())
            .await
            .context("While initial bonsai changesets fetching")?;

        Result::<_, Error>::Ok(bcs)
    });

    let ret = try_join_all(futs).await?.into_iter().collect();
    Ok(ret)
}

async fn do_pushrebase(
    ctx: &CoreContext,
    repo: &(impl Repo + BonsaiHgMappingRef),
    config: &PushrebaseFlags,
    onto_bookmark: &BookmarkKey,
    pushed_set: &HashSet<HgChangesetId>,
) -> Result<PushrebaseOutcome, PushrebaseError> {
    init_just_knobs_for_test();
    let pushed = fetch_bonsai_changesets(ctx, repo, pushed_set).await?;

    let res = do_pushrebase_bonsai(ctx, repo, config, onto_bookmark, &pushed, &[], None).await?;

    Ok(res)
}

async fn set_bookmark(
    ctx: CoreContext,
    repo: &(impl Repo + BonsaiHgMappingRef),
    book: &BookmarkKey,
    cs_id: &str,
) -> Result<(), Error> {
    let head = HgChangesetId::from_str(cs_id)?;
    let head = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(&ctx, head)
        .await?
        .ok_or_else(|| Error::msg(format_err!("Head not found: {cs_id:?}")))?;

    let mut txn = repo.bookmarks().create_transaction(ctx);
    txn.force_set(book, head, BookmarkUpdateReason::TestMove)?;
    txn.commit().await?;
    Ok(())
}

fn make_paths(paths: &[&str]) -> Vec<MPath> {
    let paths: Result<_, _> = paths.iter().map(MPath::new).collect();
    paths.unwrap()
}

fn master_bookmark() -> BookmarkKey {
    BookmarkKey::new("master").unwrap()
}

async fn push_and_verify(
    ctx: &CoreContext,
    repo: &(impl Repo + BonsaiHgMappingRef),
    parent: ChangesetId,
    bookmark: &BookmarkKey,
    content: BTreeMap<&str, Option<&str>>,
    should_succeed: bool,
) -> Result<(), Error> {
    let mut commit_ctx = CreateCommitContext::new(ctx, repo, vec![parent]);

    for (path, maybe_content) in content.iter() {
        let path: &str = path;
        commit_ctx = match maybe_content {
            Some(content) => commit_ctx.add_file(path, *content),
            None => commit_ctx.delete_file(path),
        };
    }

    let cs_id = commit_ctx.commit().await?;

    let hgcss = hashset![repo.derive_hg_changeset(ctx, cs_id).await?];

    let res = do_pushrebase(ctx, repo, &PushrebaseFlags::default(), bookmark, &hgcss).await;

    if should_succeed {
        assert!(res.is_ok());
    } else {
        should_have_conflicts(res);
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_one_commit(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    // Bottom commit of the repo
    let parents = vec!["2d7d4ba9ce0a6ffd222de7785b249ead9c51c536"];
    let bcs_id = CreateCommitContext::new(&ctx, &repo, parents)
        .add_file("file", "content")
        .commit()
        .await?;

    let hg_cs = repo.derive_hg_changeset(&ctx, bcs_id).await?;

    let book = master_bookmark();
    bookmark(&ctx, &repo, book.clone())
        .set_to("a5ffa77602a066db7d5cfb9fb5823a0895717c5a")
        .await?;

    do_pushrebase(&ctx, &repo, &Default::default(), &book, &hashset![hg_cs])
        .map_err(|err| format_err!("{err:?}"))
        .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_one_commit_transaction_hook(fb: FacebookInit) -> Result<(), Error> {
    #[derive(Copy, Clone)]
    struct Hook(RepositoryId);

    #[async_trait]
    impl PushrebaseHook for Hook {
        async fn in_critical_section(
            &self,
            _ctx: &CoreContext,
            _old_bookmark_value: Option<ChangesetId>,
        ) -> Result<Box<dyn PushrebaseCommitHook>, Error> {
            Ok(Box::new(*self) as Box<dyn PushrebaseCommitHook>)
        }
    }

    #[async_trait]
    impl PushrebaseCommitHook for Hook {
        fn post_rebase_changeset(
            &mut self,
            _bcs_old: ChangesetId,
            _bcs_new: &mut BonsaiChangesetMut,
        ) -> Result<(), Error> {
            Ok(())
        }

        async fn into_transaction_hook(
            self: Box<Self>,
            _ctx: &CoreContext,
            changesets: &RebasedChangesets,
        ) -> Result<Box<dyn PushrebaseTransactionHook>, Error> {
            let (_, (cs_id, _)) = changesets
                .iter()
                .next()
                .ok_or_else(|| Error::msg("No rebased changeset"))?;
            Ok(Box::new(TransactionHook(self.0, *cs_id)) as Box<dyn PushrebaseTransactionHook>)
        }
    }

    struct TransactionHook(RepositoryId, ChangesetId);

    #[async_trait]
    impl PushrebaseTransactionHook for TransactionHook {
        async fn populate_transaction(
            &self,
            ctx: &CoreContext,
            txn: Transaction,
        ) -> Result<Transaction, BookmarkTransactionError> {
            let key = format!("{}", self.1);

            let ret =
                SqlMutableCounters::set_counter_on_txn(ctx, self.0, &key, 1, None, txn).await?;

            match ret {
                TransactionResult::Succeeded(txn) => Ok(txn),
                TransactionResult::Failed => Err(Error::msg("Did not update").into()),
            }
        }
    }

    let ctx = CoreContext::test_mock(fb);
    let factory = TestRepoFactory::new(fb)?;
    let repo: PushrebaseTestRepo = factory.build().await?;
    Linear::init_repo(fb, &repo).await?;
    // Bottom commit of the repo
    let parents = vec!["2d7d4ba9ce0a6ffd222de7785b249ead9c51c536"];
    let bcs_id = CreateCommitContext::new(&ctx, &repo, parents)
        .add_file("file", "content")
        .commit()
        .await?;

    let bcs = bcs_id.load(&ctx, repo.repo_blobstore()).await?;

    let mut book = master_bookmark();

    bookmark(&ctx, &repo, book.clone())
        .set_to("a5ffa77602a066db7d5cfb9fb5823a0895717c5a")
        .await?;

    let hook: Box<dyn PushrebaseHook> = Box::new(Hook(repo.repo_identity().id()));
    let hooks = [hook];

    do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![bcs.clone()],
        &hooks,
        None,
    )
    .map_err(|err| format_err!("{err:?}"))
    .await?;

    let master_val = resolve_cs_id(&ctx, &repo, "master").await?;
    let key = format!("{master_val}");
    assert_eq!(
        repo.mutable_counters().get_counter(&ctx, &key).await?,
        Some(1),
    );

    // Now do the same with another non-existent bookmark,
    // make sure cs id is created.
    book = BookmarkKey::new("newbook")?;
    do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![bcs],
        &hooks,
        None,
    )
    .map_err(|err| format_err!("{err:?}"))
    .await?;

    let key = format!("{}", resolve_cs_id(&ctx, &repo, "newbook").await?);
    assert_eq!(
        repo.mutable_counters().get_counter(&ctx, &key).await?,
        Some(1),
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_stack(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    // Bottom commit of the repo
    let root = HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?;
    let p = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(&ctx, root)
        .await?
        .ok_or_else(|| Error::msg("Root is missing"))?;
    let bcs_id_1 = CreateCommitContext::new(&ctx, &repo, vec![p])
        .add_file("file", "content")
        .commit()
        .await?;
    let bcs_id_2 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_1])
        .add_file("file2", "content")
        .commit()
        .await?;

    assert_eq!(
        find_changed_files(&ctx, &repo, p, bcs_id_2).await?,
        make_paths(&["file", "file2"]),
    );

    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "a5ffa77602a066db7d5cfb9fb5823a0895717c5a",
    )
    .await?;

    let hg_cs_1 = repo.derive_hg_changeset(&ctx, bcs_id_1).await?;
    let hg_cs_2 = repo.derive_hg_changeset(&ctx, bcs_id_2).await?;
    do_pushrebase(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![hg_cs_1, hg_cs_2],
    )
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_stack_with_renames(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    // Bottom commit of the repo
    let root = HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?;
    let p = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(&ctx, root)
        .await?
        .ok_or_else(|| Error::msg("p is missing"))?;
    let bcs_id_1 = CreateCommitContext::new(&ctx, &repo, vec![p])
        .add_file("file", "content")
        .commit()
        .await?;
    let bcs_id_2 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_1])
        .add_file_with_copy_info("file_renamed", "content", (bcs_id_1, "file"))
        .commit()
        .await?;

    assert_eq!(
        find_changed_files(&ctx, &repo, p, bcs_id_2).await?,
        make_paths(&["file", "file_renamed"]),
    );

    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "a5ffa77602a066db7d5cfb9fb5823a0895717c5a",
    )
    .await?;

    let hg_cs_1 = repo.derive_hg_changeset(&ctx, bcs_id_1).await?;
    let hg_cs_2 = repo.derive_hg_changeset(&ctx, bcs_id_2).await?;
    do_pushrebase(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![hg_cs_1, hg_cs_2],
    )
    .await?;

    Ok(())
}

fn stack_rebase_flags() -> PushrebaseFlags {
    PushrebaseFlags {
        rewritedates: false,
        merge_resolution_override: MergeResolutionOverride::ForceOff,
        ..Default::default()
    }
}

#[mononoke::fbinit_test]
async fn range_diff_manifest_kind_is_knob_routed_and_equivalent(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let side = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("side", "side")
        .commit()
        .await?;
    let s1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("server_file", "server")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(&ctx, &repo, vec![s1, side])
        .add_file("merge_file", "merge")
        .commit()
        .await?;

    // Knob off: the standard entry uses HG manifests.
    let hg = find_changed_files(&ctx, &repo, root, merge).await?;
    let changesets = fetch_bonsai_range_ancestor_not_included(&ctx, &repo, root, merge).await?;
    let content = find_changed_files_with(
        &ctx,
        &repo,
        root,
        merge,
        &changesets,
        RangeDiffManifests::ContentManifest,
    )
    .await?;
    assert_eq!(
        hg, content,
        "HG and content manifests must report the same changed files"
    );
    assert!(
        !hg.is_empty(),
        "the merge-range diff must see the server and side changes"
    );

    // The override is process-global: carry the full standard map.
    override_just_knobs(JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:pushrebase_enable_merge_resolution".to_string() => KnobVal::Bool(false),
        "scm/mononoke:pushrebase_merge_resolution_derive_fsnodes".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_range_diff_use_content_manifests".to_string() => KnobVal::Bool(true),
    }));
    let via_enabled_knob = find_changed_files(&ctx, &repo, root, merge).await?;
    assert_eq!(
        via_enabled_knob, content,
        "knob on routes to content manifests"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_moves_stack_and_is_deterministic(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let stack_a = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("file_a", "a")
        .commit()
        .await?;
    let stack_b = CreateCommitContext::new(&ctx, &repo, vec![stack_a])
        .add_file("file_b", "b")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("server_file", "server")
        .commit()
        .await?;

    let rebased =
        rebase_stack_onto(&ctx, &repo, &stack_rebase_flags(), root, stack_b, onto).await?;

    assert_eq!(
        rebased.rebased_changesets.len(),
        2,
        "both stack commits should be rebased"
    );
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;

    let new_a = rebased
        .rebased_changesets
        .iter()
        .find(|pair| pair.id_old == stack_a)
        .expect("should have a mapping for the bottom commit")
        .id_new;
    let new_a_bcs = new_a.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        new_a_bcs.parents().collect::<Vec<_>>(),
        vec![onto],
        "rebased bottom commit should sit on onto"
    );
    let new_head = rebased.new_head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        new_head.parents().collect::<Vec<_>>(),
        vec![new_a],
        "rebased head should sit on the rebased bottom commit"
    );
    assert!(
        new_head
            .file_changes_map()
            .contains_key(&NonRootMPath::new("file_b")?),
        "rebased head keeps its file change"
    );

    let again = rebase_stack_onto(&ctx, &repo, &stack_rebase_flags(), root, stack_b, onto).await?;
    assert_eq!(
        again.new_head, rebased.new_head,
        "rebasing the same stack onto the same head twice should produce identical ids"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_conflict(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("conflict_file", "base")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("conflict_file", "client")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("conflict_file", "server")
        .commit()
        .await?;

    let res = rebase_stack_onto(&ctx, &repo, &stack_rebase_flags(), root, head, onto).await;
    let conflict_path = MPath::new("conflict_file")?;
    match res {
        Err(PushrebaseError::Conflicts(conflicts)) => {
            assert!(
                conflicts.iter().any(|c| c.left == conflict_path),
                "conflict should name the overlapping path, got {conflicts:?}"
            );
        }
        other => panic!("expected a conflict, got {:?}", other.map(|r| r.new_head)),
    }

    Ok(())
}

/// Stands in for an earlier land's rebase.
async fn record_rewrite(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    predecessor: ChangesetId,
    successor: ChangesetId,
) -> Result<(), Error> {
    let conn = repo.sql_bookmarks.write_connection().clone();
    let txn = conn.start_transaction(ctx.sql_query_telemetry()).await?;
    let txn = add_pushrebase_mapping(
        txn,
        &[PushrebaseMutationMappingEntry::new(
            repo.repo_identity().id(),
            predecessor,
            successor,
        )],
    )
    .await?;
    txn.commit().await?;
    Ok(())
}

fn rejection_of(res: Result<RebasedStack, PushrebaseError>) -> BaselineRejection {
    match res {
        Err(PushrebaseError::UnplaceableBaseline { rejection, .. }) => rejection,
        Err(other) => panic!("expected an unplaceable baseline, got {other:?}"),
        Ok(rebased) => panic!("expected a rejection, got {}", rebased.new_head),
    }
}

/// Baseline off the stack's history: rejected before any range is read.
#[mononoke::fbinit_test]
async fn rebase_with_baseline_rejects_a_non_ancestor_baseline(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file_base", "base")
        .commit()
        .await?;
    let unrelated = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file_other", "other")
        .commit()
        .await?;
    let stack = CreateCommitContext::new(&ctx, &repo, vec![unrelated])
        .add_file("file_stack", "one")
        .commit()
        .await?;
    let current = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file_server", "server")
        .commit()
        .await?;

    // `base` precedes the head but not the stack.
    let res = rebase_stack_onto_with_baseline(
        &ctx,
        &repo,
        &stack_rebase_flags(),
        base,
        stack,
        current,
        None,
    )
    .await;
    assert_eq!(rejection_of(res), BaselineRejection::NotAncestorOfHead);
    Ok(())
}

fn names_parent(rejection: &BaselineRejection, parent: ChangesetId) -> bool {
    matches!(rejection, BaselineRejection::ParentLandedWithChanges(cs_id) if *cs_id == parent)
}

/// Stands in for an SCM-specific source: answers the same candidates for any
/// baseline.
struct FixedCandidates(Vec<ChangesetId>);

#[async_trait]
impl BaselineCandidateSource for FixedCandidates {
    fn name(&self) -> &'static str {
        "fixed"
    }

    async fn candidates_for(
        &self,
        _ctx: &CoreContext,
        _baseline: ChangesetId,
        _onto: ChangesetId,
        _fork_point: ChangesetId,
    ) -> Result<Vec<ChangesetId>, Error> {
        Ok(self.0.clone())
    }
}

struct CandidateHop {
    v1: ChangesetId,
    child: ChangesetId,
    landed: ChangesetId,
    head: ChangesetId,
}

/// The server changed line 1 and V1 line 5; V1 landed as L with both. The
/// head then changed line 5 again, so a replay against the head cannot drop
/// V1: only a candidate proving V1 against L can place the child.
async fn candidate_hop(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
) -> Result<CandidateHop, Error> {
    let fork = CreateCommitContext::new_root(ctx, repo)
        .add_file("shared", "1\n2\n3\n4\n5\n")
        .commit()
        .await?;
    let server = CreateCommitContext::new(ctx, repo, vec![fork])
        .add_file("shared", "one\n2\n3\n4\n5\n")
        .commit()
        .await?;
    let v1 = CreateCommitContext::new(ctx, repo, vec![fork])
        .add_file("shared", "1\n2\n3\n4\nfive\n")
        .commit()
        .await?;
    let child = CreateCommitContext::new(ctx, repo, vec![v1])
        .add_file("file_child", "child")
        .commit()
        .await?;
    let landed = CreateCommitContext::new(ctx, repo, vec![server])
        .add_file("shared", "one\n2\n3\n4\nfive\n")
        .commit()
        .await?;
    let head = CreateCommitContext::new(ctx, repo, vec![landed])
        .add_file("shared", "one\n2\n3\n4\nFIVE\n")
        .commit()
        .await?;
    derive_content_manifests(ctx, repo, &[fork, landed, head]).await?;
    Ok(CandidateHop {
        v1,
        child,
        landed,
        head,
    })
}

async fn rebase_with_candidates(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    hop: &CandidateHop,
    candidates: Vec<ChangesetId>,
) -> Result<RebasedStack, PushrebaseError> {
    rebase_stack_onto_with_baseline(
        ctx,
        repo,
        &replay_flags(),
        hop.v1,
        hop.child,
        hop.head,
        Some(&FixedCandidates(candidates)),
    )
    .await
}

/// The caller's source names L; V1 drops against L, so the child lands on
/// the head with V1 as its base and only its own change.
#[mononoke::fbinit_test]
async fn rebase_with_baseline_resolves_through_a_proven_candidate(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let hop = candidate_hop(&ctx, &repo).await?;

    // Without a candidate, the replay sees V1's line conflict with the head.
    let rejection = rejection_of(rebase_with_candidates(&ctx, &repo, &hop, vec![]).await);
    assert!(names_parent(&rejection, hop.v1), "{rejection:?}");

    let rebased = rebase_with_candidates(&ctx, &repo, &hop, vec![hop.landed]).await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(
        rebased.rebased_changesets.len(),
        1,
        "only the child is rebased"
    );
    assert!(rebased.dropped.is_empty());
    let tip = rebased.new_head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(tip.parents().collect::<Vec<_>>(), vec![hop.head]);
    assert_eq!(
        tip.file_changes_map().keys().collect::<Vec<_>>(),
        vec![&NonRootMPath::new("file_child")?],
        "the child carries only its own change; the resolution is already in L"
    );
    Ok(())
}

/// Shadow evaluates the candidate but keeps today's rejection.
#[mononoke::fbinit_test]
async fn rebase_with_baseline_candidate_only_logs_in_shadow_mode(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(1);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let hop = candidate_hop(&ctx, &repo).await?;
    let res = rebase_with_candidates(&ctx, &repo, &hop, vec![hop.landed]).await;
    assert_eq!(rejection_of(res), BaselineRejection::NotOnBranch);
    Ok(())
}

/// Two candidates is not an answer; the baseline falls through to the replay.
#[mononoke::fbinit_test]
async fn rebase_with_baseline_refuses_ambiguous_candidates(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let hop = candidate_hop(&ctx, &repo).await?;
    let res = rebase_with_candidates(&ctx, &repo, &hop, vec![hop.landed, hop.head]).await;
    let rejection = rejection_of(res);
    assert!(names_parent(&rejection, hop.v1), "{rejection:?}");
    Ok(())
}

/// The source only chooses; a candidate V1 does not drop against is refused.
#[mononoke::fbinit_test]
async fn rebase_with_baseline_candidate_must_prove_the_parent_by_content(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let hop = candidate_hop(&ctx, &repo).await?;
    let res = rebase_with_candidates(&ctx, &repo, &hop, vec![hop.head]).await;
    let rejection = rejection_of(res);
    assert!(names_parent(&rejection, hop.v1), "{rejection:?}");
    Ok(())
}

/// A candidate off the branch is refused before any content is read.
#[mononoke::fbinit_test]
async fn rebase_with_baseline_refuses_an_off_branch_candidate(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let hop = candidate_hop(&ctx, &repo).await?;
    let res = rebase_with_candidates(&ctx, &repo, &hop, vec![hop.child]).await;
    let rejection = rejection_of(res);
    assert!(names_parent(&rejection, hop.v1), "{rejection:?}");
    Ok(())
}

/// A baseline an earlier land rewrote lands once the mapping says so, and is
/// rejected until then.
#[mononoke::fbinit_test]
async fn rebase_with_baseline_resolves_a_rewritten_baseline(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    // Different parents, so the two forms are distinct changesets.
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file_base", "base")
        .commit()
        .await?;
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file_server_early", "early")
        .commit()
        .await?;
    let orphan = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file_shared", "v1")
        .commit()
        .await?;
    let landed = CreateCommitContext::new(&ctx, &repo, vec![server])
        .add_file("file_shared", "v1")
        .commit()
        .await?;
    let current = CreateCommitContext::new(&ctx, &repo, vec![landed])
        .add_file("file_server", "server")
        .commit()
        .await?;
    let stack = CreateCommitContext::new(&ctx, &repo, vec![orphan])
        .add_file("file_author", "author")
        .commit()
        .await?;
    assert_ne!(orphan, landed, "the two forms must be distinct changesets");

    let res = rebase_stack_onto_with_baseline(
        &ctx,
        &repo,
        &stack_rebase_flags(),
        orphan,
        stack,
        current,
        None,
    )
    .await;
    assert_eq!(rejection_of(res), BaselineRejection::NotOnBranch);

    // Recorded twice, as a retried land would leave it: one answer, not two.
    record_rewrite(&ctx, &repo, orphan, landed).await?;
    record_rewrite(&ctx, &repo, orphan, landed).await?;

    let rebased = rebase_stack_onto_with_baseline(
        &ctx,
        &repo,
        &stack_rebase_flags(),
        orphan,
        stack,
        current,
        None,
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    let tip = rebased.new_head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        tip.parents().collect::<Vec<_>>(),
        vec![current],
        "the resolved stack must sit on the current head"
    );
    assert!(
        tip.file_changes_map()
            .contains_key(&NonRootMPath::new("file_author")?),
        "the rebased head keeps its own change"
    );
    Ok(())
}

/// The auto-rebase hop: the author built `child` on `v1`; the parent then
/// landed on the branch as `landed` (same content unless `landed_content`
/// says otherwise) under a different identity, and the branch moved on.
struct ReplayHop {
    fork: ChangesetId,
    v1: ChangesetId,
    child: ChangesetId,
    head: ChangesetId,
}

async fn replay_hop(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    landed_content: &str,
) -> Result<ReplayHop, Error> {
    let fork = CreateCommitContext::new_root(ctx, repo)
        .add_file("file_base", "base")
        .commit()
        .await?;
    let v1 = CreateCommitContext::new(ctx, repo, vec![fork])
        .add_file("file_parent", "p1")
        .commit()
        .await?;
    let child = CreateCommitContext::new(ctx, repo, vec![v1])
        .add_file("file_child", "child")
        .commit()
        .await?;
    let server = CreateCommitContext::new(ctx, repo, vec![fork])
        .add_file("file_server_early", "early")
        .commit()
        .await?;
    let landed = CreateCommitContext::new(ctx, repo, vec![server])
        .add_file("file_parent", landed_content)
        .commit()
        .await?;
    let head = CreateCommitContext::new(ctx, repo, vec![landed])
        .add_file("file_server", "server")
        .commit()
        .await?;
    derive_content_manifests(ctx, repo, &[fork, head]).await?;
    Ok(ReplayHop {
        fork,
        v1,
        child,
        head,
    })
}

fn replay_flags() -> PushrebaseFlags {
    manifest_rebase_flags(MergeResolutionOverride::UseJk)
}

/// Pure auto-rebase: the parent's landed form has the author's content, so it
/// drops and the child lands on the head with the parent's content as base.
#[mononoke::fbinit_test]
async fn replay_from_fork_lands_a_stack_whose_parent_landed_unchanged(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let hop = replay_hop(&ctx, &repo, "p1").await?;

    let rebased = rebase_stack_onto_with_baseline(
        &ctx,
        &repo,
        &replay_flags(),
        hop.v1,
        hop.child,
        hop.head,
        None,
    )
    .await?;
    assert_eq!(
        rebased.dropped,
        vec![hop.v1],
        "the parent drops as already landed"
    );
    assert_eq!(
        rebased
            .rebased_changesets
            .iter()
            .map(|pair| pair.id_old)
            .collect::<Vec<_>>(),
        vec![hop.child],
        "only the child is rewritten"
    );
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    let tip = rebased.new_head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(tip.parents().collect::<Vec<_>>(), vec![hop.head]);
    assert!(
        tip.file_changes_map()
            .contains_key(&NonRootMPath::new("file_child")?)
    );
    let _ = hop.fork;
    Ok(())
}

/// The parent landed with different content in the region the author built
/// on: rejected naming the parent, nothing landed.
#[mononoke::fbinit_test]
async fn replay_from_fork_rejects_a_parent_that_landed_with_different_content(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let hop = replay_hop(&ctx, &repo, "p2").await?;

    let res = rebase_stack_onto_with_baseline(
        &ctx,
        &repo,
        &replay_flags(),
        hop.v1,
        hop.child,
        hop.head,
        None,
    )
    .await;
    let rejection = rejection_of(res);
    assert!(names_parent(&rejection, hop.v1), "{rejection:?}");
    Ok(())
}

/// The parent's landed form dropped one of its hunks in a file the child also
/// edits. A plain replay would merge the dropped hunk back in under the child;
/// the guard rejects instead.
#[mononoke::fbinit_test]
async fn replay_from_fork_never_resurrects_a_hunk_the_parent_dropped(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let fork = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", "1\n2\n3\n4\n5\n")
        .commit()
        .await?;
    // The author's parent changed lines 1 and 5.
    let v1 = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("f", "one\n2\n3\n4\nfive\n")
        .commit()
        .await?;
    // The child changed line 3 on top of it.
    let child = CreateCommitContext::new(&ctx, &repo, vec![v1])
        .add_file("f", "one\n2\nthree\n4\nfive\n")
        .commit()
        .await?;
    // What landed kept only the line-1 change.
    let landed = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("f", "one\n2\n3\n4\n5\n")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![landed])
        .add_file("file_server", "server")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[fork, head]).await?;

    let res =
        rebase_stack_onto_with_baseline(&ctx, &repo, &replay_flags(), v1, child, head, None).await;
    let rejection = rejection_of(res);
    assert!(
        names_parent(&rejection, v1),
        "the line-5 hunk must not come back under the child: {rejection:?}"
    );
    Ok(())
}

/// Shadow mode evaluates the replay but keeps today's rejection.
#[mononoke::fbinit_test]
async fn replay_from_fork_in_shadow_mode_still_rejects(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_with_replay(1);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let hop = replay_hop(&ctx, &repo, "p1").await?;

    let res = rebase_stack_onto_with_baseline(
        &ctx,
        &repo,
        &replay_flags(),
        hop.v1,
        hop.child,
        hop.head,
        None,
    )
    .await;
    assert_eq!(rejection_of(res), BaselineRejection::NotOnBranch);
    Ok(())
}

/// A caller that forbids content merging cannot drop parents by content, so
/// it keeps today's rejection even in enforce mode.
#[mononoke::fbinit_test]
async fn replay_from_fork_skips_callers_with_merging_forced_off(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let hop = replay_hop(&ctx, &repo, "p1").await?;

    let res = rebase_stack_onto_with_baseline(
        &ctx,
        &repo,
        &stack_rebase_flags(),
        hop.v1,
        hop.child,
        hop.head,
        None,
    )
    .await;
    assert_eq!(rejection_of(res), BaselineRejection::NotOnBranch);
    Ok(())
}

/// Every commit of a multi-commit parent stack must drop.
#[mononoke::fbinit_test]
async fn replay_from_fork_drops_every_parent_of_a_multi_commit_stack(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let fork = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file_base", "base")
        .commit()
        .await?;
    let v1a = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("file_a", "a")
        .commit()
        .await?;
    let v1b = CreateCommitContext::new(&ctx, &repo, vec![v1a])
        .add_file("file_b", "b")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![v1b])
        .add_file("file_child", "child")
        .commit()
        .await?;
    let landed_a = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("file_server_early", "early")
        .add_file("file_a", "a")
        .commit()
        .await?;
    let landed_b = CreateCommitContext::new(&ctx, &repo, vec![landed_a])
        .add_file("file_b", "b")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![landed_b])
        .add_file("file_server", "server")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[fork, head]).await?;

    let rebased =
        rebase_stack_onto_with_baseline(&ctx, &repo, &replay_flags(), v1b, child, head, None)
            .await?;
    assert_eq!(rebased.dropped, vec![v1a, v1b]);
    assert_eq!(rebased.rebased_changesets.len(), 1);
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    let tip = rebased.new_head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(tip.parents().collect::<Vec<_>>(), vec![head]);
    Ok(())
}

/// A stack longer than the cap is rejected as before.
#[mononoke::fbinit_test]
async fn replay_from_fork_rejects_a_stack_longer_than_the_cap(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let fork = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file_base", "base")
        .commit()
        .await?;
    let mut parent = fork;
    let mut landed_parent = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("file_server_early", "early")
        .commit()
        .await?;
    for i in 0..REPLAY_MAX_COMMITS {
        let name = format!("file_{i}");
        parent = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .add_file(name.as_str(), "x")
            .commit()
            .await?;
        landed_parent = CreateCommitContext::new(&ctx, &repo, vec![landed_parent])
            .add_file(name.as_str(), "x")
            .commit()
            .await?;
    }
    let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
        .add_file("file_child", "child")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[fork, landed_parent]).await?;

    let res = rebase_stack_onto_with_baseline(
        &ctx,
        &repo,
        &replay_flags(),
        parent,
        child,
        landed_parent,
        None,
    )
    .await;
    assert_eq!(
        rejection_of(res),
        BaselineRejection::HistoryTooLong {
            max_commits: REPLAY_MAX_COMMITS
        },
        "the rejection names the cap the stack overran"
    );
    Ok(())
}

/// The parent's paths are untouched on a branch that has not moved: the
/// parent never landed, and the message says so rather than claiming it
/// landed with other content.
#[mononoke::fbinit_test]
async fn replay_from_fork_names_a_parent_that_never_landed(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let fork = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file_base", "base")
        .commit()
        .await?;
    let v1 = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("file_parent", "p1")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![v1])
        .add_file("file_child", "child")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[fork]).await?;

    // The branch head is still the fork point.
    let res =
        rebase_stack_onto_with_baseline(&ctx, &repo, &replay_flags(), v1, child, fork, None).await;
    assert_eq!(
        rejection_of(res),
        BaselineRejection::ParentNotLanded(v1),
        "an unlanded parent on an unmoved branch is named as not landed"
    );
    Ok(())
}

/// The branch moved, but only on paths the parent does not touch: still
/// "not landed", not "landed with different content".
#[mononoke::fbinit_test]
async fn replay_from_fork_names_a_parent_that_never_landed_on_a_moved_branch(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let fork = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file_base", "base")
        .commit()
        .await?;
    let v1 = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("file_parent", "p1")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![v1])
        .add_file("file_child", "child")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("file_server", "server")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[fork, head]).await?;

    let res =
        rebase_stack_onto_with_baseline(&ctx, &repo, &replay_flags(), v1, child, head, None).await;
    assert_eq!(
        rejection_of(res),
        BaselineRejection::ParentNotLanded(v1),
        "unrelated branch movement does not make the parent 'landed with changes'"
    );
    Ok(())
}

/// The target case: V1 drops through a three-way merge (server changed line
/// 1, V1 line 5, the landed form has both) and the child lands.
#[mononoke::fbinit_test]
async fn replay_from_fork_drops_a_parent_through_a_three_way_merge(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let hop = candidate_hop(&ctx, &repo).await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![hop.landed])
        .add_file("file_server", "server")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[head]).await?;

    let rebased = rebase_stack_onto_with_baseline(
        &ctx,
        &repo,
        &replay_flags(),
        hop.v1,
        hop.child,
        head,
        None,
    )
    .await?;
    assert_eq!(rebased.dropped, vec![hop.v1]);
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    let tip = rebased.new_head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(tip.parents().collect::<Vec<_>>(), vec![head]);
    assert_eq!(
        tip.file_changes_map().keys().collect::<Vec<_>>(),
        vec![&NonRootMPath::new("file_child")?]
    );
    Ok(())
}

/// A child whose content is already on the branch is a resubmission, not a
/// land onto someone else's commit.
#[mononoke::fbinit_test]
async fn replay_from_fork_rejects_an_already_landed_child(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let hop = replay_hop(&ctx, &repo, "p1").await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![hop.head])
        .add_file("file_child", "child")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[head]).await?;

    let res = rebase_stack_onto_with_baseline(
        &ctx,
        &repo,
        &replay_flags(),
        hop.v1,
        hop.child,
        head,
        None,
    )
    .await;
    let rejection = rejection_of(res);
    assert!(
        rejection == BaselineRejection::AlreadyLanded(hop.child),
        "{rejection:?}"
    );
    Ok(())
}

/// One of two parent commits landed: the other is named, nothing lands.
#[mononoke::fbinit_test]
async fn replay_from_fork_rejects_a_partially_landed_parent_stack(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let fork = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file_base", "base")
        .commit()
        .await?;
    let v1a = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("file_a", "a")
        .commit()
        .await?;
    let v1b = CreateCommitContext::new(&ctx, &repo, vec![v1a])
        .add_file("file_b", "b")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![v1b])
        .add_file("file_child", "child")
        .commit()
        .await?;
    let landed_a = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("file_a", "a")
        .add_file("file_server_early", "early")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![landed_a])
        .add_file("file_server", "server")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[fork, head]).await?;

    let res =
        rebase_stack_onto_with_baseline(&ctx, &repo, &replay_flags(), v1b, child, head, None).await;
    assert_eq!(
        rejection_of(res),
        BaselineRejection::ParentNotLanded(v1b),
        "the unlanded parent is named as not landed"
    );
    Ok(())
}

/// A parent that landed and was then reverted on the branch does not come
/// back under the child. Its paths are net-unchanged on the branch, so it
/// reads as not landed, which is also what re-landing it would fix.
#[mononoke::fbinit_test]
async fn replay_from_fork_rejects_a_parent_reverted_on_the_branch(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let fork = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file_base", "base")
        .commit()
        .await?;
    let v1 = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("file_parent", "p1")
        .commit()
        .await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![v1])
        .add_file("file_child", "child")
        .commit()
        .await?;
    // Distinct from V1: a different identity with the same hunk.
    let landed = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("file_parent", "p1")
        .add_file("file_server_early", "early")
        .commit()
        .await?;
    let reverted = CreateCommitContext::new(&ctx, &repo, vec![landed])
        .delete_file("file_parent")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![reverted])
        .add_file("file_server", "server")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[fork, head]).await?;

    let res =
        rebase_stack_onto_with_baseline(&ctx, &repo, &replay_flags(), v1, child, head, None).await;
    assert_eq!(
        rejection_of(res),
        BaselineRejection::ParentNotLanded(v1),
        "the reverted parent is absent from the branch, so it is named as not landed"
    );
    Ok(())
}

/// A conflict the child took part in is the child's, even on a file the
/// parent also touched.
#[mononoke::fbinit_test]
async fn replay_from_fork_reports_the_childs_own_conflict_as_a_conflict(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_with_replay(2);
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let hop = candidate_hop(&ctx, &repo).await?;
    let child = CreateCommitContext::new(&ctx, &repo, vec![hop.v1])
        .add_file("shared", "1\n2\nthree\n4\nfive\n")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![hop.landed])
        .add_file("shared", "one\n2\nTHREE\n4\nfive\n")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[head]).await?;

    let res =
        rebase_stack_onto_with_baseline(&ctx, &repo, &replay_flags(), hop.v1, child, head, None)
            .await;
    assert!(
        matches!(res, Err(PushrebaseError::Conflicts(_))),
        "the child's line-3 conflict is not the parent's: {res:?}"
    );
    Ok(())
}

/// Two distinct on-branch successors is real ambiguity; never guess.
#[mononoke::fbinit_test]
async fn rebase_with_baseline_refuses_two_distinct_on_branch_successors(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file_base", "base")
        .commit()
        .await?;
    let orphan = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file_shared", "v1")
        .commit()
        .await?;
    // Both reachable from the head.
    let landed_a = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file_a", "a")
        .commit()
        .await?;
    let landed_b = CreateCommitContext::new(&ctx, &repo, vec![landed_a])
        .add_file("file_b", "b")
        .commit()
        .await?;
    let current = CreateCommitContext::new(&ctx, &repo, vec![landed_b])
        .add_file("file_server", "server")
        .commit()
        .await?;
    let stack = CreateCommitContext::new(&ctx, &repo, vec![orphan])
        .add_file("file_author", "author")
        .commit()
        .await?;
    assert_ne!(landed_a, landed_b, "the candidates must be distinct");

    record_rewrite(&ctx, &repo, orphan, landed_a).await?;
    record_rewrite(&ctx, &repo, orphan, landed_b).await?;
    assert_eq!(
        repo.pushrebase_mutation_mapping()
            .get_successor_ids(&ctx, orphan)
            .await?
            .len(),
        2,
        "both rewrites must be on record for the ambiguity to be real"
    );

    let res = rebase_stack_onto_with_baseline(
        &ctx,
        &repo,
        &stack_rebase_flags(),
        orphan,
        stack,
        current,
        None,
    )
    .await;
    assert_eq!(rejection_of(res), BaselineRejection::NotOnBranch);
    Ok(())
}

/// A recorded successor that is not on the branch must not be trusted.
#[mononoke::fbinit_test]
async fn rebase_with_baseline_ignores_an_off_branch_successor(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file_base", "base")
        .commit()
        .await?;
    let orphan = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file_shared", "v1")
        .commit()
        .await?;
    let elsewhere = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file_elsewhere", "elsewhere")
        .commit()
        .await?;
    let current = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file_server", "server")
        .commit()
        .await?;
    let stack = CreateCommitContext::new(&ctx, &repo, vec![orphan])
        .add_file("file_author", "author")
        .commit()
        .await?;

    // Successor is not reachable from the head.
    record_rewrite(&ctx, &repo, orphan, elsewhere).await?;
    assert_eq!(
        repo.pushrebase_mutation_mapping()
            .get_successor_ids(&ctx, orphan)
            .await?,
        vec![elsewhere],
        "the off-branch rewrite must be on record for the rejection to mean anything"
    );

    let res = rebase_stack_onto_with_baseline(
        &ctx,
        &repo,
        &stack_rebase_flags(),
        orphan,
        stack,
        current,
        None,
    )
    .await;
    assert_eq!(rejection_of(res), BaselineRejection::NotOnBranch);
    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_rejects_merge_in_stack(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("d1", "d1")
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("d2", "d2")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(&ctx, &repo, vec![d1, d2])
        .add_file("m", "m")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("server_file", "server")
        .commit()
        .await?;

    let err = rebase_stack_onto(&ctx, &repo, &stack_rebase_flags(), root, merge, onto)
        .await
        .expect_err("a stack containing a merge commit should be rejected");
    assert!(
        err.to_string().contains("merge commits"),
        "unexpected error: {err}"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_rejects_non_ancestor_root(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let root = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file_a", "a")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("file_b", "b")
        .commit()
        .await?;
    // A force-moved branch: onto is NOT a descendant of root.
    let onto = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("server_file", "server")
        .commit()
        .await?;

    let err = rebase_stack_onto(&ctx, &repo, &stack_rebase_flags(), root, head, onto)
        .await
        .expect_err("a root that is not an ancestor of onto should be rejected");
    assert!(
        err.to_string().contains("must be an ancestor"),
        "unexpected error: {err}"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_with_conflict_base_lands_an_orphaned_root(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    // The shape MRL hits: the author's stack sits on `orphan`, which an earlier
    // land rewrote into `landed`. `orphan` is a sibling of `landed`, not an
    // ancestor, so it cannot bound the server range -- but it still bounds the
    // client stack and is still what the stack must be re-parented off.
    // `orphan` and `landed` carry the same change but hang off different
    // parents, which is what makes them distinct bonsais -- the same shape seen
    // in production, where the pre-rebase and landed forms of one commit
    // differed only in parent.
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("server_early", "early")
        .commit()
        .await?;
    let orphan = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("shared", "v1")
        .commit()
        .await?;
    let landed = CreateCommitContext::new(&ctx, &repo, vec![server])
        .add_file("shared", "v1")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![landed])
        .add_file("server_file", "server")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![orphan])
        .add_file("author_file", "author")
        .commit()
        .await?;
    assert_ne!(orphan, landed, "the two forms must be distinct changesets");

    // Plain rebase_stack_onto cannot express this: orphan does not precede onto.
    let err = rebase_stack_onto(&ctx, &repo, &stack_rebase_flags(), orphan, head, onto)
        .await
        .expect_err("an orphaned root should be rejected without a conflict base");
    assert!(
        err.to_string().contains("must be an ancestor"),
        "unexpected error: {err}"
    );

    // With the fork point as the conflict base it lands.
    let rebased = rebase_stack_onto_with_conflict_base(
        &ctx,
        &repo,
        &stack_rebase_flags(),
        orphan,
        base,
        head,
        onto,
    )
    .await?;

    // Only the author's own commit is replayed -- the orphan's content is
    // already on the branch as `landed` and must not be applied twice.
    assert_eq!(
        rebased.rebased_changesets.len(),
        1,
        "only the commits between orphan and head should be rebased",
    );
    assert_eq!(rebased.rebased_changesets[0].id_old, head);

    let new_head = rebased
        .rebased_bonsais
        .iter()
        .find(|bcs| bcs.get_changeset_id() == rebased.new_head)
        .expect("the new head must be among the rebased bonsais");
    assert_eq!(
        new_head.parents().collect::<Vec<_>>(),
        vec![onto],
        "the rebased stack must sit directly on onto",
    );

    Ok(())
}

/// The MRL shape: the stack sits on `orphan`, rewritten into `landed` by an
/// earlier land; `server_early` landed in between and is what the stack never
/// saw. Returns `(base, orphan, landed, onto)`; `base` is the fork point.
async fn orphaned_root_fixture(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    gap_base: &str,
    gap_server: &str,
) -> Result<(ChangesetId, ChangesetId, ChangesetId, ChangesetId), Error> {
    let base = CreateCommitContext::new_root(ctx, repo)
        .add_file("base", "base")
        .add_file("gap", gap_base)
        .add_file("blob", vec![0u8, 1, 2, 3])
        .add_file("gone", "g")
        .commit()
        .await?;
    let server_early = CreateCommitContext::new(ctx, repo, vec![base])
        .add_file("gap", gap_server)
        .commit()
        .await?;
    let orphan = CreateCommitContext::new(ctx, repo, vec![base])
        .add_file("shared", "v1")
        .add_file("blob", vec![0u8, 4, 4])
        .delete_file("gone")
        .commit()
        .await?;
    let landed = CreateCommitContext::new(ctx, repo, vec![server_early])
        .add_file("shared", "v1")
        .add_file("blob", vec![0u8, 4, 4])
        .delete_file("gone")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(ctx, repo, vec![landed])
        .add_file("server_file", "server")
        .commit()
        .await?;
    assert_ne!(orphan, landed);
    Ok((base, orphan, landed, onto))
}

async fn file_bytes(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    cs: ChangesetId,
    path: &str,
) -> Result<Vec<u8>, Error> {
    let hg = repo
        .derive_hg_changeset(ctx, cs)
        .await?
        .load(ctx, repo.repo_blobstore())
        .await?;
    let entry = hg
        .manifestid()
        .find_entry(
            ctx.clone(),
            repo.repo_blobstore().clone(),
            NonRootMPath::new(path)?.into(),
        )
        .await?
        .ok_or_else(|| format_err!("{path} should exist"))?;
    match entry {
        Entry::Leaf((_, filenode_id)) => {
            let content_id = filenode_id
                .load(ctx, repo.repo_blobstore())
                .await?
                .content_id();
            Ok(
                filestore::fetch_concat(repo.repo_blobstore(), ctx, content_id)
                    .await?
                    .to_vec(),
            )
        }
        _ => Err(format_err!("{path} should be a file")),
    }
}

async fn file_content(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    cs: ChangesetId,
    path: &str,
) -> Result<String, Error> {
    Ok(String::from_utf8(file_bytes(ctx, repo, cs, path).await?)?)
}

fn conflict_paths(err: &PushrebaseError) -> Vec<String> {
    match err {
        PushrebaseError::Conflicts(conflicts) => {
            let mut paths: Vec<_> = conflicts.iter().map(|c| c.left.to_string()).collect();
            paths.sort();
            paths
        }
        other => panic!("expected a conflict, got: {other}"),
    }
}

fn merging_flags() -> PushrebaseFlags {
    PushrebaseFlags {
        merge_resolution_override: MergeResolutionOverride::ForceOn,
        ..stack_rebase_flags()
    }
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_with_conflict_base_checks_commits_landed_before_the_rewrite(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let (base, orphan, _landed, onto) =
        orphaned_root_fixture(&ctx, &repo, "base", "server").await?;
    // The stack edits `gap`, which `server_early` edited before the rewrite.
    let head = CreateCommitContext::new(&ctx, &repo, vec![orphan])
        .add_file("gap", "author")
        .commit()
        .await?;

    let err = rebase_stack_onto_with_conflict_base(
        &ctx,
        &repo,
        &stack_rebase_flags(),
        orphan,
        base,
        head,
        onto,
    )
    .await
    .expect_err("a path changed before the rewrite must still conflict");
    assert_eq!(conflict_paths(&err), vec!["gap".to_string()]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_with_conflict_base_merges_across_the_rewrite(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let (base, orphan, _landed, onto) = orphaned_root_fixture(
        &ctx,
        &repo,
        "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n",
        "SERVER\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n",
    )
    .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![orphan])
        .add_file("gap", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nAUTHOR\n")
        .commit()
        .await?;

    let rebased = rebase_stack_onto_with_conflict_base(
        &ctx,
        &repo,
        &merging_flags(),
        orphan,
        base,
        head,
        onto,
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(
        file_content(&ctx, &repo, rebased.new_head, "gap").await?,
        "SERVER\nl2\nl3\nl4\nl5\nl6\nl7\nAUTHOR\n"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_with_conflict_base_takes_the_stack_over_its_own_landed_form(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let (base, orphan, _landed, onto) =
        orphaned_root_fixture(&ctx, &repo, "base", "server").await?;
    // `shared` and the binary `blob` were last touched by the stack's own
    // landed form; with merge resolution off that must still not conflict.
    let head = CreateCommitContext::new(&ctx, &repo, vec![orphan])
        .add_file("shared", "v2")
        .add_file("blob", vec![0u8, 9, 9, 9])
        .commit()
        .await?;

    let rebased = rebase_stack_onto_with_conflict_base(
        &ctx,
        &repo,
        &stack_rebase_flags(),
        orphan,
        base,
        head,
        onto,
    )
    .await?;
    assert_eq!(rebased.rebased_changesets.len(), 1);
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(
        file_content(&ctx, &repo, rebased.new_head, "shared").await?,
        "v2"
    );
    assert_eq!(
        file_bytes(&ctx, &repo, rebased.new_head, "blob").await?,
        vec![0u8, 9, 9, 9]
    );
    assert_eq!(
        file_content(&ctx, &repo, rebased.new_head, "server_file").await?,
        "server"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_with_conflict_base_still_rejects_a_real_binary_overlap(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let (base, orphan, landed, _onto) =
        orphaned_root_fixture(&ctx, &repo, "base", "server").await?;
    // Someone else changed the binary after the stack's landed form did.
    let onto = CreateCommitContext::new(&ctx, &repo, vec![landed])
        .add_file("blob", vec![0u8, 7, 7])
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![orphan])
        .add_file("blob", vec![0u8, 9, 9, 9])
        .commit()
        .await?;

    // With merge resolution on this still conflicts because the three
    // one-line blobs genuinely disagree, not through binary detection.
    for flags in [stack_rebase_flags(), merging_flags()] {
        let err =
            rebase_stack_onto_with_conflict_base(&ctx, &repo, &flags, orphan, base, head, onto)
                .await
                .expect_err("a binary changed on both sides cannot land");
        assert_eq!(conflict_paths(&err), vec!["blob".to_string()]);
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_with_conflict_base_merges_after_a_resolved_rewrite(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    // D1 (line 1) was merge-resolved against P (line 8) into B1', so B1'.x
    // differs from D1.x. D2 edits line 4; all three edits must survive, with
    // the merge based at D1, not at the fork point.
    let fork = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("x", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n")
        .commit()
        .await?;
    let p = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("x", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nSERVER\n")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("x", "AUTHOR1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n")
        .commit()
        .await?;
    let b1_prime = CreateCommitContext::new(&ctx, &repo, vec![p])
        .add_file("x", "AUTHOR1\nl2\nl3\nl4\nl5\nl6\nl7\nSERVER\n")
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![d1])
        .add_file("x", "AUTHOR1\nl2\nl3\nAUTHOR2\nl5\nl6\nl7\nl8\n")
        .commit()
        .await?;

    let rebased =
        rebase_stack_onto_with_conflict_base(&ctx, &repo, &merging_flags(), d1, fork, d2, b1_prime)
            .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(
        file_content(&ctx, &repo, rebased.new_head, "x").await?,
        "AUTHOR1\nl2\nl3\nAUTHOR2\nl5\nl6\nl7\nSERVER\n"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_merges_against_the_latest_server_edit(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("x", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n")
        .commit()
        .await?;
    let s1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("x", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nS1\n")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![s1])
        .add_file("x", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nS2\n")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("x", "C\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n")
        .commit()
        .await?;

    // Two server edits to one file: the merge must see the later one.
    let rebased = rebase_stack_onto(&ctx, &repo, &merging_flags(), root, head, onto).await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(
        file_content(&ctx, &repo, rebased.new_head, "x").await?,
        "C\nl2\nl3\nl4\nl5\nl6\nl7\nS2\n"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_with_conflict_base_takes_a_multi_commit_stack_over_its_own_landed_form(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let (base, orphan, _landed, onto) =
        orphaned_root_fixture(&ctx, &repo, "base", "server").await?;
    // Two new commits both touch `shared`; the cascade must apply them in order.
    let b = CreateCommitContext::new(&ctx, &repo, vec![orphan])
        .add_file("shared", "v2")
        .commit()
        .await?;
    let c = CreateCommitContext::new(&ctx, &repo, vec![b])
        .add_file("shared", "v3")
        .delete_file("blob")
        .commit()
        .await?;

    let rebased = rebase_stack_onto_with_conflict_base(
        &ctx,
        &repo,
        &stack_rebase_flags(),
        orphan,
        base,
        c,
        onto,
    )
    .await?;
    assert_eq!(rebased.rebased_changesets.len(), 2);
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(
        file_content(&ctx, &repo, rebased.new_head, "shared").await?,
        "v3"
    );
    assert!(
        file_bytes(&ctx, &repo, rebased.new_head, "blob")
            .await
            .is_err(),
        "the stack's own deletion of its landed file must apply"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_with_conflict_base_reads_the_base_from_a_deeper_landed_prefix(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    // A0 edits x line 1 and A1 does not touch x: the base is A1's view of x,
    // read from a manifest nobody derived before this land.
    let fork = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("x", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n")
        .commit()
        .await?;
    let p = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("x", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nSERVER\n")
        .commit()
        .await?;
    let a0 = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("x", "A0\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n")
        .add_file("y", "y0")
        .commit()
        .await?;
    let a1 = CreateCommitContext::new(&ctx, &repo, vec![a0])
        .add_file("other", "a1")
        .commit()
        .await?;
    let a0_prime = CreateCommitContext::new(&ctx, &repo, vec![p])
        .add_file("x", "A0\nl2\nl3\nl4\nl5\nl6\nl7\nSERVER\n")
        .add_file("y", "y0")
        .commit()
        .await?;
    let a1_prime = CreateCommitContext::new(&ctx, &repo, vec![a0_prime])
        .add_file("other", "a1")
        .commit()
        .await?;
    // `y` exists only from A0 on: a base read from the fork point instead of
    // the root would find nothing and reject this with merge resolution off.
    let head_y = CreateCommitContext::new(&ctx, &repo, vec![a1])
        .add_file("y", "y1")
        .commit()
        .await?;
    let rebased = rebase_stack_onto_with_conflict_base(
        &ctx,
        &repo,
        &stack_rebase_flags(),
        a1,
        fork,
        head_y,
        a1_prime,
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(
        file_content(&ctx, &repo, rebased.new_head, "y").await?,
        "y1"
    );

    let head_x = CreateCommitContext::new(&ctx, &repo, vec![a1])
        .add_file("x", "A0\nl2\nl3\nAUTHOR\nl5\nl6\nl7\nl8\n")
        .commit()
        .await?;
    let rebased = rebase_stack_onto_with_conflict_base(
        &ctx,
        &repo,
        &merging_flags(),
        a1,
        fork,
        head_x,
        a1_prime,
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(
        file_content(&ctx, &repo, rebased.new_head, "x").await?,
        "A0\nl2\nl3\nAUTHOR\nl5\nl6\nl7\nSERVER\n"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_with_conflict_base_rejects_an_edit_to_a_file_the_server_deleted(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let (base, orphan, landed, _onto) =
        orphaned_root_fixture(&ctx, &repo, "base", "server").await?;
    // The server deleted `shared` after the stack's landed form created it.
    let onto = CreateCommitContext::new(&ctx, &repo, vec![landed])
        .delete_file("shared")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![orphan])
        .add_file("shared", "v2")
        .commit()
        .await?;

    for flags in [stack_rebase_flags(), merging_flags()] {
        let err =
            rebase_stack_onto_with_conflict_base(&ctx, &repo, &flags, orphan, base, head, onto)
                .await
                .expect_err("editing a file the server deleted must not land silently");
        assert_eq!(conflict_paths(&err), vec!["shared".to_string()]);
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_with_conflict_base_takes_a_recreated_file_its_landed_form_deleted(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let (base, orphan, _landed, onto) =
        orphaned_root_fixture(&ctx, &repo, "base", "server").await?;
    // `gone` is absent at the root and deleted on the server by the root's
    // own landed form: re-creating it is a self-overlap, not a conflict.
    let head = CreateCommitContext::new(&ctx, &repo, vec![orphan])
        .add_file("gone", "g2")
        .commit()
        .await?;

    let rebased = rebase_stack_onto_with_conflict_base(
        &ctx,
        &repo,
        &stack_rebase_flags(),
        orphan,
        base,
        head,
        onto,
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(
        file_content(&ctx, &repo, rebased.new_head, "gone").await?,
        "g2"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_with_conflict_base_reads_the_head_when_the_range_has_a_merge(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let (base, orphan, landed, _onto) =
        orphaned_root_fixture(&ctx, &repo, "base", "server").await?;
    // A side branch adds `only_side`; the merge keeps it without recording it,
    // so server state must come from the head's manifest, not the bonsais.
    let side = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("only_side", "side")
        .commit()
        .await?;
    // The merge must record every path its parents disagree on.
    let onto = CreateCommitContext::new(&ctx, &repo, vec![landed, side])
        .add_file("gap", "server")
        .add_file("blob", vec![0u8, 4, 4])
        .delete_file("gone")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![orphan])
        .add_file("shared", "v2")
        .add_file("only_side", "author")
        .commit()
        .await?;

    let err = rebase_stack_onto_with_conflict_base(
        &ctx,
        &repo,
        &stack_rebase_flags(),
        orphan,
        base,
        head,
        onto,
    )
    .await
    .expect_err("creating a file the merge brought in must conflict");
    // `shared` is the stack's own landed content and is not reported.
    assert_eq!(conflict_paths(&err), vec!["only_side".to_string()]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_remaps_copy_info(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let stack_a = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("file", "content")
        .commit()
        .await?;
    let stack_b = CreateCommitContext::new(&ctx, &repo, vec![stack_a])
        .add_file_with_copy_info("file_renamed", "content", (stack_a, "file"))
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("server_file", "server")
        .commit()
        .await?;

    let rebased =
        rebase_stack_onto(&ctx, &repo, &stack_rebase_flags(), root, stack_b, onto).await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;

    let new_a = rebased
        .rebased_changesets
        .iter()
        .find(|pair| pair.id_old == stack_a)
        .expect("should have a mapping for the bottom commit")
        .id_new;
    let new_head = rebased.new_head.load(&ctx, repo.repo_blobstore()).await?;
    let fc = new_head
        .file_changes_map()
        .get(&NonRootMPath::new("file_renamed")?)
        .expect("rebased head should keep the renamed file");
    match fc {
        FileChange::Change(tc) => {
            assert_eq!(
                tc.copy_from().map(|(path, cs)| (path.clone(), *cs)),
                Some((NonRootMPath::new("file")?, new_a)),
                "copy_from should point at the REBASED source commit"
            );
        }
        other => panic!("expected a tracked change, got {other:?}"),
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_server_merge_range_without_hg(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    // The server range root..onto contains a merge commit whose other
    // parent lies outside the range: the range diff for it must come
    // from the derived content manifests, not HG manifests.
    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let side_root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("side", "side")
        .commit()
        .await?;
    let s1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("server_file", "server")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![s1, side_root])
        .add_file("merge_file", "merge")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("file_a", "a")
        .commit()
        .await?;

    let rebased = rebase_stack_onto(&ctx, &repo, &stack_rebase_flags(), root, head, onto).await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    let new_head = rebased.new_head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        new_head.parents().collect::<Vec<_>>(),
        vec![onto],
        "stack should land on top of the server merge"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_merge_resolution_governs_same_file_edits(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    const BASE: &str = "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\n";
    const SERVER: &str = "alpha SERVER\nbravo\ncharlie\ndelta\necho\nfoxtrot\n";
    const CLIENT: &str = "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot CLIENT\n";

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("shared.txt", BASE)
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("shared.txt", SERVER)
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("shared.txt", CLIENT)
        .commit()
        .await?;

    let merging_flags = PushrebaseFlags {
        merge_resolution_override: MergeResolutionOverride::ForceOn,
        ..stack_rebase_flags()
    };
    let rebased = rebase_stack_onto(&ctx, &repo, &merging_flags, root, head, onto).await?;

    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    let result_cs = repo
        .derive_hg_changeset(&ctx, rebased.new_head)
        .await?
        .load(&ctx, repo.repo_blobstore())
        .await?;
    let file_entry = result_cs
        .manifestid()
        .find_entry(
            ctx.clone(),
            repo.repo_blobstore().clone(),
            NonRootMPath::new("shared.txt")?.into(),
        )
        .await?
        .expect("shared.txt should exist");
    let shared = match file_entry {
        Entry::Leaf((_, filenode_id)) => {
            let content_id = filenode_id
                .load(&ctx, repo.repo_blobstore())
                .await?
                .content_id();
            let bytes = filestore::fetch_concat(repo.repo_blobstore(), &ctx, content_id).await?;
            String::from_utf8(bytes.to_vec())?
        }
        _ => panic!("shared.txt should be a file"),
    };
    assert!(
        shared.contains("alpha SERVER") && shared.contains("foxtrot CLIENT"),
        "both disjoint edits should survive the merge, got: {shared:?}"
    );

    let err = rebase_stack_onto(&ctx, &repo, &stack_rebase_flags(), root, head, onto)
        .await
        .expect_err("ForceOff keeps path-level rejection");
    assert!(
        matches!(err, PushrebaseError::Conflicts(_)),
        "unexpected error: {err}"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn rebase_stack_onto_requires_rewritedates_off(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let head = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("file_a", "a")
        .commit()
        .await?;

    let date_rewriting_flags = PushrebaseFlags {
        rewritedates: true,
        merge_resolution_override: MergeResolutionOverride::ForceOff,
        ..Default::default()
    };
    let err = rebase_stack_onto(&ctx, &repo, &date_rewriting_flags, root, head, root)
        .await
        .expect_err("date rewriting should be rejected");
    assert!(
        err.to_string().contains("rewritedates"),
        "unexpected error: {err}"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_multi_root(fb: FacebookInit) -> Result<(), Error> {
    // This test calls find_changed_files directly on a merge range,
    // which resolves the range-diff knob before do_pushrebase installs
    // the map.
    init_just_knobs_for_test();
    //
    // master -> o
    //           |
    //           :  o <- bcs3
    //           :  |
    //           :  o <- bcs2
    //           : /|
    //           |/ |
    //  root1 -> o  |
    //           |  o <- bcs1 (outside of rebase set)
    //           o /
    //           |/
    //  root0 -> o
    //
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    let config = PushrebaseFlags::default();

    let root0 = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(
            &ctx,
            HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?,
        )
        .await?
        .ok_or_else(|| Error::msg("root0 is missing"))?;

    let root1 = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(
            &ctx,
            HgChangesetId::from_str("607314ef579bd2407752361ba1b0c1729d08b281")?,
        )
        .await?
        .ok_or_else(|| Error::msg("root0 is missing"))?;

    let bcs_id_1 = CreateCommitContext::new(&ctx, &repo, vec![root0])
        .add_file("f0", "f0")
        .delete_file("files")
        .commit()
        .await?;
    let bcs_id_2 = CreateCommitContext::new(&ctx, &repo, vec![root1, bcs_id_1])
        .add_file("f1", "f1")
        .commit()
        .await?;
    let bcs_id_3 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_2])
        .add_file("f2", "f2")
        .commit()
        .await?;

    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "a5ffa77602a066db7d5cfb9fb5823a0895717c5a",
    )
    .await?;
    let bcs_id_master = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(
            &ctx,
            HgChangesetId::from_str("a5ffa77602a066db7d5cfb9fb5823a0895717c5a")?,
        )
        .await?
        .ok_or_else(|| Error::msg("bcs_id_master is missing"))?;

    let root = root1;
    assert_eq!(
        find_closest_root(
            &ctx,
            &repo,
            &config,
            &book,
            &hashmap! {root0 => ChildIndex(0), root1 => ChildIndex(0) },
        )
        .await?,
        root,
    );

    assert_eq!(
        find_changed_files(&ctx, &repo, root, bcs_id_3).await?,
        make_paths(&["f0", "f1", "f2"]),
    );

    let hg_cs_1 = repo.derive_hg_changeset(&ctx, bcs_id_1).await?;
    let hg_cs_2 = repo.derive_hg_changeset(&ctx, bcs_id_2).await?;
    let hg_cs_3 = repo.derive_hg_changeset(&ctx, bcs_id_3).await?;
    let bcs_id_rebased = do_pushrebase(
        &ctx,
        &repo,
        &config,
        &book,
        &hashset![hg_cs_1, hg_cs_2, hg_cs_3],
    )
    .await?;

    // should only rebase {bcs2, bcs3}
    let rebased =
        fetch_bonsai_range_ancestor_not_included(&ctx, &repo, bcs_id_master, bcs_id_rebased.head)
            .await?;
    assert_eq!(rebased.len(), 2);
    let bcs2 = &rebased[0];
    let bcs3 = &rebased[1];

    // bcs3 parent correctly updated and contains only {bcs2}
    assert_eq!(
        bcs3.parents().collect::<Vec<_>>(),
        vec![bcs2.get_changeset_id()]
    );

    // bcs2 parents contains old bcs1 and old master bookmark
    assert_eq!(
        bcs2.parents().collect::<HashSet<_>>(),
        hashset! { bcs_id_1, bcs_id_master },
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_conflict(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    let root = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(
            &ctx,
            HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?,
        )
        .await?
        .ok_or_else(|| Error::msg("Root is missing"))?;

    let bcs_id_1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f0", "f0")
        .commit()
        .await?;
    let bcs_id_2 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_1])
        .add_file("9/file", "file")
        .commit()
        .await?;
    let bcs_id_3 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_2])
        .add_file("f1", "f1")
        .commit()
        .await?;

    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "a5ffa77602a066db7d5cfb9fb5823a0895717c5a",
    )
    .await?;

    let hg_cs_1 = repo.derive_hg_changeset(&ctx, bcs_id_1).await?;
    let hg_cs_2 = repo.derive_hg_changeset(&ctx, bcs_id_2).await?;
    let hg_cs_3 = repo.derive_hg_changeset(&ctx, bcs_id_3).await?;
    let result = do_pushrebase(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![hg_cs_1, hg_cs_2, hg_cs_3],
    )
    .await;
    match result {
        Err(PushrebaseError::Conflicts(conflicts)) => {
            assert_eq!(
                conflicts,
                vec![PushrebaseConflict {
                    left: MPath::new("9")?,
                    right: MPath::new("9/file")?,
                },],
            );
        }
        _ => panic!("push-rebase should have failed with conflict"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_caseconflicting_rename(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    let root = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(
            &ctx,
            HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?,
        )
        .await?
        .ok_or_else(|| Error::msg("Root is missing"))?;

    let bcs_id_1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("FILE", "file")
        .commit()
        .await?;
    let bcs_id_2 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_1])
        .delete_file("FILE")
        .commit()
        .await?;
    let bcs_id_3 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_2])
        .add_file("file", "file")
        .commit()
        .await?;

    let hgcss = hashset![
        repo.derive_hg_changeset(&ctx, bcs_id_1).await?,
        repo.derive_hg_changeset(&ctx, bcs_id_2).await?,
        repo.derive_hg_changeset(&ctx, bcs_id_3).await?,
    ];

    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "a5ffa77602a066db7d5cfb9fb5823a0895717c5a",
    )
    .await?;

    do_pushrebase(&ctx, &repo, &Default::default(), &book, &hgcss).await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_caseconflicting_dirs(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    let root = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(
            &ctx,
            HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?,
        )
        .await?
        .ok_or_else(|| Error::msg("Root is missing"))?;

    let bcs_id_1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("DIR/a", "a")
        .add_file("DIR/b", "b")
        .commit()
        .await?;
    let bcs_id_2 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_1])
        .add_file("dir/a", "a")
        .delete_file("DIR/a")
        .delete_file("DIR/b")
        .commit()
        .await?;
    let hgcss = hashset![
        repo.derive_hg_changeset(&ctx, bcs_id_1).await?,
        repo.derive_hg_changeset(&ctx, bcs_id_2).await?,
    ];

    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "a5ffa77602a066db7d5cfb9fb5823a0895717c5a",
    )
    .await?;

    do_pushrebase(&ctx, &repo, &Default::default(), &book, &hgcss).await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_recursion_limit(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    let root = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(
            &ctx,
            HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?,
        )
        .await?
        .ok_or_else(|| Error::msg("Root is missing"))?;

    // create a lot of commits
    let (_, bcss) = stream::iter((0..128usize).map(Ok))
        .try_fold((root, vec![]), async |(head, mut bcss), index| {
            let file = format!("f{index}");
            let content = format!("{index}");
            let bcs = CreateCommitContext::new(&ctx, &repo, vec![head])
                .add_file(file.as_str(), content)
                .commit()
                .await?;
            bcss.push(bcs);
            Result::<_, Error>::Ok((bcs, bcss))
        })
        .await?;

    let hgcss = try_join_all(bcss.iter().map(|bcs| repo.derive_hg_changeset(&ctx, *bcs))).await?;
    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "a5ffa77602a066db7d5cfb9fb5823a0895717c5a",
    )
    .await?;
    do_pushrebase(
        &ctx,
        &repo,
        &Default::default(),
        &book.clone(),
        &hgcss.into_iter().collect(),
    )
    .await?;

    let bcs = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("file", "data")
        .commit()
        .await?;

    let hgcss = hashset![repo.derive_hg_changeset(&ctx, bcs).await?];

    // try rebase with small recursion limit
    let config = PushrebaseFlags {
        recursion_limit: Some(128),
        ..Default::default()
    };
    let result = do_pushrebase(&ctx, &repo, &config, &book, &hgcss).await;
    match result {
        Err(PushrebaseError::RootTooFarBehind) => {}
        _ => panic!("push-rebase should have failed because root too far behind"),
    }

    let config = PushrebaseFlags {
        recursion_limit: Some(256),
        ..Default::default()
    };
    do_pushrebase(&ctx, &repo, &config, &book, &hgcss).await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_rewritedates(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let (commits, _dag) = extend_from_dag_with_actions(
        &ctx,
        &repo,
        r#"
            A-B-C
               \
                D
            # author_date: D "2020-01-01 01:00:00+04:00"
            # committer: D "Committer <committer@example.test>"
            # committer_date: D "2020-01-01 09:00:00-02:00"
            # bookmark: C keep
            # bookmark: C rewrite
        "#,
    )
    .await?;

    let config = PushrebaseFlags {
        rewritedates: false,
        ..Default::default()
    };
    let source = hashset![commits["D"].load(&ctx, repo.repo_blobstore()).await?];
    let bcs_keep_date = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &config,
        &BookmarkKey::new("keep")?,
        &source,
        &[],
        None,
    )
    .await?;

    let config = PushrebaseFlags {
        rewritedates: true,
        ..Default::default()
    };
    let bcs_rewrite_date = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &config,
        &BookmarkKey::new("rewrite")?,
        &source,
        &[],
        None,
    )
    .await?;

    let bcs = commits["D"].load(&ctx, repo.repo_blobstore()).await?;
    let bcs_keep_date = bcs_keep_date.head.load(&ctx, repo.repo_blobstore()).await?;
    let bcs_rewrite_date = bcs_rewrite_date
        .head
        .load(&ctx, repo.repo_blobstore())
        .await?;

    // For the keep variant, the time should not have changed.
    assert_eq!(bcs.author_date(), bcs_keep_date.author_date());
    assert_eq!(bcs.committer_date(), bcs_keep_date.committer_date());

    // For the rewrite variant, the time should be updated.
    assert!(bcs.author_date() < bcs_rewrite_date.author_date());
    assert!(bcs.committer_date() < bcs_rewrite_date.committer_date());

    // Timezone shouldn't have changed for either author or committer.
    assert_eq!(
        bcs.author_date().tz_offset_secs(),
        bcs_rewrite_date.author_date().tz_offset_secs()
    );
    assert_eq!(
        bcs.committer_date().unwrap().tz_offset_secs(),
        bcs_rewrite_date.committer_date().unwrap().tz_offset_secs()
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_case_conflict(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = ManyFilesDirs::get_repo(fb).await;
    let root = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(
            &ctx,
            HgChangesetId::from_str("5a28e25f924a5d209b82ce0713d8d83e68982bc8")?,
        )
        .await?
        .ok_or_else(|| Error::msg("Root is missing"))?;

    let bcs = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("Dir1/file_1_in_dir1", "data")
        .commit()
        .await?;

    let hgcss = hashset![repo.derive_hg_changeset(&ctx, bcs).await?];

    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "2f866e7e549760934e31bf0420a873f65100ad63",
    )
    .await?;

    let result = do_pushrebase(&ctx, &repo, &Default::default(), &book, &hgcss).await;
    match result {
        Err(PushrebaseError::PotentialCaseConflict(conflict)) => {
            assert_eq!(conflict, NonRootMPath::new("Dir1/file_1_in_dir1")?)
        }
        _ => panic!("push-rebase should have failed with case conflict"),
    };

    // make sure that it is succeeds with disabled casefolding
    do_pushrebase(
        &ctx,
        &repo,
        &PushrebaseFlags {
            casefolding_check: false,
            ..Default::default()
        },
        &book,
        &hgcss,
    )
    .await?;

    Ok(())
}
#[mononoke::fbinit_test]

async fn pushrebase_case_conflict_exclusion(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = ManyFilesDirs::get_repo(fb).await;
    let root = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(
            &ctx,
            HgChangesetId::from_str("5a28e25f924a5d209b82ce0713d8d83e68982bc8")?,
        )
        .await?
        .ok_or_else(|| Error::msg("Root is missing"))?;

    let bcs1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("dir1/File_1_in_dir1", "data")
        .commit()
        .await?;

    let hgcs1 = hashset![repo.derive_hg_changeset(&ctx, bcs1).await?];

    let bcs2 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("dir2/File_1_in_dir2", "data")
        .commit()
        .await?;

    let hgcs2 = hashset![repo.derive_hg_changeset(&ctx, bcs2).await?];

    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "2f866e7e549760934e31bf0420a873f65100ad63",
    )
    .await?;

    let result = do_pushrebase(&ctx, &repo, &Default::default(), &book, &hgcs1).await;
    match result {
        Err(PushrebaseError::PotentialCaseConflict(conflict)) => {
            assert_eq!(conflict, NonRootMPath::new("dir1/File_1_in_dir1")?)
        }
        _ => panic!("push-rebase should have failed with case conflict"),
    };

    // make sure that it is succeeds with exclusion
    do_pushrebase(
        &ctx,
        &repo,
        &PushrebaseFlags {
            casefolding_check: true,
            casefolding_check_excluded_paths: PrefixTrie::from_iter(
                vec![Some(NonRootMPath::new("dir1")?)].into_iter(),
            ),
            ..Default::default()
        },
        &book,
        &hgcs1,
    )
    .await?;

    // revert bookmark back
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "2f866e7e549760934e31bf0420a873f65100ad63",
    )
    .await?;
    // make sure that exclusion doesn't exclude too much
    let result = do_pushrebase(
        &ctx,
        &repo,
        &PushrebaseFlags {
            casefolding_check: true,
            casefolding_check_excluded_paths: PrefixTrie::from_iter(
                vec![Some(NonRootMPath::new("dir1")?)].into_iter(),
            ),
            ..Default::default()
        },
        &book,
        &hgcs2,
    )
    .await;
    match result {
        Err(PushrebaseError::PotentialCaseConflict(conflict)) => {
            assert_eq!(conflict, NonRootMPath::new("dir2/File_1_in_dir2")?)
        }
        _ => panic!("push-rebase should have failed with case conflict"),
    };
    Ok(())
}

#[mononoke::test]
fn pushrebase_intersect_changed() -> Result<(), Error> {
    match intersect_changed_files(
        make_paths(&["a/b/c", "c", "a/b/d", "d/d", "b", "e/c"]),
        make_paths(&["d/f", "a/b/d/f", "c", "e"]),
    ) {
        Err(PushrebaseError::Conflicts(conflicts)) => assert_eq!(
            *conflicts,
            [
                PushrebaseConflict {
                    left: MPath::new("a/b/d")?,
                    right: MPath::new("a/b/d/f")?,
                },
                PushrebaseConflict {
                    left: MPath::new("c")?,
                    right: MPath::new("c")?,
                },
                PushrebaseConflict {
                    left: MPath::new("e/c")?,
                    right: MPath::new("e")?,
                },
            ]
        ),
        _ => panic!("should contain conflict"),
    };

    Ok(())
}

#[mononoke::test]
fn pushrebase_intersect_changed_with_reponame() -> Result<(), Error> {
    // Verifies intersect_changed_files detects exact path conflicts
    match intersect_changed_files(make_paths(&["a/b/c"]), make_paths(&["a/b/c"])) {
        Err(PushrebaseError::Conflicts(conflicts)) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(
                conflicts[0],
                PushrebaseConflict {
                    left: MPath::new("a/b/c")?,
                    right: MPath::new("a/b/c")?,
                }
            );
            Ok(())
        }
        _ => Err(Error::msg("expected conflict")),
    }
}

#[mononoke::fbinit_test]
async fn pushrebase_executable_bit_change(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    let path_1 = NonRootMPath::new("1")?;

    let root_hg = HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?;
    let root_cs = root_hg.load(&ctx, repo.repo_blobstore()).await?;

    let root_1_id = root_cs
        .manifestid()
        .find_entry(
            ctx.clone(),
            repo.repo_blobstore().clone(),
            path_1.clone().into(),
        )
        .await?
        .and_then(|entry| Some(entry.into_leaf()?.1))
        .ok_or_else(|| Error::msg("path_1 missing in manifest"))?;

    // crate filechange with with same content as "1" but set executable bit
    let root = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(&ctx, root_hg)
        .await?
        .ok_or_else(|| Error::msg("Root missing"))?;
    let root_bcs = root.load(&ctx, repo.repo_blobstore()).await?;
    let file_1 = match root_bcs
        .file_changes()
        .find(|(path, _)| path == &&path_1)
        .ok_or_else(|| Error::msg("path_1 missing in file_changes"))?
        .1
    {
        FileChange::Change(tc) => tc.clone(),
        _ => return Err(Error::msg("path_1 change info missing")),
    };
    assert_eq!(file_1.file_type(), FileType::Regular);
    let file_1_exec = FileChange::tracked(
        file_1.content_id(),
        FileType::Executable,
        file_1.size(),
        /* copy_from */ None,
        GitLfs::FullContent,
    );

    let bcs = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file_change(path_1.clone(), file_1_exec.clone())
        .commit()
        .await?;

    let hgcss = hashset![repo.derive_hg_changeset(&ctx, bcs).await?];

    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "a5ffa77602a066db7d5cfb9fb5823a0895717c5a",
    )
    .await?;

    let result = do_pushrebase(&ctx, &repo, &Default::default(), &book, &hgcss).await?;
    let result_bcs = result.head.load(&ctx, repo.repo_blobstore()).await?;
    let file_1_result = match result_bcs
        .file_changes()
        .find(|(path, _)| path == &&path_1)
        .ok_or_else(|| Error::msg("path_1 missing in file_changes"))?
        .1
    {
        FileChange::Change(tc) => tc.clone(),
        _ => return Err(Error::msg("path_1 change info missing")),
    };
    assert_eq!(FileChange::Change(file_1_result), file_1_exec);

    let result_hg = repo.derive_hg_changeset(&ctx, result.head).await?;
    let result_cs = result_hg.load(&ctx, repo.repo_blobstore()).await?;
    let result_1_id = result_cs
        .manifestid()
        .find_entry(
            ctx.clone(),
            repo.repo_blobstore().clone(),
            path_1.clone().into(),
        )
        .await?
        .and_then(|entry| Some(entry.into_leaf()?.1))
        .ok_or_else(|| Error::msg("path_1 missing in manifest"))?;

    // `result_1_id` should be equal to `root_1_id`, because executable flag
    // is not a part of file envelope
    assert_eq!(root_1_id, result_1_id);

    Ok(())
}

async fn count_commits_between(
    ctx: CoreContext,
    repo: &(impl Repo + BonsaiHgMappingRef),
    ancestor: HgChangesetId,
    descendant: BookmarkKey,
) -> Result<usize, Error> {
    let ancestor = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(&ctx, ancestor)
        .await?
        .ok_or_else(|| Error::msg("ancestor not found"))?;

    let descendant = repo
        .get_bookmark_hg(ctx.clone(), &descendant)
        .await?
        .ok_or_else(|| Error::msg("bookmark not found"))?;

    let descendant = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(&ctx, descendant)
        .await?
        .ok_or_else(|| Error::msg("bonsai not found"))?;

    let n = repo
        .commit_graph()
        .range_stream(&ctx, ancestor, descendant)
        .await?
        .count()
        .await;

    Ok(n)
}

#[derive(Copy, Clone)]
struct SleepHook;

#[async_trait]
impl PushrebaseHook for SleepHook {
    async fn in_critical_section(
        &self,
        _ctx: &CoreContext,
        _old_bookmark_value: Option<ChangesetId>,
    ) -> Result<Box<dyn PushrebaseCommitHook>, Error> {
        let us = rand::random_range(0..100);
        tokio::time::sleep(Duration::from_micros(us)).await;
        Ok(Box::new(*self) as Box<dyn PushrebaseCommitHook>)
    }
}

#[async_trait]
impl PushrebaseCommitHook for SleepHook {
    fn post_rebase_changeset(
        &mut self,
        _bcs_old: ChangesetId,
        _bcs_new: &mut BonsaiChangesetMut,
    ) -> Result<(), Error> {
        Ok(())
    }

    async fn into_transaction_hook(
        self: Box<Self>,
        _ctx: &CoreContext,
        _changesets: &RebasedChangesets,
    ) -> Result<Box<dyn PushrebaseTransactionHook>, Error> {
        Ok(Box::new(*self) as Box<dyn PushrebaseTransactionHook>)
    }
}

#[async_trait]
impl PushrebaseTransactionHook for SleepHook {
    async fn populate_transaction(
        &self,
        _ctx: &CoreContext,
        txn: Transaction,
    ) -> Result<Transaction, BookmarkTransactionError> {
        Ok(txn)
    }
}

#[mononoke::fbinit_test]
async fn pushrebase_simultaneously(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    // Bottom commit of the repo
    let root = HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?;
    let p = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(&ctx, root)
        .await?
        .ok_or_else(|| Error::msg("Root is missing"))?;
    let parents = vec![p];

    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "a5ffa77602a066db7d5cfb9fb5823a0895717c5a",
    )
    .await?;

    let num_pushes = 10;
    let mut futs = vec![];
    for i in 0..num_pushes {
        cloned!(ctx, repo, book);

        let hooks = [Box::new(SleepHook) as Box<dyn PushrebaseHook>];

        let f = format!("file{i}");
        let bcs_id = CreateCommitContext::new(&ctx, &repo, parents.clone())
            .add_file(f.as_str(), "content")
            .commit()
            .await?;

        let bcs = bcs_id.load(&ctx, repo.repo_blobstore()).await?;

        let fut = async move {
            do_pushrebase_bonsai(
                &ctx,
                &repo,
                &Default::default(),
                &book,
                &hashset![bcs],
                &hooks,
                None,
            )
            .await
        };

        futs.push(fut);
    }

    let res = try_join_all(futs).await?;
    let mut has_retry_num_bigger_1 = false;
    for r in res {
        if r.retry_num.0 > 1 {
            has_retry_num_bigger_1 = true;
        }
    }

    assert!(has_retry_num_bigger_1);

    let previous_master = HgChangesetId::from_str("a5ffa77602a066db7d5cfb9fb5823a0895717c5a")?;
    let commits_between = count_commits_between(ctx, &repo, previous_master, book).await?;

    // `- 1` because range_stream is inclusive
    assert_eq!(commits_between - 1, num_pushes);

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_create_new_bookmark(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    // Bottom commit of the repo
    let root = HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?;
    let p = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(&ctx, root)
        .await?
        .ok_or_else(|| Error::msg("Root is missing"))?;
    let parents = vec![p];

    let bcs_id = CreateCommitContext::new(&ctx, &repo, parents)
        .add_file("file", "content")
        .commit()
        .await?;

    let hg_cs = repo.derive_hg_changeset(&ctx, bcs_id).await?;

    let book = BookmarkKey::new("newbook")?;
    do_pushrebase(&ctx, &repo, &Default::default(), &book, &hashset![hg_cs]).await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_simultaneously_and_create_new(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    // Bottom commit of the repo
    let root = HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?;
    let p = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(&ctx, root)
        .await?
        .ok_or_else(|| Error::msg("Root is missing"))?;
    let parents = vec![p];

    let book = BookmarkKey::new("newbook")?;

    let num_pushes = 10;
    let mut futs = vec![];
    for i in 0..num_pushes {
        cloned!(ctx, repo, book);

        let hooks = [Box::new(SleepHook) as Box<dyn PushrebaseHook>];

        let f = format!("file{i}");
        let bcs_id = CreateCommitContext::new(&ctx, &repo, parents.clone())
            .add_file(f.as_str(), "content")
            .commit()
            .await?;

        let bcs = bcs_id.load(&ctx, repo.repo_blobstore()).await?;

        let fut = async move {
            do_pushrebase_bonsai(
                &ctx,
                &repo,
                &Default::default(),
                &book,
                &hashset![bcs],
                &hooks,
                None,
            )
            .await
        };

        futs.push(fut);
    }

    let res = try_join_all(futs).await?;
    let mut has_retry_num_bigger_1 = false;
    for r in res {
        if r.retry_num.0 > 1 {
            has_retry_num_bigger_1 = true;
        }
    }

    assert!(has_retry_num_bigger_1);

    let commits_between = count_commits_between(ctx, &repo, root, book).await?;
    // `- 1` because range_stream is inclusive
    assert_eq!(commits_between - 1, num_pushes);

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_one_commit_with_bundle_id(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    // Bottom commit of the repo
    let root = HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?;
    let p = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(&ctx, root)
        .await?
        .ok_or_else(|| Error::msg("Root is missing"))?;
    let parents = vec![p];

    let bcs_id = CreateCommitContext::new(&ctx, &repo, parents)
        .add_file("file", "content")
        .commit()
        .await?;
    let hg_cs = repo.derive_hg_changeset(&ctx, bcs_id).await?;

    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "a5ffa77602a066db7d5cfb9fb5823a0895717c5a",
    )
    .await?;

    do_pushrebase(&ctx, &repo, &Default::default(), &book, &hashset![hg_cs]).await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn forbid_p2_root_rebases(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;

    let root = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(
            &ctx,
            HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?,
        )
        .await?
        .ok_or_else(|| Error::msg("Root is missing"))?;

    let bcs_id_0 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("merge_file", "merge content")
        .commit()
        .await?;
    let bcs_id_1 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_0, root])
        .add_file("file", "content")
        .commit()
        .await?;
    let hgcss = hashset![
        repo.derive_hg_changeset(&ctx, bcs_id_0).await?,
        repo.derive_hg_changeset(&ctx, bcs_id_1).await?,
    ];

    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "a5ffa77602a066db7d5cfb9fb5823a0895717c5a",
    )
    .await?;

    let config_forbid_p2 = PushrebaseFlags {
        forbid_p2_root_rebases: true,
        ..Default::default()
    };

    assert!(
        do_pushrebase(&ctx, &repo, &config_forbid_p2, &book, &hgcss)
            .await
            .is_err()
    );

    let config_allow_p2 = PushrebaseFlags {
        forbid_p2_root_rebases: false,
        ..Default::default()
    };

    do_pushrebase(&ctx, &repo, &config_allow_p2, &book, &hgcss).await?;

    Ok(())
}

/// Commits of a merge of a branch into trunk, made on the branch, so that
/// trunk is the merge's second parent. Trunk is base-root, and `other`
/// branches from base. The merge of `other` and root (in that order) resolves
/// a conflict on "conflict", copies "trunk" from root and "other" from other,
/// and renames "rename_me" from root.
struct P2RootMerge {
    root: ChangesetId,
    other: ChangesetId,
    merge: ChangesetId,
}

async fn p2_root_merge(ctx: &CoreContext, repo: &PushrebaseTestRepo) -> Result<P2RootMerge, Error> {
    let base = CreateCommitContext::new_root(ctx, repo)
        .add_file("base", "base")
        .add_file("conflict", "base")
        .commit()
        .await?;
    let root = CreateCommitContext::new(ctx, repo, vec![base])
        .add_file("trunk", "trunk")
        .add_file("conflict", "trunk")
        .add_file("rename_me", "rename_me")
        .commit()
        .await?;
    let other = CreateCommitContext::new(ctx, repo, vec![base])
        .add_file("other", "other")
        .add_file("conflict", "other")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(ctx, repo, vec![other, root])
        .add_file("conflict", "resolved")
        .add_file_with_copy_info("copy_trunk", "trunk", (root, "trunk"))
        .add_file_with_copy_info("copy_other", "other", (other, "other"))
        .add_file_with_copy_info("renamed", "rename_me", (root, "rename_me"))
        .delete_file("rename_me")
        .commit()
        .await?;
    Ok(P2RootMerge { root, other, merge })
}

fn swap_flags() -> PushrebaseFlags {
    PushrebaseFlags {
        mainline_parent_first: true,
        ..Default::default()
    }
}

fn copy_source(bcs: &BonsaiChangeset, path: &str) -> Result<Option<(NonRootMPath, ChangesetId)>> {
    match bcs.file_changes_map().get(&NonRootMPath::new(path)?) {
        Some(FileChange::Change(tc)) => Ok(tc.copy_from().map(|(path, cs)| (path.clone(), *cs))),
        fc => Err(format_err!(
            "expected a tracked change to {path}, got {fc:?}"
        )),
    }
}

/// Asserts that `cs_id` carries `PARENTS_SWAPPED_EXTRA` in bonsai and in its
/// derived Mercurial changeset, and returns that changeset's id.
async fn assert_parents_swapped(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    cs_id: ChangesetId,
) -> Result<HgChangesetId, Error> {
    let bcs = cs_id.load(ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        bcs.hg_extra().collect::<Vec<_>>(),
        vec![(PARENTS_SWAPPED_EXTRA, b"1".as_slice())],
    );
    let hg_cs_id = repo.derive_hg_changeset(ctx, cs_id).await?;
    let hg_cs = hg_cs_id.load(ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        hg_cs.extra().get(PARENTS_SWAPPED_EXTRA.as_bytes()),
        Some(&Bytes::from_static(b"1")),
    );
    Ok(hg_cs_id)
}

#[mononoke::fbinit_test]
async fn mainline_parent_first(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    let P2RootMerge { root, other, merge } = p2_root_merge(&ctx, &repo).await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("onto", "onto")
        .commit()
        .await?;
    bookmark(&ctx, &repo, master_bookmark())
        .set_to(onto)
        .await?;
    let pushed = hashset![
        repo.derive_hg_changeset(&ctx, other).await?,
        repo.derive_hg_changeset(&ctx, merge).await?,
    ];
    let book = master_bookmark();

    assert!(
        do_pushrebase(&ctx, &repo, &PushrebaseFlags::default(), &book, &pushed)
            .await
            .is_err(),
        "a second-parent root is forbidden without mainline_parent_first",
    );

    let outcome = do_pushrebase(&ctx, &repo, &swap_flags(), &book, &pushed).await?;

    // Only the merge descends from root, so only the merge is rebased, with
    // the new trunk as its first parent.
    assert_eq!(outcome.rebased_changesets.len(), 1);
    assert_eq!(outcome.rebased_changesets[0].id_old, merge);
    let new_head = outcome.head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(new_head.parents().collect::<Vec<_>>(), vec![onto, other]);
    assert_parents_swapped(&ctx, &repo, outcome.head).await?;

    // Copy sources in root follow it to its replacement, the new first
    // parent; copy sources in the other side are kept.
    assert_eq!(
        copy_source(&new_head, "copy_trunk")?,
        Some((NonRootMPath::new("trunk")?, onto)),
    );
    assert_eq!(
        copy_source(&new_head, "copy_other")?,
        Some((NonRootMPath::new("other")?, other)),
    );
    assert_eq!(
        copy_source(&new_head, "renamed")?,
        Some((NonRootMPath::new("rename_me")?, onto)),
    );

    let head_hg = repo.derive_hg_changeset(&ctx, outcome.head).await?;
    ensure_content(
        &ctx,
        head_hg,
        &repo,
        btreemap! {
            "base".to_string() => "base".to_string(),
            "conflict".to_string() => "resolved".to_string(),
            "copy_other".to_string() => "other".to_string(),
            "copy_trunk".to_string() => "trunk".to_string(),
            "onto".to_string() => "onto".to_string(),
            "other".to_string() => "other".to_string(),
            "renamed".to_string() => "rename_me".to_string(),
            "trunk".to_string() => "trunk".to_string(),
        },
    )
    .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn mainline_parent_first_onto_unmoved_trunk(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    let P2RootMerge { root, other, merge } = p2_root_merge(&ctx, &repo).await?;
    bookmark(&ctx, &repo, master_bookmark())
        .set_to(root)
        .await?;
    let merge_hg = repo.derive_hg_changeset(&ctx, merge).await?;
    let pushed = hashset![repo.derive_hg_changeset(&ctx, other).await?, merge_hg];

    // Trunk has not moved and dates are kept, so only the order of the
    // parents and the extra distinguish the landed merge from the pushed one.
    let config = PushrebaseFlags {
        rewritedates: false,
        ..swap_flags()
    };
    let outcome = do_pushrebase(&ctx, &repo, &config, &master_bookmark(), &pushed).await?;

    assert_ne!(outcome.head, merge);
    let new_head = outcome.head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(new_head.parents().collect::<Vec<_>>(), vec![root, other]);
    let head_hg = assert_parents_swapped(&ctx, &repo, outcome.head).await?;
    assert_ne!(head_hg, merge_hg);
    assert_eq!(
        list_working_copy_with_types(&ctx, &repo, outcome.head).await?,
        list_working_copy_with_types(&ctx, &repo, merge).await?,
    );

    Ok(())
}

/// Paths, types and content of the files in the manifest of `hg_cs_id`.
async fn hg_working_copy(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    hg_cs_id: HgChangesetId,
) -> Result<BTreeMap<NonRootMPath, (FileType, Bytes)>, Error> {
    let blobstore = repo.repo_blobstore();
    hg_cs_id
        .load(ctx, blobstore)
        .await?
        .manifestid()
        .list_leaf_entries(ctx.clone(), blobstore.clone())
        .map_ok(|(path, (file_type, filenode_id))| async move {
            let content_id = filenode_id.load(ctx, blobstore).await?.content_id();
            let content = filestore::fetch_concat(blobstore, ctx, content_id).await?;
            anyhow::Ok((path, (file_type, content)))
        })
        .try_buffer_unordered(100)
        .try_collect()
        .await
}

/// The node of the file at `path` in the manifest of `hg_cs_id`.
async fn hg_file_node(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    hg_cs_id: HgChangesetId,
    path: &str,
) -> Result<HgFileNodeId, Error> {
    let blobstore = repo.repo_blobstore();
    match hg_cs_id
        .load(ctx, blobstore)
        .await?
        .manifestid()
        .find_entry(ctx.clone(), blobstore.clone(), MPath::new(path)?)
        .await?
    {
        Some(Entry::Leaf((_, filenode_id))) => Ok(filenode_id),
        entry => Err(format_err!(
            "expected a file at {path} in {hg_cs_id}, got {entry:?}"
        )),
    }
}

#[mononoke::fbinit_test]
async fn mainline_parent_first_derives_hg_data(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    let blobstore = repo.repo_blobstore();
    let P2RootMerge { root, other, merge } = p2_root_merge(&ctx, &repo).await?;
    bookmark(&ctx, &repo, master_bookmark())
        .set_to(root)
        .await?;
    let (root_hg, other_hg, merge_hg) = futures::try_join!(
        repo.derive_hg_changeset(&ctx, root),
        repo.derive_hg_changeset(&ctx, other),
        repo.derive_hg_changeset(&ctx, merge),
    )?;

    let config = PushrebaseFlags {
        rewritedates: false,
        ..swap_flags()
    };
    let outcome = do_pushrebase(
        &ctx,
        &repo,
        &config,
        &master_bookmark(),
        &hashset![other_hg, merge_hg],
    )
    .await?;
    let head = outcome.head;

    let head_hg = repo.derive_hg_changeset(&ctx, head).await?;
    let filenodes = repo
        .repo_derived_data()
        .derive::<FilenodesOnlyPublic>(&ctx, head, DerivationPriority::LOW)
        .await?;
    assert!(
        matches!(
            filenodes,
            FilenodesOnlyPublic::Present {
                root_filenode: Some(_)
            }
        ),
        "unexpected filenodes: {filenodes:?}",
    );
    repo.repo_derived_data()
        .derive::<RootHgAugmentedManifestId>(&ctx, head, DerivationPriority::LOW)
        .await?;
    assert_eq!(
        hg_working_copy(&ctx, &repo, head_hg).await?,
        hg_working_copy(&ctx, &repo, merge_hg).await?,
    );

    // The changesets differ, in the order of their parents and in the extra.
    let (head_cs, merge_cs) = futures::try_join!(
        head_hg.load(&ctx, blobstore),
        merge_hg.load(&ctx, blobstore),
    )?;
    assert_ne!(head_hg, merge_hg);
    assert_eq!(
        (head_cs.p1(), head_cs.p2()),
        (
            Some(root_hg.into_nodehash()),
            Some(other_hg.into_nodehash())
        ),
    );
    assert_eq!(
        (merge_cs.p1(), merge_cs.p2()),
        (
            Some(other_hg.into_nodehash()),
            Some(root_hg.into_nodehash())
        ),
    );

    // Tree and file node ids do not depend on the order of their parents, so
    // the landed merge shares its root tree, and the node of the file resolved
    // in the merge, with the pushed merge. The stored node, and the filenode
    // derived from it, keep the parents in the pushed order.
    assert_eq!(head_cs.manifestid(), merge_cs.manifestid());
    let resolved = hg_file_node(&ctx, &repo, head_hg, "conflict").await?;
    assert_eq!(
        resolved,
        hg_file_node(&ctx, &repo, merge_hg, "conflict").await?
    );
    let pushed_parents = (
        Some(hg_file_node(&ctx, &repo, other_hg, "conflict").await?),
        Some(hg_file_node(&ctx, &repo, root_hg, "conflict").await?),
    );
    assert_eq!(
        resolved.load(&ctx, blobstore).await?.parents(),
        pushed_parents
    );
    let filenode = repo
        .repo_derived_data()
        .manager()
        .filenodes()?
        .get_filenode(&ctx, &RepoPath::file("conflict")?, resolved)
        .await?
        .do_not_handle_disabled_filenodes()?
        .ok_or_else(|| format_err!("no filenode for conflict"))?;
    assert_eq!((filenode.p1, filenode.p2), pushed_parents);
    assert_eq!(filenode.linknode, head_hg);

    Ok(())
}

#[mononoke::fbinit_test]
async fn mainline_parent_first_rejects_merge_with_extra(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let root = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("trunk", "trunk")
        .commit()
        .await?;
    let other = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("other", "other")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(&ctx, &repo, vec![other, root])
        .add_extra(PARENTS_SWAPPED_EXTRA, "1")
        .commit()
        .await?;
    bookmark(&ctx, &repo, master_bookmark())
        .set_to(root)
        .await?;
    let pushed = hashset![
        repo.derive_hg_changeset(&ctx, other).await?,
        repo.derive_hg_changeset(&ctx, merge).await?,
    ];
    let config = PushrebaseFlags {
        rewritedates: false,
        ..swap_flags()
    };

    let err = do_pushrebase(&ctx, &repo, &config, &master_bookmark(), &pushed)
        .await
        .expect_err("the extra could not distinguish the landed merge");
    assert!(
        format!("{err:#}").contains("already has the parents_swapped extra"),
        "unexpected error: {err:#}",
    );

    Ok(())
}

/// Trunk is base-root-onto, with `master` at onto. `other` branches from base,
/// `stack` from root, and the draft merge of `other` and `stack` (in that
/// order) descends from root through its second parent.
async fn merge_with_rebased_p2(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
) -> Result<(ChangesetId, ChangesetId, ChangesetId), Error> {
    let base = CreateCommitContext::new_root(ctx, repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let root = CreateCommitContext::new(ctx, repo, vec![base])
        .add_file("trunk", "trunk")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(ctx, repo, vec![root])
        .add_file("onto", "onto")
        .commit()
        .await?;
    let other = CreateCommitContext::new(ctx, repo, vec![base])
        .add_file("other", "other")
        .commit()
        .await?;
    let stack = CreateCommitContext::new(ctx, repo, vec![root])
        .add_file("stack", "stack")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(ctx, repo, vec![other, stack])
        .commit()
        .await?;
    bookmark(ctx, repo, master_bookmark()).set_to(onto).await?;
    Ok((other, stack, merge))
}

#[mononoke::fbinit_test]
async fn mainline_parent_first_through_rebased_parent(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    for mainline_parent_first in [false, true] {
        let repo: PushrebaseTestRepo = test_repo_factory::build_empty(ctx.fb).await?;
        let (other, stack, merge) = merge_with_rebased_p2(&ctx, &repo).await?;
        let pushed = hashset![
            repo.derive_hg_changeset(&ctx, other).await?,
            repo.derive_hg_changeset(&ctx, stack).await?,
            repo.derive_hg_changeset(&ctx, merge).await?,
        ];
        let config = PushrebaseFlags {
            mainline_parent_first,
            ..Default::default()
        };
        let outcome = do_pushrebase(&ctx, &repo, &config, &master_bookmark(), &pushed).await?;

        let new_stack = outcome
            .rebased_changesets
            .iter()
            .find(|pair| pair.id_old == stack)
            .expect("stack descends from root, so it is rebased")
            .id_new;
        let new_head = outcome.head.load(&ctx, repo.repo_blobstore()).await?;
        if mainline_parent_first {
            assert_eq!(
                new_head.parents().collect::<Vec<_>>(),
                vec![new_stack, other]
            );
            assert_parents_swapped(&ctx, &repo, outcome.head).await?;
        } else {
            assert_eq!(
                new_head.parents().collect::<Vec<_>>(),
                vec![other, new_stack]
            );
            assert_eq!(new_head.hg_extra().count(), 0);
            repo.derive_hg_changeset(&ctx, outcome.head).await?;
        }
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn mainline_parent_first_keeps_merges_on_trunk(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("onto", "onto")
        .commit()
        .await?;
    let side = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("side", "side")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(&ctx, &repo, vec![base, side])
        .add_file("merge", "merge")
        .commit()
        .await?;
    bookmark(&ctx, &repo, master_bookmark())
        .set_to(onto)
        .await?;
    let pushed = hashset![
        repo.derive_hg_changeset(&ctx, side).await?,
        repo.derive_hg_changeset(&ctx, merge).await?,
    ];

    // The merge's first parent is the root, so its order is kept and it gets
    // no extra.
    let outcome = do_pushrebase(&ctx, &repo, &swap_flags(), &master_bookmark(), &pushed).await?;
    let new_side = outcome
        .rebased_changesets
        .iter()
        .find(|pair| pair.id_old == side)
        .expect("side descends from the root, so it is rebased")
        .id_new;
    let new_head = outcome.head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(new_head.parents().collect::<Vec<_>>(), vec![onto, new_side]);
    assert_eq!(new_head.hg_extra().count(), 0);

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_over_merge(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(ctx.fb).await?;

    let p1 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("p1", "some content")
        .commit()
        .await?;

    let p2 = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("p2", "some content")
        .commit()
        .await?;

    let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2])
        .add_file("merge", "some content")
        .commit()
        .await?;

    let book = master_bookmark();

    let merge_hg_cs_id = repo.derive_hg_changeset(&ctx, merge).await?;

    set_bookmark(ctx.clone(), &repo, &book, &{
        // https://github.com/rust-lang/rust/pull/64856
        let r = format!("{merge_hg_cs_id}");
        r
    })
    .await?;

    // Modify a file touched in another branch - should fail
    push_and_verify(
        &ctx,
        &repo,
        p1,
        &book,
        btreemap! {"p2" => Some("some content")},
        false,
    )
    .await?;

    // Modify a file modified in th merge commit - should fail
    push_and_verify(
        &ctx,
        &repo,
        p1,
        &book,
        btreemap! {"merge" => Some("some content")},
        false,
    )
    .await?;

    // Any other files should succeed
    push_and_verify(
        &ctx,
        &repo,
        p1,
        &book,
        btreemap! {"p1" => Some("some content")},
        true,
    )
    .await?;

    push_and_verify(
        &ctx,
        &repo,
        p1,
        &book,
        btreemap! {"otherfile" => Some("some content")},
        true,
    )
    .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_over_merge_even(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = MergeEven::get_repo(fb).await;

    // 4dcf230cd2f20577cb3e88ba52b73b376a2b3f69 - is a merge commit,
    // 3cda5c78aa35f0f5b09780d971197b51cad4613a is one of the ancestors
    let root = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(
            &ctx,
            HgChangesetId::from_str("3cda5c78aa35f0f5b09780d971197b51cad4613a")?,
        )
        .await?
        .ok_or_else(|| Error::msg("Root is missing"))?;

    // Modifies the same file "branch" - pushrebase should fail because of conflicts
    let bcs_id_should_fail = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("branch", "some content")
        .commit()
        .await?;

    let bcs_id_should_succeed = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("randomfile", "some content")
        .commit()
        .await?;

    let book = master_bookmark();

    let hgcss = hashset![repo.derive_hg_changeset(&ctx, bcs_id_should_fail).await?];

    let res = do_pushrebase(&ctx, &repo, &PushrebaseFlags::default(), &book, &hgcss).await;

    should_have_conflicts(res);
    let hgcss = hashset![
        repo.derive_hg_changeset(&ctx, bcs_id_should_succeed)
            .await?,
    ];

    do_pushrebase(&ctx, &repo, &PushrebaseFlags::default(), &book, &hgcss).await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_of_branch_merge(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(ctx.fb).await?;

    // Pushrebase two branch merges (bcs_id_first_merge and bcs_id_second_merge)
    // on top of master
    let bcs_id_base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;

    let bcs_id_p1 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_base])
        .add_file("p1", "p1")
        .commit()
        .await?;

    let bcs_id_p2 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_base])
        .add_file("p2", "p2")
        .commit()
        .await?;

    let bcs_id_first_merge = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_p1, bcs_id_p2])
        .add_file("merge", "merge")
        .commit()
        .await?;

    let bcs_id_second_merge =
        CreateCommitContext::new(&ctx, &repo, vec![bcs_id_first_merge, bcs_id_p2])
            .add_file("merge2", "merge")
            .commit()
            .await?;

    // Modify base file again
    let bcs_id_master = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_p1])
        .add_file("base", "base2")
        .commit()
        .await?;

    let hg_cs = repo.derive_hg_changeset(&ctx, bcs_id_master).await?;

    let book = master_bookmark();
    set_bookmark(ctx.clone(), &repo, &book, &{
        // https://github.com/rust-lang/rust/pull/64856
        let r = format!("{hg_cs}");
        r
    })
    .await?;

    let hgcss = hashset![
        repo.derive_hg_changeset(&ctx, bcs_id_first_merge).await?,
        repo.derive_hg_changeset(&ctx, bcs_id_second_merge).await?,
    ];

    do_pushrebase(&ctx, &repo, &PushrebaseFlags::default(), &book, &hgcss).await?;

    let new_master = get_bookmark_value(&ctx, &repo, &BookmarkKey::new("master")?)
        .await?
        .ok_or_else(|| Error::msg("master not set"))?;

    let master_hg = repo.derive_hg_changeset(&ctx, new_master).await?;

    ensure_content(
        &ctx,
        master_hg,
        &repo,
        btreemap! {
                "base".to_string()=> "base2".to_string(),
                "merge".to_string()=> "merge".to_string(),
                "merge2".to_string()=> "merge".to_string(),
                "p1".to_string()=> "p1".to_string(),
                "p2".to_string()=> "p2".to_string(),
        },
    )
    .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_of_branch_merge_with_removal(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(ctx.fb).await?;

    // Pushrebase two branch merges (bcs_id_first_merge and bcs_id_second_merge)
    // on top of master
    let bcs_id_base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;

    let bcs_id_p1 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_base])
        .add_file("p1", "p1")
        .commit()
        .await?;

    let bcs_id_p2 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_base])
        .add_file("p2", "p2")
        .commit()
        .await?;

    let bcs_id_merge = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_p1, bcs_id_p2])
        .add_file("merge", "merge")
        .commit()
        .await?;

    // Modify base file again
    let bcs_id_master = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_p1])
        .delete_file("base")
        .add_file("anotherfile", "anotherfile")
        .commit()
        .await?;

    let hg_cs = repo.derive_hg_changeset(&ctx, bcs_id_master).await?;

    let book = master_bookmark();
    set_bookmark(ctx.clone(), &repo, &book, &{
        // https://github.com/rust-lang/rust/pull/64856
        let r = format!("{hg_cs}");
        r
    })
    .await?;

    let hgcss = hashset![repo.derive_hg_changeset(&ctx, bcs_id_merge).await?,];

    do_pushrebase(&ctx, &repo, &PushrebaseFlags::default(), &book, &hgcss).await?;

    let new_master = get_bookmark_value(&ctx, &repo, &BookmarkKey::new("master")?)
        .await?
        .ok_or_else(|| Error::msg("master not set"))?;

    let master_hg = repo.derive_hg_changeset(&ctx, new_master).await?;

    ensure_content(
        &ctx,
        master_hg,
        &repo,
        btreemap! {
                "anotherfile".to_string() => "anotherfile".to_string(),
                "merge".to_string()=> "merge".to_string(),
                "p1".to_string()=> "p1".to_string(),
                "p2".to_string()=> "p2".to_string(),
        },
    )
    .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_of_branch_merge_with_rename(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(ctx.fb).await?;

    // Pushrebase two branch merges (bcs_id_first_merge and bcs_id_second_merge)
    // on top of master
    let bcs_id_base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;

    let bcs_id_p1 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_base])
        .add_file("p1", "p1")
        .commit()
        .await?;

    let bcs_id_p2 = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_base])
        .add_file("p2", "p2")
        .commit()
        .await?;

    let bcs_id_merge = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_p1, bcs_id_p2])
        .add_file("merge", "merge")
        .commit()
        .await?;

    // Remove base file
    let bcs_id_pre_pre_master = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_p1])
        .delete_file("base")
        .commit()
        .await?;

    // Move to base file
    let bcs_id_pre_master = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_pre_pre_master])
        .add_file_with_copy_info("base", "somecontent", (bcs_id_pre_pre_master, "p1"))
        .commit()
        .await?;

    let bcs_id_master = CreateCommitContext::new(&ctx, &repo, vec![bcs_id_pre_master])
        .add_file("somefile", "somecontent")
        .commit()
        .await?;

    let hg_cs = repo.derive_hg_changeset(&ctx, bcs_id_master).await?;

    let book = master_bookmark();
    set_bookmark(ctx.clone(), &repo, &book, &{
        // https://github.com/rust-lang/rust/pull/64856
        let r = format!("{hg_cs}");
        r
    })
    .await?;

    let hgcss = hashset![repo.derive_hg_changeset(&ctx, bcs_id_merge).await?];

    do_pushrebase(&ctx, &repo, &PushrebaseFlags::default(), &book, &hgcss).await?;

    let new_master = get_bookmark_value(&ctx, &repo.clone(), &BookmarkKey::new("master")?)
        .await?
        .ok_or_else(|| Error::msg("master is not set"))?;

    let master_hg = repo.derive_hg_changeset(&ctx, new_master).await?;

    ensure_content(
        &ctx,
        master_hg,
        &repo,
        btreemap! {
                "base".to_string() => "somecontent".to_string(),
                "somefile".to_string() => "somecontent".to_string(),
                "merge".to_string()=> "merge".to_string(),
                "p1".to_string()=> "p1".to_string(),
                "p2".to_string()=> "p2".to_string(),
        },
    )
    .await?;

    Ok(())
}

/// Counts, and once `deny` is set fails, every read of an Hg manifest blob,
/// standing in for a store where they have been deleted.
#[derive(Debug, Default)]
struct HgManifestReads {
    deny: AtomicBool,
    denied: AtomicUsize,
}

impl SamplingHandler for HgManifestReads {
    fn sample_get(
        &self,
        _ctx: &CoreContext,
        key: &str,
        _value: Option<&BlobstoreGetData>,
    ) -> Result<(), Error> {
        if self.deny.load(Ordering::Relaxed) && key.contains("hgmanifest.sha1.") {
            self.denied.fetch_add(1, Ordering::Relaxed);
            return Err(format_err!("Hg manifest read denied: {key}"));
        }
        Ok(())
    }
}

fn init_just_knobs_for_augmented_read_test(read_from_augmented: bool) {
    init_just_knobs_for_test();
    override_just_knobs(JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:hgmanifest_read_from_augmented".to_string() => KnobVal::Bool(read_from_augmented),
        // The range diff of the pushed merge reads the same manifests as the merge fix-up.
        "scm/mononoke:pushrebase_range_diff_use_content_manifests".to_string() => KnobVal::Bool(false),
        // Tests choose which commits have augmented manifests.
        "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset".to_string() => KnobVal::Bool(false),
    }));
}

struct BranchMerge {
    repo: PushrebaseTestRepo,
    hg_reads: Arc<HgManifestReads>,
    merge: BonsaiChangeset,
    onto: ChangesetId,
}

/// A merge of `p1` and `p2` pushed onto a master that changed `base` after
/// `p1`: landing it must carry master's `base` into the merge, because `p2`
/// still has the old one. `p1`, `p2`, the merge (as uploaded) and master have
/// Hg manifests, and augmented manifests unless `augmented_for_onto` is false,
/// which leaves master without one.
async fn branch_merge(ctx: &CoreContext, augmented_for_onto: bool) -> Result<BranchMerge, Error> {
    let hg_reads = Arc::new(HgManifestReads::default());
    let repo: PushrebaseTestRepo = TestRepoFactory::new(ctx.fb)?
        .with_blobstore(Arc::new(SamplingBlobstore::new(
            Memblob::default(),
            hg_reads.clone(),
        )))
        .build()
        .await?;

    let base = CreateCommitContext::new_root(ctx, &repo)
        .add_file("base", "base")
        .add_file("dir/kept", "kept")
        .commit()
        .await?;
    let p1 = CreateCommitContext::new(ctx, &repo, vec![base])
        .add_file("p1", "p1")
        .commit()
        .await?;
    let p2 = CreateCommitContext::new(ctx, &repo, vec![base])
        .add_file("p2", "p2")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(ctx, &repo, vec![p1, p2])
        .add_file("merge", "merge")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(ctx, &repo, vec![p1])
        .add_file("base", "base2")
        .add_file("dir/new", "new")
        .commit()
        .await?;
    bookmark(ctx, &repo, "master").set_to(onto).await?;

    let with_augmented = if augmented_for_onto {
        vec![p1, p2, merge, onto]
    } else {
        vec![p1, p2, merge]
    };
    for cs_id in [p1, p2, merge, onto] {
        repo.derive_hg_changeset(ctx, cs_id).await?;
    }
    for cs_id in with_augmented {
        repo.repo_derived_data()
            .derive::<RootHgAugmentedManifestId>(ctx, cs_id, DerivationPriority::LOW)
            .await?;
    }

    let merge = merge.load(ctx, repo.repo_blobstore()).await?;
    Ok(BranchMerge {
        repo,
        hg_reads,
        merge,
        onto,
    })
}

async fn pushrebase_branch_merge(
    ctx: &CoreContext,
    branch_merge: &BranchMerge,
    deny_hg_reads: bool,
) -> Result<PushrebaseOutcome, PushrebaseError> {
    branch_merge
        .hg_reads
        .deny
        .store(deny_hg_reads, Ordering::Relaxed);
    do_pushrebase_bonsai(
        ctx,
        &branch_merge.repo,
        &PushrebaseFlags::default(),
        &master_bookmark(),
        &hashset![branch_merge.merge.clone()],
        &[],
        None,
    )
    .await
}

async fn landed_file_changes(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    outcome: &PushrebaseOutcome,
) -> Result<Vec<(NonRootMPath, FileChange)>, Error> {
    let landed = outcome.head.load(ctx, repo.repo_blobstore()).await?;
    Ok(landed
        .file_changes()
        .map(|(path, change)| (path.clone(), change.clone()))
        .collect())
}

#[mononoke::fbinit_test]
async fn pushrebase_of_branch_merge_reads_augmented_manifests(
    fb: FacebookInit,
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    init_just_knobs_for_augmented_read_test(false);

    // Given: the same merge in two repos.
    let from_hg = branch_merge(&ctx, true).await?;
    let from_augmented = branch_merge(&ctx, true).await?;

    // When: one lands it reading Hg manifests (Hg reads allowed), the other
    // reading augmented manifests with every Hg manifest read failing.
    let from_hg_outcome = pushrebase_branch_merge(&ctx, &from_hg, false)
        .map_err(|err| format_err!("{err:?}"))
        .await?;
    init_just_knobs_for_augmented_read_test(true);
    let from_augmented_outcome = pushrebase_branch_merge(&ctx, &from_augmented, true)
        .map_err(|err| format_err!("{err:?}"))
        .await?;

    // Then: both land the same merge, which carries master's `base`, and the
    // augmented landing read no Hg manifest.
    let expected = landed_file_changes(&ctx, &from_hg.repo, &from_hg_outcome).await?;
    let landed = landed_file_changes(&ctx, &from_augmented.repo, &from_augmented_outcome).await?;
    assert_eq!(landed, expected);
    let landed_paths: Vec<_> = landed.iter().map(|(path, _)| path.to_string()).collect();
    assert_eq!(
        landed_paths,
        vec!["base", "merge"],
        "the merge should carry master's `base` because `p2` has the old one"
    );
    assert_eq!(from_augmented.hg_reads.denied.load(Ordering::Relaxed), 0);

    from_augmented.hg_reads.deny.store(false, Ordering::Relaxed);
    let master_hg = from_augmented
        .repo
        .derive_hg_changeset(&ctx, from_augmented_outcome.head)
        .await?;
    ensure_content(
        &ctx,
        master_hg,
        &from_augmented.repo,
        btreemap! {
            "base".to_string() => "base2".to_string(),
            "dir/kept".to_string() => "kept".to_string(),
            "dir/new".to_string() => "new".to_string(),
            "merge".to_string() => "merge".to_string(),
            "p1".to_string() => "p1".to_string(),
            "p2".to_string() => "p2".to_string(),
        },
    )
    .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_of_branch_merge_reads_hg_manifests_with_knob_off(
    fb: FacebookInit,
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    init_just_knobs_for_augmented_read_test(false);
    let branch_merge = branch_merge(&ctx, true).await?;

    let result = pushrebase_branch_merge(&ctx, &branch_merge, true).await;

    assert!(
        result.is_err(),
        "with the knob off, landing should read Hg manifests and fail when they are gone"
    );
    assert!(branch_merge.hg_reads.denied.load(Ordering::Relaxed) > 0);
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_of_branch_merge_reports_missing_augmented_manifest(
    fb: FacebookInit,
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    init_just_knobs_for_augmented_read_test(true);
    let branch_merge = branch_merge(&ctx, false).await?;
    let onto_root = branch_merge
        .repo
        .derive_hg_changeset(&ctx, branch_merge.onto)
        .await?
        .load(&ctx, branch_merge.repo.repo_blobstore())
        .await?
        .manifestid();

    let err = match pushrebase_branch_merge(&ctx, &branch_merge, true).await {
        Err(PushrebaseError::Error(err)) => err,
        other => {
            return Err(format_err!(
                "expected a missing manifest error, got {other:?}"
            ));
        }
    };

    let missing_key = HgAugmentedManifestId::new(onto_root.into_nodehash()).blobstore_key();
    assert!(
        err.chain().any(|cause| matches!(
            cause.downcast_ref::<LoadableError>(),
            Some(LoadableError::Missing(key)) if key == &missing_key
        )),
        "expected master's augmented manifest to be reported missing, got {err:?}"
    );
    assert_eq!(branch_merge.hg_reads.denied.load(Ordering::Relaxed), 0);
    assert_eq!(
        get_bookmark_value(&ctx, &branch_merge.repo, &master_bookmark()).await?,
        Some(branch_merge.onto),
        "master should not move"
    );
    Ok(())
}

/// Upload a root commit with no files, as commit upload does: it sends no root
/// tree, so the commit has the null manifest id.
async fn upload_commit_without_files(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
) -> Result<ChangesetId, Error> {
    let (bonsai, _) = CreateChangeset {
        expected_nodeid: None,
        expected_files: Some(Vec::new()),
        p1: None,
        p2: None,
        subtree_changes: None,
        root_manifest: future::ok(None).boxed(),
        sub_entries: stream::empty().boxed(),
        cs_metadata: ChangesetMetadata {
            user: "author <author@fb.com>".into(),
            time: DateTime::from_timestamp(0, 0)?,
            extra: Default::default(),
            message: "commit without files".into(),
        },
        upload_to_blobstore_only: false,
        acl_file_validation: None,
    }
    .create(
        ctx.clone(),
        repo,
        repo.restricted_paths_arc().config_based().clone(),
        None,
        MononokeScubaSampleBuilder::with_discard(),
    )
    .get_completed_changeset()
    .await?;
    Ok(bonsai.get_changeset_id())
}

async fn sorted_bonsai_diff(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    ancestor: ChangesetId,
    descendant: ChangesetId,
) -> Result<Vec<BonsaiDiffFileChange<(FileType, HgFileNodeId)>>, Error> {
    let mut diff: Vec<_> = find_bonsai_diff(ctx, repo, ancestor, descendant)
        .await?
        .try_collect()
        .await?;
    diff.sort();
    Ok(diff)
}

#[mononoke::fbinit_test]
async fn find_bonsai_diff_across_null_manifest_reads_augmented_manifests(
    fb: FacebookInit,
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    init_just_knobs_for_augmented_read_test(false);
    let hg_reads = Arc::new(HgManifestReads::default());
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?
        .with_blobstore(Arc::new(SamplingBlobstore::new(
            Memblob::default(),
            hg_reads.clone(),
        )))
        .build()
        .await?;
    let empty = upload_commit_without_files(&ctx, &repo).await?;
    let files = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("a", "a")
        .add_file("dir/b", "b")
        .commit()
        .await?;
    repo.derive_hg_changeset(&ctx, files).await?;
    repo.repo_derived_data()
        .derive::<RootHgAugmentedManifestId>(&ctx, files, DerivationPriority::LOW)
        .await?;
    let empty_mf = repo
        .derive_hg_changeset(&ctx, empty)
        .await?
        .load(&ctx, repo.repo_blobstore())
        .await?
        .manifestid();
    assert_eq!(empty_mf, HgManifestId::new(NULL_HASH));

    // Given: the diffs from and to the commit without files, from Hg manifests.
    let added = sorted_bonsai_diff(&ctx, &repo, empty, files).await?;
    let deleted = sorted_bonsai_diff(&ctx, &repo, files, empty).await?;
    let added_paths: Vec<_> = added
        .iter()
        .map(|change| match change {
            BonsaiDiffFileChange::Changed(path, _) => Ok(path.to_string()),
            other => Err(format_err!("expected an added file, got {other:?}")),
        })
        .collect::<Result<_, _>>()?;
    assert_eq!(added_paths, vec!["a", "dir/b"]);
    assert_eq!(
        deleted,
        vec![
            BonsaiDiffFileChange::Deleted(NonRootMPath::new("a")?),
            BonsaiDiffFileChange::Deleted(NonRootMPath::new("dir/b")?),
        ]
    );

    // When: every Hg manifest read fails and the knob is on.
    hg_reads.deny.store(true, Ordering::Relaxed);
    init_just_knobs_for_augmented_read_test(true);

    // Then: augmented manifests give the same diffs, and no Hg manifest is
    // read.
    assert_eq!(sorted_bonsai_diff(&ctx, &repo, empty, files).await?, added);
    assert_eq!(
        sorted_bonsai_diff(&ctx, &repo, files, empty).await?,
        deleted
    );
    assert_eq!(hg_reads.denied.load(Ordering::Relaxed), 0);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_pushrebase_new_repo_merge_no_new_file_changes(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;

    // First commit in the new repo
    let other_first_commit = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("otherrepofile", "otherrepocontent")
        .commit()
        .await?;

    let bcs_id = CreateCommitContext::new_root(&ctx, &repo)
        // Bottom commit of the main repo
        .add_parent("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")
        .add_parent(other_first_commit)
        .commit()
        .await?;

    let hg_cs = repo.derive_hg_changeset(&ctx, bcs_id).await?;

    let result = do_pushrebase(
        &ctx,
        &repo,
        &Default::default(),
        &master_bookmark(),
        &hashset![hg_cs],
    )
    .map_err(|err| format_err!("{err:?}"))
    .await?;

    let bcs = result.head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(bcs.file_changes().collect::<Vec<_>>(), vec![]);

    let master_hg = repo.derive_hg_changeset(&ctx, result.head).await?;

    ensure_content(
        &ctx,
        master_hg,
        &repo,
        btreemap! {
                "1".to_string()=> "1\n".to_string(),
                "2".to_string()=> "2\n".to_string(),
                "3".to_string()=> "3\n".to_string(),
                "4".to_string()=> "4\n".to_string(),
                "5".to_string()=> "5\n".to_string(),
                "6".to_string()=> "6\n".to_string(),
                "7".to_string()=> "7\n".to_string(),
                "8".to_string()=> "8\n".to_string(),
                "9".to_string()=> "9\n".to_string(),
                "10".to_string()=> "modified10\n".to_string(),

                "files".to_string()=> "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n".to_string(),
                "otherrepofile".to_string()=> "otherrepocontent".to_string(),
        },
    )
    .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_test_failpushrebase_extra(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;

    // Create one commit on top of latest commit in the linear repo
    let before_head_commit = "79a13814c5ce7330173ec04d279bf95ab3f652fb";
    let head_bcs_id = CreateCommitContext::new(&ctx, &repo, vec![before_head_commit])
        .add_file("file", "content")
        .add_extra(FAIL_PUSHREBASE_EXTRA.to_string(), vec![])
        .commit()
        .await?;

    bookmark(&ctx, &repo, "head").set_to(head_bcs_id).await?;

    let bcs_id = CreateCommitContext::new(&ctx, &repo, vec![before_head_commit])
        .add_file("file", "content2")
        .commit()
        .await?;

    let hg_cs = repo.derive_hg_changeset(&ctx, bcs_id).await?;

    let err = do_pushrebase(
        &ctx,
        &repo,
        &Default::default(),
        &BookmarkKey::new("head")?,
        &hashset![hg_cs],
    )
    .await;

    match err {
        Err(PushrebaseError::ForceFailPushrebase(_)) => {}
        _ => {
            return Err(format_err!(
                "unexpected result: expected ForceFailPushrebase error, found {err:?}"
            ));
        }
    };

    // Now create the same commit on top of head commit - pushrebase should succeed
    let bcs_id = CreateCommitContext::new(&ctx, &repo, vec![head_bcs_id])
        .add_file("file", "content2")
        .commit()
        .await?;

    let hg_cs = repo.derive_hg_changeset(&ctx, bcs_id).await?;

    do_pushrebase(
        &ctx,
        &repo,
        &Default::default(),
        &BookmarkKey::new("head")?,
        &hashset![hg_cs],
    )
    .map_err(|err| format_err!("{err:?}"))
    .await?;

    Ok(())
}

async fn ensure_content(
    ctx: &CoreContext,
    hg_cs_id: HgChangesetId,
    repo: &impl Repo,
    expected: BTreeMap<String, String>,
) -> Result<(), Error> {
    let cs = hg_cs_id.load(ctx, repo.repo_blobstore()).await?;

    let entries = cs
        .manifestid()
        .list_all_entries(ctx.clone(), repo.repo_blobstore().clone())
        .try_collect::<Vec<_>>()
        .await?;

    let mut actual = BTreeMap::new();
    for (path, entry) in entries {
        match entry {
            Entry::Leaf((_, filenode_id)) => {
                let store = repo.repo_blobstore();
                let content_id = filenode_id.load(ctx, store).await?.content_id();
                let content = filestore::fetch_concat(store, ctx, content_id).await?;

                let s = String::from_utf8_lossy(content.as_ref()).into_owned();
                actual.insert(
                    format!("{}", Option::<NonRootMPath>::from(path).unwrap()),
                    s,
                );
            }
            Entry::Tree(_) => {}
        }
    }

    assert_eq!(expected, actual);

    Ok(())
}

#[mononoke::fbinit_test]
async fn batched_pushrebase_two_stacks(fb: FacebookInit) -> Result<(), Error> {
    #[derive(Copy, Clone)]
    struct Hook(RepositoryId);

    #[async_trait]
    impl PushrebaseHook for Hook {
        async fn in_critical_section(
            &self,
            _ctx: &CoreContext,
            _old_bookmark_value: Option<ChangesetId>,
        ) -> Result<Box<dyn PushrebaseCommitHook>, Error> {
            Ok(Box::new(*self) as Box<dyn PushrebaseCommitHook>)
        }
    }

    #[async_trait]
    impl PushrebaseCommitHook for Hook {
        fn post_rebase_changeset(
            &mut self,
            _bcs_old: ChangesetId,
            _bcs_new: &mut BonsaiChangesetMut,
        ) -> Result<(), Error> {
            Ok(())
        }

        async fn into_transaction_hook(
            self: Box<Self>,
            _ctx: &CoreContext,
            changesets: &RebasedChangesets,
        ) -> Result<Box<dyn PushrebaseTransactionHook>, Error> {
            Ok(Box::new(TransactionHook(self.0, changesets.len()))
                as Box<dyn PushrebaseTransactionHook>)
        }
    }

    struct TransactionHook(RepositoryId, usize);

    #[async_trait]
    impl PushrebaseTransactionHook for TransactionHook {
        async fn populate_transaction(
            &self,
            ctx: &CoreContext,
            txn: Transaction,
        ) -> Result<Transaction, BookmarkTransactionError> {
            let ret = SqlMutableCounters::set_counter_on_txn(
                ctx,
                self.0,
                "batched_hook_changesets",
                self.1 as i64,
                None,
                txn,
            )
            .await?;

            match ret {
                TransactionResult::Succeeded(txn) => Ok(txn),
                TransactionResult::Failed => Err(Error::msg("Did not update").into()),
            }
        }
    }

    let ctx = CoreContext::test_mock(fb);
    let factory = TestRepoFactory::new(fb)?;
    let repo: PushrebaseTestRepo = factory.build().await?;
    let (commits, _dag) = Linear::init_repo(fb, &repo).await?;

    let master_cs = commits["K"];
    let root_bcs_id = commits["A"];
    let root_bcs_id_b = commits["B"];

    // Stack A: adds "fileA"
    let bcs_id_a = CreateCommitContext::new(&ctx, &repo, vec![root_bcs_id])
        .add_file("fileA", "content_a")
        .commit()
        .await?;
    let bcs_a = bcs_id_a.load(&ctx, repo.repo_blobstore()).await?;

    // Stack B: adds "fileB"
    let bcs_id_b = CreateCommitContext::new(&ctx, &repo, vec![root_bcs_id_b])
        .add_file("fileB", "content_b")
        .commit()
        .await?;
    let bcs_b = bcs_id_b.load(&ctx, repo.repo_blobstore()).await?;

    // Index both stacks
    let bookmark = master_bookmark();
    let config = PushrebaseFlags::default();
    let stack_a =
        index_pushrebase_request(&ctx, &repo, &config, &bookmark, &hashset![bcs_a]).await?;
    let stack_b =
        index_pushrebase_request(&ctx, &repo, &config, &bookmark, &hashset![bcs_b]).await?;

    // Build PushrebaseRequests with oneshot channels
    let (tx_a, rx_a) = oneshot::channel();
    let (tx_b, rx_b) = oneshot::channel();

    let hooks = vec![Box::new(Hook(repo.repo_identity().id())) as Box<dyn PushrebaseHook>];
    let req_a = queued_pushrebase_request(&ctx, stack_a, tx_a);
    let req_b = queued_pushrebase_request(&ctx, stack_b, tx_b);

    // Call do_batched_pushrebase
    let requeued =
        do_batched_pushrebase(&ctx, &repo, &config, &bookmark, vec![req_a, req_b], &hooks).await;

    // No CAS failures expected
    assert!(requeued.is_empty(), "Expected no re-queued requests");

    // Both receivers should get Ok outcomes
    let outcome_a = rx_a.await.unwrap().map_err(|e| format_err!("{e:?}"))?;
    let outcome_b = rx_b.await.unwrap().map_err(|e| format_err!("{e:?}"))?;

    // outcome_a sees the original bookmark value (it was rebased first)
    assert_eq!(outcome_a.old_bookmark_value, Some(master_cs));
    // outcome_b sees the head after A was rebased (running_head before B's rebase)
    assert_eq!(outcome_b.old_bookmark_value, Some(outcome_a.head));

    // Both should have one rebased changeset
    assert_eq!(outcome_a.rebased_changesets.len(), 1);
    assert_eq!(outcome_b.rebased_changesets.len(), 1);

    // Pushrebase distance should be 10 (A to K in the Linear fixture)
    assert_eq!(outcome_a.pushrebase_distance.0, 10);
    assert_eq!(outcome_b.pushrebase_distance.0, 9);

    // The final bookmark should point to outcome_b's head (second stack lands on top of first)
    let new_master = resolve_cs_id(&ctx, &repo, "master").await?;
    assert_eq!(new_master, outcome_b.head);

    // Verify the hook fired: the transaction hook should have written the total
    // number of rebased changesets (2, one per stack) to the mutable counter
    assert_eq!(
        repo.mutable_counters()
            .get_counter(&ctx, "batched_hook_changesets")
            .await?,
        Some(2),
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn batched_pushrebase_one_conflict(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let (repo, commits, _dag): (PushrebaseTestRepo, _, _) = Linear::get_repo_and_dag(fb).await;

    let root_bcs_id = commits["A"];

    // Stack A: adds a new file (no conflict)
    let bcs_id_a = CreateCommitContext::new(&ctx, &repo, vec![root_bcs_id])
        .add_file("new_file", "content")
        .commit()
        .await?;
    let bcs_a = bcs_id_a.load(&ctx, repo.repo_blobstore()).await?;

    // Stack B: modifies "files" which is also modified by commits B-K (conflict)
    let bcs_id_b = CreateCommitContext::new(&ctx, &repo, vec![root_bcs_id])
        .add_file("files", "conflicting content")
        .commit()
        .await?;
    let bcs_b = bcs_id_b.load(&ctx, repo.repo_blobstore()).await?;

    // Index both stacks
    let bookmark = master_bookmark();
    let config = PushrebaseFlags::default();
    let stack_a =
        index_pushrebase_request(&ctx, &repo, &config, &bookmark, &hashset![bcs_a]).await?;
    let stack_b =
        index_pushrebase_request(&ctx, &repo, &config, &bookmark, &hashset![bcs_b]).await?;

    let (tx_a, rx_a) = oneshot::channel();
    let (tx_b, rx_b) = oneshot::channel();

    let req_a = queued_pushrebase_request(&ctx, stack_a, tx_a);
    let req_b = queued_pushrebase_request(&ctx, stack_b, tx_b);

    let requeued =
        do_batched_pushrebase(&ctx, &repo, &config, &bookmark, vec![req_a, req_b], &[]).await;
    assert!(requeued.is_empty(), "Expected no re-queued requests");

    // Stack A should succeed
    let outcome_a = rx_a.await.unwrap().map_err(|e| format_err!("{e:?}"))?;
    assert_eq!(outcome_a.rebased_changesets.len(), 1);

    // Stack B should fail with conflicts
    let result_b = rx_b.await.unwrap();
    assert!(result_b.is_err(), "Expected stack B to fail with conflicts");
    match result_b.unwrap_err().inner() {
        PushrebaseError::Conflicts(_) => {}
        other => panic!("Expected Conflicts error, got: {other:?}"),
    }

    // Bookmark should still be updated (stack A succeeded)
    let new_master = resolve_cs_id(&ctx, &repo, "master").await?;
    assert_eq!(new_master, outcome_a.head);

    Ok(())
}

/// Verify that a rebase failure in one request cannot corrupt hook
/// state that flows into the bookmark transaction for other requests.
///
/// Setup: three requests [A, B, C] in one batch.  The shared commit
/// hook records an assignment for every changeset it sees (mimicking
/// globalrev), then fails on request B's changeset — *after* recording
/// the phantom assignment.  With the bug, the loop would continue to
/// C, and `into_transaction_hook` would fire with 3 recorded
/// assignments but only 2 entries in `all_rebased_changesets`.  A hook
/// without its own count-check (or one that uses the recorded count
/// for sequencing) would write corrupt data to the transaction.
///
/// The fix aborts the batch on the first `create_rebased_changesets`
/// failure, so `into_transaction_hook` is never reached and no data
/// is written.
#[mononoke::fbinit_test]
async fn batched_pushrebase_rebase_failure_prevents_corrupt_hook_data(
    fb: FacebookInit,
) -> Result<(), Error> {
    #[derive(Clone)]
    struct TrackingHook(RepositoryId);

    /// Commit hook that records one assignment per changeset it sees,
    /// then fails on the Nth call.  The assignment is recorded
    /// *before* the error check — this is the state contamination.
    struct TrackingCommitHook {
        repo_id: RepositoryId,
        assignments: usize,
        fail_on_call: usize,
    }

    #[async_trait]
    impl PushrebaseHook for TrackingHook {
        async fn in_critical_section(
            &self,
            _ctx: &CoreContext,
            _old_bookmark_value: Option<ChangesetId>,
        ) -> Result<Box<dyn PushrebaseCommitHook>, Error> {
            Ok(Box::new(TrackingCommitHook {
                repo_id: self.0,
                assignments: 0,
                // Requests are processed in order: A(call 1), B(call 2), C(call 3).
                // Fail on call 2 (request B) after recording its assignment.
                fail_on_call: 2,
            }))
        }
    }

    #[async_trait]
    impl PushrebaseCommitHook for TrackingCommitHook {
        fn post_rebase_changeset(
            &mut self,
            _bcs_old: ChangesetId,
            _bcs_new: &mut BonsaiChangesetMut,
        ) -> Result<(), Error> {
            // Record the assignment FIRST (mirrors globalrev hook which
            // calls set_on_changeset + insert + increment before returning).
            self.assignments += 1;
            if self.assignments == self.fail_on_call {
                return Err(anyhow::anyhow!("simulated rebase failure"));
            }
            Ok(())
        }

        async fn into_transaction_hook(
            self: Box<Self>,
            ctx: &CoreContext,
            _changesets: &RebasedChangesets,
        ) -> Result<Box<dyn PushrebaseTransactionHook>, Error> {
            // Write the number of recorded assignments to a mutable
            // counter.  If hook state was contaminated by the failed
            // request, this value is WRONG — it includes a phantom
            // assignment for a changeset that was never committed.
            Ok(Box::new(WriteCountHook {
                repo_id: self.repo_id,
                count: self.assignments,
                ctx: ctx.clone(),
            }))
        }
    }

    struct WriteCountHook {
        repo_id: RepositoryId,
        count: usize,
        ctx: CoreContext,
    }

    #[async_trait]
    impl PushrebaseTransactionHook for WriteCountHook {
        async fn populate_transaction(
            &self,
            _ctx: &CoreContext,
            txn: Transaction,
        ) -> Result<Transaction, BookmarkTransactionError> {
            let ret = SqlMutableCounters::set_counter_on_txn(
                &self.ctx,
                self.repo_id,
                "hook_assignments",
                self.count as i64,
                None,
                txn,
            )
            .await?;
            match ret {
                TransactionResult::Succeeded(txn) => Ok(txn),
                TransactionResult::Failed => Err(Error::msg("counter write failed").into()),
            }
        }
    }

    let ctx = CoreContext::test_mock(fb);
    let factory = TestRepoFactory::new(fb)?;
    let repo: PushrebaseTestRepo = factory.build().await?;
    let (commits, _dag) = Linear::init_repo(fb, &repo).await?;

    let root = commits["A"];
    let bookmark = master_bookmark();
    let config = PushrebaseFlags::default();
    let hooks = vec![Box::new(TrackingHook(repo.repo_identity().id())) as Box<dyn PushrebaseHook>];

    // Three non-conflicting stacks, each with one changeset.
    let mut requests = Vec::new();
    let mut receivers = Vec::new();
    for name in ["fileA", "fileB", "fileC"] {
        let bcs_id = CreateCommitContext::new(&ctx, &repo, vec![root])
            .add_file(name, "content")
            .commit()
            .await?;
        let bcs = bcs_id.load(&ctx, repo.repo_blobstore()).await?;
        let stack =
            index_pushrebase_request(&ctx, &repo, &config, &bookmark, &hashset![bcs]).await?;
        let (tx, rx) = oneshot::channel();
        requests.push(queued_pushrebase_request(&ctx, stack, tx));
        receivers.push(rx);
    }

    let requeued = do_batched_pushrebase(&ctx, &repo, &config, &bookmark, requests, &hooks).await;

    // Request B (index 1) should have received the hook error.
    let result_b = receivers.remove(1).await.unwrap();
    assert!(result_b.is_err(), "Request B should have failed");

    // Requests A and C should be requeued — NOT resolved with
    // corrupt hook data flowing into the transaction.
    assert_eq!(requeued.len(), 2, "Requests A and C should be requeued");

    // The transaction hook must NOT have fired.  If it did, it would
    // have written a count of 3 (including the phantom assignment
    // from failed request B) — that's data corruption.
    assert_eq!(
        repo.mutable_counters()
            .get_counter(&ctx, "hook_assignments")
            .await?,
        None,
        "into_transaction_hook should not have been reached",
    );

    Ok(())
}

fn should_have_conflicts(res: Result<PushrebaseOutcome, PushrebaseError>) {
    match res {
        Err(err) => match err {
            PushrebaseError::Conflicts(_) => {}
            _ => {
                panic!("pushrebase should have had conflicts");
            }
        },
        Ok(_) => {
            panic!("pushrebase should have failed");
        }
    }
}

fn init_just_knobs_for_merge_test() {
    override_just_knobs(JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:pushrebase_enable_merge_resolution".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_merge_resolution_derive_fsnodes".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_range_diff_use_content_manifests".to_string() => KnobVal::Bool(false),
    }));
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_clean(fb: FacebookInit) -> Result<(), Error> {
    // Test: server and client modify different parts of the same file.
    // With merge resolution enabled, pushrebase should succeed and
    // the resulting file should contain both modifications.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    // Create a base commit with a multi-line file
    let base_content = "line1\nline2\nline3\nline4\nline5\n";
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", base_content)
        .commit()
        .await?;

    // Server-side commit: modify the first line
    let server_content = "modified_line1\nline2\nline3\nline4\nline5\n";
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", server_content)
        .commit()
        .await?;

    // Set bookmark to the server commit
    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;

    // Client-side commit (based on base, not server): modify the last line
    let client_content = "line1\nline2\nline3\nline4\nmodified_line5\n";
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", client_content)
        .commit()
        .await?;

    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    // Enable merge resolution
    init_just_knobs_for_merge_test();

    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await?;

    // Verify the merged content has both modifications
    let result_hg = repo.derive_hg_changeset(&ctx, result.head).await?;
    let expected_content = "modified_line1\nline2\nline3\nline4\nmodified_line5\n";
    ensure_content(
        &ctx,
        result_hg,
        &repo,
        btreemap! {
            "file.txt".to_string() => expected_content.to_string(),
        },
    )
    .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_identical_lfs_is_already_applied(
    fb: FacebookInit,
) -> Result<(), Error> {
    // Both sides updated an LFS-tracked blob to the same bytes while editing
    // different lines of a text file. The identical blob is no merge at all,
    // so the LFS rule must not block the text merge.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let blob = NonRootMPath::new("blob.bin")?;
    let lfs = GitLfs::canonical_pointer();

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", "line1\nline2\nline3\nline4\nline5\n")
        .add_file_with_type_and_lfs("blob.bin", "payload v1", FileType::Regular, lfs)
        .commit()
        .await?;

    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", "modified_line1\nline2\nline3\nline4\nline5\n")
        .add_file_with_type_and_lfs("blob.bin", "payload v2", FileType::Regular, lfs)
        .commit()
        .await?;
    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;

    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", "line1\nline2\nline3\nline4\nmodified_line5\n")
        .add_file_with_type_and_lfs("blob.bin", "payload v2", FileType::Regular, lfs)
        .commit()
        .await?;
    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_merge_test();
    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await?;

    let result_hg = repo.derive_hg_changeset(&ctx, result.head).await?;
    ensure_content(
        &ctx,
        result_hg,
        &repo,
        btreemap! {
            "file.txt".to_string() => "modified_line1\nline2\nline3\nline4\nmodified_line5\n".to_string(),
            "blob.bin".to_string() => "payload v2".to_string(),
        },
    )
    .await?;
    let result_bcs = result.head.load(&ctx, repo.repo_blobstore()).await?;
    match result_bcs.file_changes_map().get(&blob) {
        Some(FileChange::Change(tc)) => assert!(
            tc.git_lfs().is_lfs_pointer(),
            "the identical blob is carried through untouched, still as a pointer"
        ),
        other => panic!("unexpected blob.bin change after rebase: {other:?}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_identical_lfs_alone_follows_noop_policy(
    fb: FacebookInit,
) -> Result<(), Error> {
    // A commit whose only change re-lands the server's LFS bytes is the same
    // duplicate-content case as a text file: rejected on that path when the
    // noop knob is on.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let lfs = GitLfs::canonical_pointer();

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file_with_type_and_lfs("blob.bin", "payload v1", FileType::Regular, lfs)
        .commit()
        .await?;
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file_with_type_and_lfs("blob.bin", "payload v2", FileType::Regular, lfs)
        .commit()
        .await?;
    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file_with_type_and_lfs("blob.bin", "payload v2", FileType::Regular, lfs)
        .commit()
        .await?;
    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_noop_rejection_test(true);
    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await;
    match result {
        Err(PushrebaseError::Conflicts(conflicts)) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(format!("{}", conflicts[0].left), "blob.bin");
        }
        other => panic!("Expected Conflicts error, got: {other:?}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_differing_lfs_still_conflicts(
    fb: FacebookInit,
) -> Result<(), Error> {
    // The identical-content early-exit must fire ONLY when both sides carry
    // the same content id. When the same LFS path lands different bytes on
    // each side there is nothing identical to short-circuit, so the LFS rule
    // still refuses the merge and the push conflicts as it did before.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let lfs = GitLfs::canonical_pointer();

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file_with_type_and_lfs("blob.bin", "payload v1", FileType::Regular, lfs)
        .commit()
        .await?;
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file_with_type_and_lfs("blob.bin", "payload server", FileType::Regular, lfs)
        .commit()
        .await?;
    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file_with_type_and_lfs("blob.bin", "payload client", FileType::Regular, lfs)
        .commit()
        .await?;
    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_merge_test();
    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await;

    should_have_conflicts(result);

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_conflict(fb: FacebookInit) -> Result<(), Error> {
    // Test: server and client modify the SAME line of a file.
    // Even with merge resolution enabled, pushrebase should fail
    // because the merge has a true content-level conflict.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    // Create a base commit with a multi-line file
    let base_content = "line1\nline2\nline3\n";
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", base_content)
        .commit()
        .await?;

    // Server-side commit: modify line 2
    let server_content = "line1\nserver_modified\nline3\n";
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", server_content)
        .commit()
        .await?;

    // Set bookmark to the server commit
    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;

    // Client-side commit (based on base): also modify line 2 differently
    let client_content = "line1\nclient_modified\nline3\n";
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", client_content)
        .commit()
        .await?;

    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    // Enable merge resolution
    init_just_knobs_for_merge_test();

    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await;

    // Should fail with conflicts because of overlapping edits
    should_have_conflicts(result);

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_carry_forward_on_retry(fb: FacebookInit) -> Result<(), Error> {
    // Test: On CAS retry, attempt 2 uses a narrow range (S1→S2) that
    // does NOT contain the original conflict. The carried MergedFileInfo
    // from attempt 1 is used via reconciliation.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    let base_content = "\
line 1
line 2
line 3
line 4
line 5
line 6
line 7
line 8
";
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", base_content)
        .commit()
        .await?;

    // Server commit S1: adds "line 2.1" between line 2 and line 3
    let s1_content = "\
line 1
line 2
line 2.1
line 3
line 4
line 5
line 6
line 7
line 8
";
    let s1 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", s1_content)
        .commit()
        .await?;

    // Server commit S2: unrelated change (different file) — simulates
    // another push that moves the bookmark after S1
    let s2 = CreateCommitContext::new(&ctx, &repo, vec![s1])
        .add_file("unrelated.txt", "unrelated change\n")
        .commit()
        .await?;

    // Set bookmark to S2 (after both server commits)
    let book = BookmarkKey::new("master")?;
    let hg_s2 = repo.derive_hg_changeset(&ctx, s2).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_s2}")).await?;

    // Client commit: adds "line 6.1" between line 6 and line 7
    // (based on base, NOT on server commits)
    let client_content = "\
line 1
line 2
line 3
line 4
line 5
line 6
line 6.1
line 7
line 8
";
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", client_content)
        .commit()
        .await?;

    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_merge_test();

    let client_cf = find_changed_files(&ctx, &repo, base, client).await?;

    // --- Attempt 1: full range base → S1 (first bookmark position) ---
    let result1 = check_pushrebase_conflicts(
        &ctx,
        &repo,
        &Default::default(),
        base,
        base,
        s1,
        std::slice::from_ref(&client_bcs),
        &client_cf,
    )
    .await?;
    assert!(
        result1.merged_file_overrides.is_some(),
        "Attempt 1 should detect the conflict and produce merge overrides"
    );

    // --- Attempt 2: narrow range S1 → S2 (only the delta after CAS fail) ---
    // S2 only changes unrelated.txt, so no conflict with client's file.txt
    let result2 = check_pushrebase_conflicts(
        &ctx,
        &repo,
        &Default::default(),
        base,
        s1,
        s2,
        std::slice::from_ref(&client_bcs),
        &client_cf,
    )
    .await?;
    assert!(
        result2.merged_file_overrides.is_none(),
        "Narrow range S1→S2 should have no conflicts (unrelated.txt only)"
    );

    // Simulate carry-forward: attempt 1 produces overrides, CAS fails,
    // attempt 2 sees no new conflicts but carried info is used.
    let carried = result1.merged_file_overrides.clone().unwrap();
    let reconciled = match result2.merged_file_overrides {
        Some(ref delta) => reconcile_merge_file_info(&carried, delta),
        None => carried,
    };

    // Rebase with the reconciled (carried) overrides
    let rebased_set = fetch_bonsai_range_ancestor_not_included(&ctx, &repo, base, client).await?;
    let (new_head, _, rebased_bonsais) = create_rebased_changesets(
        &ctx,
        &repo,
        &Default::default(),
        &rebased_set,
        base,
        client,
        s2,
        &mut [],
        Some(reconciled),
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased_bonsais).await?;

    // Check the file content at the rebased head
    let result_hg = repo.derive_hg_changeset(&ctx, new_head).await?;
    let result_cs = result_hg.load(&ctx, repo.repo_blobstore()).await?;
    let manifest = result_cs.manifestid();
    let file_path = NonRootMPath::new("file.txt")?;
    let file_entry = manifest
        .find_entry(ctx.clone(), repo.repo_blobstore().clone(), file_path.into())
        .await?
        .expect("file.txt should exist");

    let file_content = match file_entry {
        Entry::Leaf((_, filenode_id)) => {
            let content_id = filenode_id
                .load(&ctx, repo.repo_blobstore())
                .await?
                .content_id();
            let bytes = filestore::fetch_concat(repo.repo_blobstore(), &ctx, content_id).await?;
            String::from_utf8(bytes.to_vec())?
        }
        _ => panic!("file.txt should be a file"),
    };

    // Both changes are preserved via carry-forward
    assert!(
        file_content.contains("line 6.1"),
        "rebased commit should have client's line 6.1"
    );
    assert!(
        file_content.contains("line 2.1"),
        "rebased commit should have server's line 2.1. Actual:\n{file_content}",
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_carry_forward_with_new_server_changes(
    fb: FacebookInit,
) -> Result<(), Error> {
    // Test: When the same file is changed in both base→S1 and S1→S2,
    // the carry-forward reconciliation updates server_content_id from
    // the delta so the rebase uses the latest server content.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("a.txt", "a1\na2\na3\na4\na5\na6\na7\na8\n")
        .add_file("b.txt", "b1\nb2\nb3\nb4\nb5\nb6\nb7\nb8\n")
        .commit()
        .await?;

    // Server commit S1: modifies a.txt line 1 (conflict with client)
    let s1 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("a.txt", "SERVER_a1\na2\na3\na4\na5\na6\na7\na8\n")
        .commit()
        .await?;

    // Server commit S2: modifies b.txt line 1 (conflict with client)
    let s2 = CreateCommitContext::new(&ctx, &repo, vec![s1])
        .add_file("b.txt", "SERVER_b1\nb2\nb3\nb4\nb5\nb6\nb7\nb8\n")
        .commit()
        .await?;

    let book = BookmarkKey::new("master")?;
    let hg_s2 = repo.derive_hg_changeset(&ctx, s2).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_s2}")).await?;

    // Client modifies both files at the END (non-overlapping with server)
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("a.txt", "a1\na2\na3\na4\na5\na6\na7\nCLIENT_a8\n")
        .add_file("b.txt", "b1\nb2\nb3\nb4\nb5\nb6\nb7\nCLIENT_b8\n")
        .commit()
        .await?;

    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_merge_test();

    let client_cf = find_changed_files(&ctx, &repo, base, client).await?;

    // --- Attempt 1: base → S1 (only a.txt conflict detected) ---
    let result1 = check_pushrebase_conflicts(
        &ctx,
        &repo,
        &Default::default(),
        base,
        base,
        s1,
        std::slice::from_ref(&client_bcs),
        &client_cf,
    )
    .await?;
    assert!(
        result1.merged_file_overrides.is_some(),
        "Attempt 1 should resolve a.txt conflict"
    );
    let carried = result1.merged_file_overrides.unwrap();
    assert_eq!(carried.len(), 1, "Only a.txt should be in carried info");

    // --- Attempt 2: narrow range S1 → S2 (b.txt conflict detected) ---
    let result2 = check_pushrebase_conflicts(
        &ctx,
        &repo,
        &Default::default(),
        base,
        s1,
        s2,
        std::slice::from_ref(&client_bcs),
        &client_cf,
    )
    .await?;
    assert!(
        result2.merged_file_overrides.is_some(),
        "Attempt 2 should resolve b.txt conflict in narrow range"
    );
    let delta = result2.merged_file_overrides.unwrap();
    assert_eq!(delta.len(), 1, "Only b.txt should be in delta info");

    // Reconcile: carried has a.txt, delta has b.txt → union of both
    let reconciled = reconcile_merge_file_info(&carried, &delta);
    assert_eq!(
        reconciled.len(),
        2,
        "Reconciled should have both a.txt and b.txt"
    );

    // Rebase with reconciled overrides
    let rebased_set = fetch_bonsai_range_ancestor_not_included(&ctx, &repo, base, client).await?;
    let (new_head, _, rebased_bonsais) = create_rebased_changesets(
        &ctx,
        &repo,
        &Default::default(),
        &rebased_set,
        base,
        client,
        s2,
        &mut [],
        Some(reconciled),
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased_bonsais).await?;

    // Verify both files have merged content
    let result_hg = repo.derive_hg_changeset(&ctx, new_head).await?;
    ensure_content(
        &ctx,
        result_hg,
        &repo,
        btreemap! {
            "a.txt".to_string() => "SERVER_a1\na2\na3\na4\na5\na6\na7\nCLIENT_a8\n".to_string(),
            "b.txt".to_string() => "SERVER_b1\nb2\nb3\nb4\nb5\nb6\nb7\nCLIENT_b8\n".to_string(),
        },
    )
    .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_stack_non_head_conflict(
    fb: FacebookInit,
) -> Result<(), Error> {
    // Regression test: 2-commit stack where the FIRST commit (not HEAD)
    // touches a conflicting file. The merge override must be applied to
    // that first commit, not HEAD; otherwise the first commit keeps stale
    // content that reverts the server's changes.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    let base_content = "\
line 1
line 2
line 3
line 4
line 5
line 6
line 7
line 8
";
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", base_content)
        .add_file("other.txt", "other\n")
        .commit()
        .await?;

    // Server adds "line 2.1" between line 2 and line 3 (top region)
    let server_content = "\
line 1
line 2
line 2.1
line 3
line 4
line 5
line 6
line 7
line 8
";
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", server_content)
        .commit()
        .await?;

    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;

    // Client commit 1: adds "line 6.1" between line 6 and line 7 (bottom region)
    let client_content_1 = "\
line 1
line 2
line 3
line 4
line 5
line 6
line 6.1
line 7
line 8
";
    let client_1 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", client_content_1)
        .commit()
        .await?;

    // Client commit 2 (HEAD): only touches other.txt, NOT file.txt
    let client_2 = CreateCommitContext::new(&ctx, &repo, vec![client_1])
        .add_file("other.txt", "modified other\n")
        .commit()
        .await?;

    let client_bcs_1 = client_1.load(&ctx, repo.repo_blobstore()).await?;
    let client_bcs_2 = client_2.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_merge_test();

    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs_1.clone(), client_bcs_2.clone()],
        &[],
        None,
    )
    .await?;

    let expected_merged = "\
line 1
line 2
line 2.1
line 3
line 4
line 5
line 6
line 6.1
line 7
line 8
";

    // HEAD has the correct merged content
    let result_hg = repo.derive_hg_changeset(&ctx, result.head).await?;
    ensure_content(
        &ctx,
        result_hg,
        &repo,
        btreemap! {
            "file.txt".to_string() => expected_merged.to_string(),
            "other.txt".to_string() => "modified other\n".to_string(),
        },
    )
    .await?;

    // Read file.txt from the FIRST rebased commit
    let rebased_1 = result
        .rebased_changesets
        .iter()
        .find(|pair| pair.id_old == client_1)
        .map(|pair| pair.id_new)
        .expect("first commit should be in rebased set");

    let rebased_1_hg = repo.derive_hg_changeset(&ctx, rebased_1).await?;
    let rebased_1_cs = rebased_1_hg.load(&ctx, repo.repo_blobstore()).await?;
    let rebased_1_manifest = rebased_1_cs.manifestid();
    let file_path = NonRootMPath::new("file.txt")?;
    let file_entry = rebased_1_manifest
        .find_entry(ctx.clone(), repo.repo_blobstore().clone(), file_path.into())
        .await?
        .expect("file.txt should exist in first rebased commit");

    let file_content = match file_entry {
        Entry::Leaf((_, filenode_id)) => {
            let content_id = filenode_id
                .load(&ctx, repo.repo_blobstore())
                .await?
                .content_id();
            let bytes = filestore::fetch_concat(repo.repo_blobstore(), &ctx, content_id).await?;
            String::from_utf8(bytes.to_vec())?
        }
        _ => panic!("file.txt should be a file"),
    };

    assert!(
        file_content.contains("line 6.1"),
        "first rebased commit should have line 6.1 (client's change)"
    );

    // FIX: With cascading merge, the first rebased commit now has the
    // server's "line 2.1" because the merge is applied per-commit
    // during the rebase, not just to HEAD.
    assert!(
        file_content.contains("line 2.1"),
        "first rebased commit should have server's 'line 2.1' \
         (cascading merge applied per-commit). Actual:\n{file_content}",
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_stack_non_head_conflict_without_merge_resolution(
    fb: FacebookInit,
) -> Result<(), Error> {
    // Same scenario but merge resolution DISABLED: pushrebase should fail.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    let base_content = "\
line 1
line 2
line 3
line 4
line 5
";
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", base_content)
        .add_file("other.txt", "other\n")
        .commit()
        .await?;

    let server_content = "\
line 1
line 2
line 2.1
line 3
line 4
line 5
";
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", server_content)
        .commit()
        .await?;

    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;

    let client_content = "\
line 1
line 2
line 3
line 4
line 5
line 5.1
";
    let client_1 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", client_content)
        .commit()
        .await?;
    let client_2 = CreateCommitContext::new(&ctx, &repo, vec![client_1])
        .add_file("other.txt", "modified\n")
        .commit()
        .await?;

    let client_bcs_1 = client_1.load(&ctx, repo.repo_blobstore()).await?;
    let client_bcs_2 = client_2.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_test();

    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs_1, client_bcs_2],
        &[],
        None,
    )
    .await;

    should_have_conflicts(result);

    Ok(())
}

#[mononoke::test]
fn reconcile_merge_file_info_basic() {
    use mononoke_types::hash::Blake2;

    let id_a = ContentId::new(Blake2::from_byte_array([1; 32]));
    let id_b = ContentId::new(Blake2::from_byte_array([2; 32]));
    let id_c = ContentId::new(Blake2::from_byte_array([3; 32]));
    let id_d = ContentId::new(Blake2::from_byte_array([4; 32]));

    let file = |content_id: ContentId| {
        Some(BaseFile {
            content_id,
            file_type: FileType::Regular,
        })
    };
    let make_info = |path: &str, base: ContentId, server: ContentId| -> MergedFileInfo {
        MergedFileInfo {
            path: NonRootMPath::new(path).unwrap(),
            base: file(base),
            server: file(server),
            resolution: Resolution::Merge,
        }
    };

    // Test 1: empty carried + non-empty delta returns delta
    let delta = vec![make_info("f1", id_a, id_b)];
    let result = reconcile_merge_file_info(&[], &delta);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].path, NonRootMPath::new("f1").unwrap());
    assert_eq!(result[0].server, file(id_b));

    // Test 2: non-empty carried + empty delta returns carried
    let carried = vec![make_info("f1", id_a, id_b)];
    let result = reconcile_merge_file_info(&carried, &[]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].server, file(id_b));

    // Test 3: overlapping path updates server_content_id from delta
    let carried = vec![make_info("f1", id_a, id_b)];
    let delta = vec![make_info("f1", id_a, id_c)];
    let result = reconcile_merge_file_info(&carried, &delta);
    assert_eq!(result.len(), 1);
    assert_eq!(
        result[0].server,
        file(id_c),
        "server state should be updated from delta"
    );
    assert_eq!(
        result[0].base,
        file(id_a),
        "base state should remain from carried"
    );

    // Test 4: non-overlapping paths produce union
    let carried = vec![make_info("f1", id_a, id_b)];
    let delta = vec![make_info("f2", id_c, id_d)];
    let result = reconcile_merge_file_info(&carried, &delta);
    assert_eq!(result.len(), 2, "Should have both f1 and f2");
    let has_f1 = result
        .iter()
        .any(|i| i.path == NonRootMPath::new("f1").unwrap());
    let has_f2 = result
        .iter()
        .any(|i| i.path == NonRootMPath::new("f2").unwrap());
    assert!(has_f1, "Should contain f1 from carried");
    assert!(has_f2, "Should contain f2 from delta");
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_server_deletion_on_retry(
    fb: FacebookInit,
) -> Result<(), Error> {
    // Test: If a previously-conflicting file is deleted on the server
    // in the delta range, the narrow-range check should detect the
    // conflict (file deleted vs client modified) and fail.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    init_just_knobs_for_merge_test();

    let base_content = "line1\nline2\nline3\nline4\nline5\n";
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", base_content)
        .commit()
        .await?;

    // S1: modify first line (resolvable conflict with client)
    let s1_content = "modified_line1\nline2\nline3\nline4\nline5\n";
    let s1 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", s1_content)
        .commit()
        .await?;

    // S2: delete file.txt
    let s2 = CreateCommitContext::new(&ctx, &repo, vec![s1])
        .delete_file("file.txt")
        .commit()
        .await?;

    // Set bookmark to S2
    let book = BookmarkKey::new("master")?;
    let hg_s2 = repo.derive_hg_changeset(&ctx, s2).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_s2}")).await?;

    // Client: modify last line (based on base)
    let client_content = "line1\nline2\nline3\nline4\nmodified_line5\n";
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", client_content)
        .commit()
        .await?;

    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;
    let client_cf = find_changed_files(&ctx, &repo, base, client).await?;

    // Attempt 1: base → S1 (detects conflict, produces MergedFileInfo)
    let result1 = check_pushrebase_conflicts(
        &ctx,
        &repo,
        &Default::default(),
        base,
        base,
        s1,
        std::slice::from_ref(&client_bcs),
        &client_cf,
    )
    .await?;
    assert!(
        result1.merged_file_overrides.is_some(),
        "Attempt 1 should resolve the conflict"
    );

    // Attempt 2: S1 → S2 (file deleted — should fail with conflict)
    let result2 = check_pushrebase_conflicts(
        &ctx,
        &repo,
        &Default::default(),
        base,
        s1,
        s2,
        std::slice::from_ref(&client_bcs),
        &client_cf,
    )
    .await;

    // File was deleted on server but client modifies it — irreconcilable
    match result2 {
        Err(PushrebaseError::Conflicts(_)) => { /* expected */ }
        Err(e) => {
            panic!("Expected Conflicts error for file deleted on server, got error: {e}",)
        }
        Ok(_) => panic!("Expected Conflicts error for file deleted on server, but got Ok",),
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_no_conflict_in_delta(fb: FacebookInit) -> Result<(), Error> {
    // Test: When merge resolution succeeds on attempt 1 and a subsequent
    // unrelated server commit moves the bookmark, the carried MergedFileInfo
    // is correctly used on retry (no conflict in the delta range).
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    init_just_knobs_for_merge_test();

    let base_content = "line1\nline2\nline3\nline4\nline5\n";
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", base_content)
        .commit()
        .await?;

    // S1: modify first line
    let s1_content = "modified_line1\nline2\nline3\nline4\nline5\n";
    let s1 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", s1_content)
        .commit()
        .await?;

    // S2: unrelated change
    let s2 = CreateCommitContext::new(&ctx, &repo, vec![s1])
        .add_file("other.txt", "unrelated\n")
        .commit()
        .await?;

    let book = BookmarkKey::new("master")?;
    let hg_s2 = repo.derive_hg_changeset(&ctx, s2).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_s2}")).await?;

    // Client: modify last line
    let client_content = "line1\nline2\nline3\nline4\nmodified_line5\n";
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", client_content)
        .commit()
        .await?;

    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;
    let client_cf = find_changed_files(&ctx, &repo, base, client).await?;

    // Attempt 1: base → S1
    let result1 = check_pushrebase_conflicts(
        &ctx,
        &repo,
        &Default::default(),
        base,
        base,
        s1,
        std::slice::from_ref(&client_bcs),
        &client_cf,
    )
    .await?;
    assert!(
        result1.merged_file_overrides.is_some(),
        "Attempt 1 should detect and resolve the conflict"
    );
    let carried = result1.merged_file_overrides.unwrap();

    // Attempt 2: S1 → S2 (no conflict in delta)
    let result2 = check_pushrebase_conflicts(
        &ctx,
        &repo,
        &Default::default(),
        base,
        s1,
        s2,
        std::slice::from_ref(&client_bcs),
        &client_cf,
    )
    .await?;
    assert!(
        result2.merged_file_overrides.is_none(),
        "No conflicts in narrow range S1→S2"
    );

    // Reconcile: carried info used as-is (no delta)
    let reconciled = carried;

    // Rebase with carried overrides
    let rebased_set = fetch_bonsai_range_ancestor_not_included(&ctx, &repo, base, client).await?;
    let (new_head, _, rebased_bonsais) = create_rebased_changesets(
        &ctx,
        &repo,
        &Default::default(),
        &rebased_set,
        base,
        client,
        s2,
        &mut [],
        Some(reconciled),
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased_bonsais).await?;

    // Verify merged content
    let result_hg = repo.derive_hg_changeset(&ctx, new_head).await?;
    let result_cs = result_hg.load(&ctx, repo.repo_blobstore()).await?;
    let manifest = result_cs.manifestid();
    let file_path = NonRootMPath::new("file.txt")?;
    let file_entry = manifest
        .find_entry(ctx.clone(), repo.repo_blobstore().clone(), file_path.into())
        .await?
        .expect("file.txt should exist");

    let file_content = match file_entry {
        Entry::Leaf((_, filenode_id)) => {
            let content_id = filenode_id
                .load(&ctx, repo.repo_blobstore())
                .await?
                .content_id();
            let bytes = filestore::fetch_concat(repo.repo_blobstore(), &ctx, content_id).await?;
            String::from_utf8(bytes.to_vec())?
        }
        _ => panic!("file.txt should be a file"),
    };

    assert!(
        file_content.contains("modified_line1"),
        "should have server's change"
    );
    assert!(
        file_content.contains("modified_line5"),
        "should have client's change"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn batched_pushrebase_merge_resolution(fb: FacebookInit) -> Result<(), Error> {
    // Test: batched pushrebase resolves merge conflicts when server and
    // client modify different parts of the same file.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    init_just_knobs_for_merge_test();

    let base_content = "line1\nline2\nline3\nline4\nline5\n";
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", base_content)
        .commit()
        .await?;

    // Server commit: modify first line
    let server_content = "modified_line1\nline2\nline3\nline4\nline5\n";
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", server_content)
        .commit()
        .await?;

    // Set bookmark to server commit
    let bookmark = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &bookmark, &format!("{hg_server}")).await?;

    // Client: modify last line (based on base)
    let client_content = "line1\nline2\nline3\nline4\nmodified_line5\n";
    let client_cs_id = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", client_content)
        .commit()
        .await?;

    let client_bcs = client_cs_id.load(&ctx, repo.repo_blobstore()).await?;
    let config = PushrebaseFlags::default();
    let mut stack =
        index_pushrebase_request(&ctx, &repo, &config, &bookmark, &hashset![client_bcs]).await?;

    stack.precompute(&ctx, &repo, &config, &bookmark).await?;
    let (tx, rx) = oneshot::channel();
    let request = queued_pushrebase_request(&ctx, stack, tx);

    let requeued = do_batched_pushrebase(&ctx, &repo, &config, &bookmark, vec![request], &[]).await;
    assert!(requeued.is_empty(), "Should not be requeued");

    let outcome = rx.await.unwrap().unwrap();
    assert!(
        outcome.merge_resolved_paths.is_some(),
        "Should report merge resolved paths"
    );
    let resolved_paths = outcome.merge_resolved_paths.unwrap();
    assert_eq!(resolved_paths.len(), 1);
    assert_eq!(resolved_paths[0], NonRootMPath::new("file.txt")?);

    // Verify merged content
    let result_hg = repo.derive_hg_changeset(&ctx, outcome.head).await?;
    let result_cs = result_hg.load(&ctx, repo.repo_blobstore()).await?;
    let manifest = result_cs.manifestid();
    let file_path = NonRootMPath::new("file.txt")?;
    let file_entry = manifest
        .find_entry(ctx.clone(), repo.repo_blobstore().clone(), file_path.into())
        .await?
        .expect("file.txt should exist");

    let file_content = match file_entry {
        Entry::Leaf((_, filenode_id)) => {
            let content_id = filenode_id
                .load(&ctx, repo.repo_blobstore())
                .await?
                .content_id();
            let bytes = filestore::fetch_concat(repo.repo_blobstore(), &ctx, content_id).await?;
            String::from_utf8(bytes.to_vec())?
        }
        _ => panic!("file.txt should be a file"),
    };

    assert!(
        file_content.contains("modified_line1"),
        "should have server's change"
    );
    assert!(
        file_content.contains("modified_line5"),
        "should have client's change"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn batched_pushrebase_rebase_failure_preserves_conflict_checks(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_merge_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let config = PushrebaseFlags::default();
    let book = master_bookmark();
    let base_content = "one\ntwo\nthree\nfour\nfive\n";
    let server_content = "server\ntwo\nthree\nfour\nfive\n";
    let client_content = "one\ntwo\nthree\nfour\nclient\n";
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("before", base_content)
        .add_file("broken", base_content)
        .add_file("after", base_content)
        .commit()
        .await?;
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("before", server_content)
        .add_file("broken", server_content)
        .add_file("after", server_content)
        .commit()
        .await?;

    bookmark(&ctx, &repo, book.clone()).set_to(base).await?;
    let mut requests = Vec::new();
    let mut receivers = Vec::new();
    for (path, content) in [
        ("before", client_content),
        ("broken", "client\ntwo\nthree\nfour\nfive\n"),
        ("after", client_content),
    ] {
        let cs_id = CreateCommitContext::new(&ctx, &repo, vec![base])
            .add_file(path, content)
            .commit()
            .await?;
        let bcs = cs_id.load(&ctx, repo.repo_blobstore()).await?;
        let mut stack =
            index_pushrebase_request(&ctx, &repo, &config, &book, &hashset![bcs]).await?;
        stack.precompute(&ctx, &repo, &config, &book).await?;
        let (tx, rx) = oneshot::channel();
        requests.push(queued_pushrebase_request(&ctx, stack, tx));
        receivers.push(rx);
    }

    bookmark(&ctx, &repo, book.clone()).set_to(server).await?;
    let requeued = do_batched_pushrebase(&ctx, &repo, &config, &book, requests, &[]).await;
    let error = receivers.remove(1).await?.expect_err("merge should fail");
    assert!(matches!(error.inner(), PushrebaseError::Conflicts(_)));
    assert_eq!(requeued.len(), 2);
    assert_eq!(requeued[0].stack.conflict_check_base, server);
    assert_eq!(requeued[0].stack.carried_merge_file_info.len(), 1);
    assert_eq!(requeued[1].stack.conflict_check_base, base);
    assert!(requeued[1].stack.carried_merge_file_info.is_empty());

    let requeued = do_batched_pushrebase(&ctx, &repo, &config, &book, requeued, &[]).await;
    assert!(requeued.is_empty());
    for receiver in receivers {
        receiver.await?.map_err(|error| anyhow!(error))?;
    }
    let head = get_bookmark_value(&ctx, &repo, &book)
        .await?
        .expect("bookmark should be set after pushrebase");
    let files = list_working_copy(&ctx, &repo, head).await?;
    for path in ["before", "after"] {
        let content = &files[&NonRootMPath::new(path)?];
        assert_eq!(&content[..], b"server\ntwo\nthree\nfour\nclient\n");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn batched_pushrebase_merge_resolution_carry_forward(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    init_just_knobs_for_merge_test();

    let base_content = "line1\nline2\nline3\nline4\nline5\n";
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", base_content)
        .commit()
        .await?;

    // S1: modify first line (conflict with client)
    let s1_content = "modified_line1\nline2\nline3\nline4\nline5\n";
    let s1 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", s1_content)
        .commit()
        .await?;

    // S2: unrelated change (no conflict in delta S1→S2)
    let s2 = CreateCommitContext::new(&ctx, &repo, vec![s1])
        .add_file("other.txt", "unrelated\n")
        .commit()
        .await?;

    let bookmark = BookmarkKey::new("master")?;
    let hg_s2 = repo.derive_hg_changeset(&ctx, s2).await?;
    set_bookmark(ctx.clone(), &repo, &bookmark, &format!("{hg_s2}")).await?;

    // Client: modify last line
    let client_content = "line1\nline2\nline3\nline4\nmodified_line5\n";
    let client_cs_id = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", client_content)
        .commit()
        .await?;

    let client_bcs = client_cs_id.load(&ctx, repo.repo_blobstore()).await?;
    let config = PushrebaseFlags::default();
    let mut stack =
        index_pushrebase_request(&ctx, &repo, &config, &bookmark, &hashset![client_bcs]).await?;

    let hg_s1 = repo.derive_hg_changeset(&ctx, s1).await?;
    set_bookmark(ctx.clone(), &repo, &bookmark, &format!("{hg_s1}")).await?;
    stack.precompute(&ctx, &repo, &config, &bookmark).await?;
    assert_eq!(stack.conflict_check_base, s1);
    assert!(!stack.carried_merge_file_info.is_empty());
    set_bookmark(ctx.clone(), &repo, &bookmark, &format!("{hg_s2}")).await?;
    let (tx, rx) = oneshot::channel();
    let mut request = queued_pushrebase_request(&ctx, stack, tx);
    request.retry_num = PushrebaseRetryNum(1);

    let requeued = do_batched_pushrebase(&ctx, &repo, &config, &bookmark, vec![request], &[]).await;
    assert!(requeued.is_empty(), "Should not be requeued");

    let outcome = rx.await.unwrap().unwrap();
    assert!(
        outcome.merge_resolved_paths.is_some(),
        "Should report merge resolved paths via carry-forward"
    );

    // Verify merged content
    let result_hg = repo.derive_hg_changeset(&ctx, outcome.head).await?;
    let result_cs = result_hg.load(&ctx, repo.repo_blobstore()).await?;
    let manifest = result_cs.manifestid();
    let file_path = NonRootMPath::new("file.txt")?;
    let file_entry = manifest
        .find_entry(ctx.clone(), repo.repo_blobstore().clone(), file_path.into())
        .await?
        .expect("file.txt should exist");

    let file_content = match file_entry {
        Entry::Leaf((_, filenode_id)) => {
            let content_id = filenode_id
                .load(&ctx, repo.repo_blobstore())
                .await?
                .content_id();
            let bytes = filestore::fetch_concat(repo.repo_blobstore(), &ctx, content_id).await?;
            String::from_utf8(bytes.to_vec())?
        }
        _ => panic!("file.txt should be a file"),
    };

    assert!(
        file_content.contains("modified_line1"),
        "should have server's change via carry-forward"
    );
    assert!(
        file_content.contains("modified_line5"),
        "should have client's change"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn batched_precomputation_survives_cas_retry(fb: FacebookInit) -> Result<(), Error> {
    struct MoveBookmark(PushrebaseTestRepo, ChangesetId);

    #[async_trait]
    impl PushrebaseHook for MoveBookmark {
        async fn in_critical_section(
            &self,
            ctx: &CoreContext,
            _old_bookmark_value: Option<ChangesetId>,
        ) -> Result<Box<dyn PushrebaseCommitHook>, Error> {
            bookmark(ctx, &self.0, master_bookmark())
                .set_to(self.1)
                .await?;
            Ok(Box::new(SleepHook))
        }
    }

    init_just_knobs_for_merge_test();
    for same_file in [false, true] {
        let ctx = CoreContext::test_mock(fb);
        let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
        let config = PushrebaseFlags::default();
        let book = master_bookmark();
        let base = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file", "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n")
            .commit()
            .await?;
        let s1 = CreateCommitContext::new(&ctx, &repo, vec![base])
            .add_file(
                "file",
                "server\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n",
            )
            .commit()
            .await?;
        let s2 = CreateCommitContext::new(&ctx, &repo, vec![s1])
            .add_file(
                if same_file { "file" } else { "unrelated" },
                "server\ntwo\nserver2\nfour\nfive\nsix\nseven\neight\n",
            )
            .commit()
            .await?;
        bookmark(&ctx, &repo, book.clone()).set_to(s1).await?;
        let c1 = CreateCommitContext::new(&ctx, &repo, vec![base])
            .add_file(
                "file",
                "one\ntwo\nthree\nfour\nclient1\nsix\nseven\neight\n",
            )
            .commit()
            .await?;
        let c2 = CreateCommitContext::new(&ctx, &repo, vec![c1])
            .add_file(
                "file",
                "one\ntwo\nthree\nfour\nclient1\nsix\nseven\nclient2\n",
            )
            .commit()
            .await?;
        let mut stack = index_pushrebase_request(
            &ctx,
            &repo,
            &config,
            &book,
            &hashset![
                c1.load(&ctx, repo.repo_blobstore()).await?,
                c2.load(&ctx, repo.repo_blobstore()).await?
            ],
        )
        .await?;
        stack.precompute(&ctx, &repo, &config, &book).await?;
        let carried = stack.carried_merge_file_info.clone();
        assert_eq!(carried.len(), 1);
        let (tx, rx) = oneshot::channel();
        let mut retries = do_batched_pushrebase(
            &ctx,
            &repo,
            &config,
            &book,
            vec![queued_pushrebase_request(&ctx, stack, tx)],
            &[Box::new(MoveBookmark(repo.clone(), s2))],
        )
        .await;
        assert_eq!(retries.len(), 1);
        assert_eq!(retries[0].stack.conflict_check_base, s1);
        assert_eq!(retries[0].stack.carried_merge_file_info, carried);
        let fresh_id = CreateCommitContext::new(&ctx, &repo, vec![base])
            .add_file("fresh", "fresh")
            .commit()
            .await?;
        let mut fresh = index_pushrebase_request(
            &ctx,
            &repo,
            &config,
            &book,
            &hashset![fresh_id.load(&ctx, repo.repo_blobstore()).await?],
        )
        .await?;
        fresh.precompute(&ctx, &repo, &config, &book).await?;
        let (fresh_tx, fresh_rx) = oneshot::channel();
        retries.push(queued_pushrebase_request(&ctx, fresh, fresh_tx));
        assert!(
            do_batched_pushrebase(&ctx, &repo, &config, &book, retries, &[])
                .await
                .is_empty()
        );
        let outcome = rx.await??;
        assert_eq!(outcome.retry_num.0, 1);
        assert_eq!(outcome.log_id, fresh_rx.await??.log_id);
        for (id, last_line) in [(c1, "eight"), (c2, "client2")] {
            let pair = outcome
                .rebased_changesets
                .iter()
                .find(|pair| pair.id_old == id)
                .unwrap();
            let files = list_working_copy(&ctx, &repo, pair.id_new).await?;
            let expected = format!(
                "server\ntwo\n{}\nfour\nclient1\nsix\nseven\n{last_line}\n",
                if same_file { "server2" } else { "three" }
            );
            assert_eq!(&files[&NonRootMPath::new("file")?][..], expected.as_bytes());
        }
    }
    Ok(())
}

fn init_just_knobs_for_noop_rejection_test(reject: bool) {
    override_just_knobs(JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:pushrebase_enable_merge_resolution".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_merge_resolution_derive_fsnodes".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_reject_noop_merge_commits".to_string() => KnobVal::Bool(reject),
        "scm/mononoke:pushrebase_range_diff_use_content_manifests".to_string() => KnobVal::Bool(false),
    }));
}

#[mononoke::fbinit_test]
async fn pushrebase_noop_merge_detected_only_when_jk_off(fb: FacebookInit) -> Result<(), Error> {
    // JK off: client wrote identical content to server. The commit should
    // still land as a no-op (current Phase 1 dry-run behavior). Detection
    // is logged but no rejection happens.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", "line1\nline2\nline3\n")
        .commit()
        .await?;

    let shared_content = "line1\nSHARED_EDIT\nline3\n";
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", shared_content)
        .commit()
        .await?;

    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;

    // Client wrote IDENTICAL content (same `shared_content`) — duplicate change.
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", shared_content)
        .commit()
        .await?;
    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_noop_rejection_test(false);

    // Should succeed (JK off → detection only, no rejection).
    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await?;

    assert!(result.rebased_changesets.len() == 1, "one commit rebased");
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_noop_merge_rejected_single_file(fb: FacebookInit) -> Result<(), Error> {
    // JK on: single-file duplicate-content commit → rejected with Conflicts
    // on that path, matching pre-merge-resolution behavior.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", "line1\nline2\nline3\n")
        .commit()
        .await?;

    let shared_content = "line1\nSHARED_EDIT\nline3\n";
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", shared_content)
        .commit()
        .await?;

    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;

    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", shared_content)
        .commit()
        .await?;
    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_noop_rejection_test(true);

    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await;

    match result {
        Err(PushrebaseError::Conflicts(conflicts)) => {
            assert_eq!(
                conflicts.len(),
                1,
                "Should report one conflict for the single duplicate path"
            );
            let path_str = format!("{}", conflicts[0].left);
            assert_eq!(
                path_str, "file.txt",
                "Conflict should name the duplicate file"
            );
        }
        other => panic!("Expected Conflicts error, got: {other:?}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_noop_merge_mixed_commit_lands(fb: FacebookInit) -> Result<(), Error> {
    // NEGATIVE test: commit has both a duplicate-content file AND a
    // non-conflicting real change. The commit has a real net change so
    // it must NOT be flagged as no-op — should land normally.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    // Base has only the file that will conflict
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dup.txt", "line1\nline2\nline3\n")
        .commit()
        .await?;

    let shared_content = "line1\nSHARED_EDIT\nline3\n";
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("dup.txt", shared_content)
        .commit()
        .await?;

    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;

    // Client: duplicate edit to dup.txt + real new file `new.txt`
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("dup.txt", shared_content)
        .add_file("new.txt", "brand new content\n")
        .commit()
        .await?;
    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_noop_rejection_test(true);

    // Should succeed despite the duplicate file_change because new.txt
    // is a real change.
    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await?;

    assert_eq!(
        result.rebased_changesets.len(),
        1,
        "One commit should be rebased — mixed commit must not be rejected"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_noop_merge_in_stack_rejects_entire_stack(
    fb: FacebookInit,
) -> Result<(), Error> {
    // JK on: 2-commit stack where the FIRST commit becomes a no-op (its
    // only file_change is a duplicate). The entire stack should be
    // rejected, not just the no-op commit.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dup.txt", "line1\nline2\nline3\n")
        .commit()
        .await?;

    let shared_content = "line1\nSHARED_EDIT\nline3\n";
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("dup.txt", shared_content)
        .commit()
        .await?;

    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;

    // Client commit 1: duplicate edit to dup.txt only — would become no-op
    let c1 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("dup.txt", shared_content)
        .commit()
        .await?;
    // Client commit 2 (HEAD): real change to a different file
    let c2 = CreateCommitContext::new(&ctx, &repo, vec![c1])
        .add_file("other.txt", "real content\n")
        .commit()
        .await?;
    let c1_bcs = c1.load(&ctx, repo.repo_blobstore()).await?;
    let c2_bcs = c2.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_noop_rejection_test(true);

    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![c1_bcs, c2_bcs],
        &[],
        None,
    )
    .await;

    match result {
        Err(PushrebaseError::Conflicts(conflicts)) => {
            let path_strs: HashSet<String> =
                conflicts.iter().map(|c| format!("{}", c.left)).collect();
            assert!(
                path_strs.contains("dup.txt"),
                "Conflicts should include dup.txt from the no-op c1"
            );
        }
        other => panic!("Expected Conflicts error rejecting the entire stack, got: {other:?}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_noop_merge_rejected_all_files_duplicate(fb: FacebookInit) -> Result<(), Error> {
    // JK on: client commit touches two files (A and B); server made the
    // identical edits to both. Every file_change is a duplicate, so the
    // commit becomes a no-op and must be rejected with conflicts on both
    // paths.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("a.txt", "line1\nline2\nline3\n")
        .add_file("b.txt", "alpha\nbeta\ngamma\n")
        .commit()
        .await?;

    let shared_a = "line1\nA_EDIT\nline3\n";
    let shared_b = "alpha\nB_EDIT\ngamma\n";
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("a.txt", shared_a)
        .add_file("b.txt", shared_b)
        .commit()
        .await?;

    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;

    // Client wrote IDENTICAL content to both files.
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("a.txt", shared_a)
        .add_file("b.txt", shared_b)
        .commit()
        .await?;
    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_noop_rejection_test(true);

    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await;

    match result {
        Err(PushrebaseError::Conflicts(conflicts)) => {
            let path_strs: HashSet<String> =
                conflicts.iter().map(|c| format!("{}", c.left)).collect();
            assert!(
                path_strs.contains("a.txt") && path_strs.contains("b.txt"),
                "Conflicts must include both duplicate paths, got: {path_strs:?}"
            );
        }
        other => panic!("Expected Conflicts error, got: {other:?}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_noop_merge_genuine_merge_not_flagged(fb: FacebookInit) -> Result<(), Error> {
    // NEGATIVE test: client and server edited DIFFERENT lines of the same
    // file. local_content_id != other_id, so our new check is not
    // triggered. Cascading merge resolves cleanly, override ≠ other, the
    // commit makes a real net change, and is not classified as no-op even
    // when the JK is on.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", "line1\nline2\nline3\n")
        .commit()
        .await?;

    // Server edits line 3 only.
    let server_content = "line1\nline2\nSERVER_EDIT\n";
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", server_content)
        .commit()
        .await?;

    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;

    // Client edits line 1 only — non-overlapping with server.
    let client_content = "CLIENT_EDIT\nline2\nline3\n";
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", client_content)
        .commit()
        .await?;
    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_noop_rejection_test(true);

    // Should succeed — genuine 3-way merge, no duplicate content detected.
    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await?;

    assert_eq!(
        result.rebased_changesets.len(),
        1,
        "Genuine merge must land — no duplicate paths detected"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_noop_merge_local_equals_base_not_flagged(
    fb: FacebookInit,
) -> Result<(), Error> {
    // NEGATIVE test: server edited file.txt away from base then edited it
    // BACK to the original base content. So path is in merge_paths
    // (server touched it), but base_id == other_id (server's net change
    // is zero). Client's edit goes through the existing
    // `base_id == other_id` short-circuit, NOT our new
    // `local_content_id == other_id` check, so duplicate_paths stays
    // empty and the commit is not flagged as no-op.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    let base_content = "line1\nline2\nline3\n";
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", base_content)
        .commit()
        .await?;

    // Server commit 1: edit away from base.
    let server1 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", "line1\nSERVER_INTERMEDIATE\nline3\n")
        .commit()
        .await?;

    // Server commit 2: revert back to base content. base_id == other_id
    // for this path now (root and current_master agree on content).
    let server2 = CreateCommitContext::new(&ctx, &repo, vec![server1])
        .add_file("file.txt", base_content)
        .commit()
        .await?;

    let book = BookmarkKey::new("master")?;
    let hg_server2 = repo.derive_hg_changeset(&ctx, server2).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server2}")).await?;

    // Client edits file.txt to brand-new content.
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", "line1\nCLIENT_EDIT\nline3\n")
        .commit()
        .await?;
    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_noop_rejection_test(true);

    // Should succeed — base==other path is hit, no duplicate detected.
    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await?;

    assert_eq!(
        result.rebased_changesets.len(),
        1,
        "base==other branch must not trip our duplicate-content detection"
    );
    Ok(())
}

// Bookmark points at a server commit that edited line 1; client commit
// edits line 5. Path-level conflict that MR can resolve, vanilla can't.
async fn setup_non_overlapping_conflict(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
) -> Result<(BonsaiChangeset, BookmarkKey), Error> {
    let base_content = "line1\nline2\nline3\nline4\nline5\n";
    let base = CreateCommitContext::new_root(ctx, repo)
        .add_file("file.txt", base_content)
        .commit()
        .await?;

    let server_content = "modified_line1\nline2\nline3\nline4\nline5\n";
    let server = CreateCommitContext::new(ctx, repo, vec![base])
        .add_file("file.txt", server_content)
        .commit()
        .await?;

    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(ctx, server).await?;
    set_bookmark(ctx.clone(), repo, &book, &format!("{hg_server}")).await?;

    let client_content = "line1\nline2\nline3\nline4\nmodified_line5\n";
    let client = CreateCommitContext::new(ctx, repo, vec![base])
        .add_file("file.txt", client_content)
        .commit()
        .await?;
    let client_bcs = client.load(ctx, repo.repo_blobstore()).await?;

    Ok((client_bcs, book))
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_override_forces_off(fb: FacebookInit) -> Result<(), Error> {
    // JK on, override off → must conflict.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let (client_bcs, book) = setup_non_overlapping_conflict(&ctx, &repo).await?;

    init_just_knobs_for_merge_test();

    let config = PushrebaseFlags {
        merge_resolution_override: MergeResolutionOverride::ForceOff,
        ..Default::default()
    };

    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &config,
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await;

    should_have_conflicts(result);
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_override_forces_on(fb: FacebookInit) -> Result<(), Error> {
    // JK off, override on → must merge cleanly.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let (client_bcs, book) = setup_non_overlapping_conflict(&ctx, &repo).await?;

    override_just_knobs(JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:pushrebase_enable_merge_resolution".to_string() => KnobVal::Bool(false),
        "scm/mononoke:pushrebase_merge_resolution_derive_fsnodes".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_range_diff_use_content_manifests".to_string() => KnobVal::Bool(false),
    }));

    let config = PushrebaseFlags {
        merge_resolution_override: MergeResolutionOverride::ForceOn,
        ..Default::default()
    };

    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &config,
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await?;

    let result_hg = repo.derive_hg_changeset(&ctx, result.head).await?;
    let expected_content = "modified_line1\nline2\nline3\nline4\nmodified_line5\n";
    ensure_content(
        &ctx,
        result_hg,
        &repo,
        btreemap! {
            "file.txt".to_string() => expected_content.to_string(),
        },
    )
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_override_none_falls_through(
    fb: FacebookInit,
) -> Result<(), Error> {
    // No override, JK off → must conflict (historical path).
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let (client_bcs, book) = setup_non_overlapping_conflict(&ctx, &repo).await?;

    override_just_knobs(JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:pushrebase_enable_merge_resolution".to_string() => KnobVal::Bool(false),
        "scm/mononoke:pushrebase_merge_resolution_derive_fsnodes".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_range_diff_use_content_manifests".to_string() => KnobVal::Bool(false),
    }));

    let config = PushrebaseFlags {
        merge_resolution_override: MergeResolutionOverride::UseJk,
        ..Default::default()
    };

    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &config,
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await;

    should_have_conflicts(result);
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_keeps_git_lfs_pointer_flag(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = Linear::get_repo(fb).await;
    let path = NonRootMPath::new("blob.bin")?;

    let root_hg = HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536")?;
    let root = repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(&ctx, root_hg)
        .await?
        .ok_or_else(|| Error::msg("Root missing"))?;

    // Store the content once, then re-add it flagged as an LFS pointer.
    let seed = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("blob.bin", "binary payload")
        .commit()
        .await?;
    let seed_bcs = seed.load(&ctx, repo.repo_blobstore()).await?;
    let seed_change = match seed_bcs.file_changes_map().get(&path) {
        Some(FileChange::Change(tc)) => tc.clone(),
        _ => return Err(Error::msg("blob.bin change missing")),
    };
    let lfs_change = FileChange::tracked(
        seed_change.content_id(),
        seed_change.file_type(),
        seed_change.size(),
        /* copy_from */ None,
        GitLfs::canonical_pointer(),
    );
    let bcs = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file_change(path.clone(), lfs_change)
        .commit()
        .await?;
    let hgcss = hashset![repo.derive_hg_changeset(&ctx, bcs).await?];

    // The bookmark is ahead of the commit's parent, so the commit is rebased.
    let book = master_bookmark();
    set_bookmark(
        ctx.clone(),
        &repo,
        &book,
        "a5ffa77602a066db7d5cfb9fb5823a0895717c5a",
    )
    .await?;
    let result = do_pushrebase(&ctx, &repo, &Default::default(), &book, &hgcss).await?;
    assert_ne!(result.head, bcs, "the commit should have been rebased");

    let result_bcs = result.head.load(&ctx, repo.repo_blobstore()).await?;
    match result_bcs.file_changes_map().get(&path) {
        Some(FileChange::Change(tc)) => {
            assert!(
                tc.git_lfs().is_lfs_pointer(),
                "a rebased change must still be served as an LFS pointer"
            );
            assert_eq!(tc.content_id(), seed_change.content_id());
        }
        _ => return Err(Error::msg("blob.bin change missing after rebase")),
    }
    Ok(())
}

fn manifest_rebase_flags(merge_resolution_override: MergeResolutionOverride) -> PushrebaseFlags {
    PushrebaseFlags {
        rewritedates: false,
        merge_resolution_override,
        ..Default::default()
    }
}

async fn derive_content_manifests(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    cs_ids: &[ChangesetId],
) -> Result<(), Error> {
    for cs_id in cs_ids {
        repo.repo_derived_data()
            .derive::<RootContentManifestId>(ctx, *cs_id, DerivationPriority::HIGH)
            .await?;
    }
    Ok(())
}

async fn changed_file_content(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    cs_id: ChangesetId,
    path: &str,
) -> Result<Bytes, Error> {
    let bcs = cs_id.load(ctx, repo.repo_blobstore()).await?;
    let content_id = match bcs.file_changes_map().get(&NonRootMPath::new(path)?) {
        Some(FileChange::Change(tc)) => tc.content_id(),
        other => panic!("expected a tracked change at {path}, got {other:?}"),
    };
    filestore::fetch_concat(repo.repo_blobstore(), ctx, content_id).await
}

const NINE_LINES: &str = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\n";
const MAX_PATHS: usize = 10_000;

#[mononoke::fbinit_test]
async fn fetch_root_manifest_id_does_not_derive(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let cs = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", "f")
        .commit()
        .await?;

    assert!(
        fetch_root_manifest_id(&ctx, &repo, cs).await?.is_none(),
        "nothing derived yet"
    );
    assert!(
        fetch_root_manifest_id(&ctx, &repo, cs).await?.is_none(),
        "the probe must not derive"
    );
    derive_content_manifests(&ctx, &repo, &[cs]).await?;
    assert!(fetch_root_manifest_id(&ctx, &repo, cs).await?.is_some());
    Ok(())
}

#[mononoke::fbinit_test]
async fn lookup_manifest_states_reports_file_tree_absent(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let cs = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/file", "content")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[cs]).await?;
    let mf = fetch_root_manifest_id(&ctx, &repo, cs)
        .await?
        .expect("derived above");

    let states = lookup_manifest_states(
        &ctx,
        &repo,
        mf,
        &[
            MPath::new("dir/file")?,
            MPath::new("dir/nope")?,
            MPath::new("other")?,
        ],
    )
    .await?;
    assert!(matches!(
        states.get(&MPath::new("dir")?),
        Some(ManifestState::Tree(_))
    ));
    assert!(matches!(
        states.get(&MPath::new("dir/file")?),
        Some(ManifestState::File { size: 7, .. })
    ));
    assert_eq!(
        states.get(&MPath::new("dir/nope")?),
        Some(&ManifestState::Absent)
    );
    assert_eq!(
        states.get(&MPath::new("other")?),
        Some(&ManifestState::Absent)
    );
    assert_eq!(states.len(), 4, "the path set plus the one shared ancestor");
    Ok(())
}

#[mononoke::test]
fn manifest_overlaps_exact_and_prefix() -> Result<(), Error> {
    use mononoke_types::hash::Blake2;

    let file = |byte: u8| ManifestState::File {
        file: BaseFile {
            content_id: ContentId::new(Blake2::from_byte_array([byte; 32])),
            file_type: FileType::Regular,
        },
        size: 1,
    };
    let tree =
        |byte: u8| ManifestState::Tree(ContentManifestId::new(Blake2::from_byte_array([byte; 32])));
    let p = MPath::new("a/b/c")?;
    let ab = MPath::new("a/b")?;
    let a = MPath::new("a")?;

    // Same file both sides: no overlap.
    let base = hashmap! { a.clone() => tree(9), ab.clone() => tree(9), p.clone() => file(1) };
    assert!(manifest_overlaps(std::slice::from_ref(&p), &base, &base).is_empty());

    // Content differs: the path itself.
    let onto = hashmap! { a.clone() => tree(9), ab.clone() => tree(9), p.clone() => file(2) };
    assert_eq!(
        manifest_overlaps(std::slice::from_ref(&p), &base, &onto),
        vec![p.clone()]
    );

    // Present on one side only: the path itself.
    let onto = hashmap! { a.clone() => tree(9), ab.clone() => tree(9), p.clone() => ManifestState::Absent };
    assert_eq!(
        manifest_overlaps(std::slice::from_ref(&p), &base, &onto),
        vec![p.clone()]
    );

    // Ancestor is a file in onto: the shallowest such ancestor.
    let onto = hashmap! { a.clone() => tree(9), ab.clone() => file(3), p.clone() => ManifestState::Absent };
    assert_eq!(
        manifest_overlaps(std::slice::from_ref(&p), &base, &onto),
        vec![ab.clone()]
    );

    // Stack path is a file in base and a directory in onto.
    let base2 = hashmap! { a.clone() => tree(9), ab.clone() => file(1) };
    let onto2 = hashmap! { a.clone() => tree(9), ab.clone() => tree(9) };
    assert_eq!(
        manifest_overlaps(std::slice::from_ref(&ab), &base2, &onto2),
        vec![ab]
    );

    // Directory missing on one side is not an overlap by itself.
    let base3 = hashmap! { a.clone() => tree(9), MPath::new("a/x")? => file(1) };
    let onto3 =
        hashmap! { a => ManifestState::Absent, MPath::new("a/x")? => ManifestState::Absent };
    assert_eq!(
        manifest_overlaps(&[MPath::new("a/x")?], &base3, &onto3),
        vec![MPath::new("a/x")?],
        "only the file itself, which differs, is reported"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_forward_clean_and_deterministic(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let a = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("file_a", "a")
        .commit()
        .await?;
    let b = CreateCommitContext::new(&ctx, &repo, vec![a])
        .add_file("file_b", "b")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("server_file", "server")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;
    let flags = manifest_rebase_flags(MergeResolutionOverride::ForceOn);

    let rebased = rebase_stack_onto_manifest(&ctx, &repo, &flags, root, b, onto, MAX_PATHS).await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(rebased.rebased_changesets.len(), 2);
    assert!(rebased.dropped.is_empty());
    assert!(rebased.merged_paths.is_empty());
    assert!(matches!(
        rebased.merge_summary,
        MergeResolutionSummary::NotNeeded
    ));
    let new_a = rebased
        .rebased_changesets
        .iter()
        .find(|pair| pair.id_old == a)
        .expect("bottom commit rebased")
        .id_new;
    let new_a_bcs = new_a.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(new_a_bcs.parents().collect::<Vec<_>>(), vec![onto]);
    let new_head = rebased.new_head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(new_head.parents().collect::<Vec<_>>(), vec![new_a]);

    let again = rebase_stack_onto_manifest(&ctx, &repo, &flags, root, b, onto, MAX_PATHS).await?;
    assert_eq!(again.new_head, rebased.new_head, "deterministic rewrite");
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_backward_onto_ancestor(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let older = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", NINE_LINES)
        .commit()
        .await?;
    let newer = CreateCommitContext::new(&ctx, &repo, vec![older])
        .add_file("f", NINE_LINES.replace("l1\n", "L1\n"))
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![newer])
        .add_file(
            "f",
            NINE_LINES.replace("l1\n", "L1\n").replace("l9\n", "L9\n"),
        )
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[older, newer]).await?;

    let err = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOff),
        newer,
        d1,
        older,
        MAX_PATHS,
    )
    .await
    .expect_err("without merge resolution the overlap is a conflict");
    assert!(matches!(err, PushrebaseError::Conflicts(_)), "{err}");

    let rebased = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        newer,
        d1,
        older,
        MAX_PATHS,
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    let new_head = rebased.new_head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(new_head.parents().collect::<Vec<_>>(), vec![older]);
    assert_eq!(
        rebased.merged_paths.get(&d1).cloned().unwrap_or_default(),
        vec![NonRootMPath::new("f")?]
    );
    assert_eq!(
        changed_file_content(&ctx, &repo, rebased.new_head, "f").await?,
        Bytes::from(NINE_LINES.replace("l9\n", "L9\n")),
        "only the stack's own change is applied onto the older commit"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_divergent_onto(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let fork = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let root = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("left", "left")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![fork])
        .add_file("right", "right")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("stack", "stack")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let rebased = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d1,
        onto,
        MAX_PATHS,
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    let new_head = rebased.new_head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(new_head.parents().collect::<Vec<_>>(), vec![onto]);
    assert_eq!(
        new_head.file_changes_map().keys().collect::<Vec<_>>(),
        vec![&NonRootMPath::new("stack")?],
        "the rebased commit carries only the stack's change; `left` is not replayed"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_true_conflict(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", NINE_LINES)
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", NINE_LINES.replace("l5\n", "client\n"))
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", NINE_LINES.replace("l5\n", "server\n"))
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let err = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d1,
        onto,
        MAX_PATHS,
    )
    .await
    .expect_err("same line edited on both sides");
    match err {
        PushrebaseError::Conflicts(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].left, MPath::new("f")?);
        }
        other => panic!("expected Conflicts, got {other}"),
    }
    Ok(())
}

async fn assert_bottom_dropped(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
    root: ChangesetId,
    d1: ChangesetId,
    d2: ChangesetId,
    onto: ChangesetId,
) -> Result<(), Error> {
    derive_content_manifests(ctx, repo, &[root, onto]).await?;
    let rebased = rebase_stack_onto_manifest(
        ctx,
        repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d2,
        onto,
        MAX_PATHS,
    )
    .await?;
    changesets_creation::save_changesets(ctx, repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(rebased.dropped, vec![d1], "the landed bottom is dropped");
    assert_eq!(
        rebased
            .rebased_changesets
            .iter()
            .map(|pair| pair.id_old)
            .collect::<Vec<_>>(),
        vec![d2],
        "dropped commits are not reported as rebased"
    );
    assert_eq!(rebased.rebased_bonsais.len(), 1);
    let new_head = rebased.new_head.load(ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        new_head.parents().collect::<Vec<_>>(),
        vec![onto],
        "the child re-parents onto the destination"
    );
    assert_eq!(rebased.new_head, rebased.rebased_changesets[0].id_new);
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_drops_landed_bottom_modify(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", "v0")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", "v1")
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![d1])
        .add_file("g", "g")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", "v1")
        .add_file("other", "other")
        .commit()
        .await?;
    assert_bottom_dropped(&ctx, &repo, root, d1, d2, onto).await
}

#[mononoke::fbinit_test]
async fn manifest_rebase_drops_landed_bottom_add(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", "v0")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("added", "new")
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![d1])
        .add_file("g", "g")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("added", "new")
        .commit()
        .await?;
    assert_bottom_dropped(&ctx, &repo, root, d1, d2, onto).await
}

#[mononoke::fbinit_test]
async fn manifest_rebase_drops_landed_bottom_delete(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", "v0")
        .add_file("doomed", "x")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .delete_file("doomed")
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![d1])
        .add_file("g", "g")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .delete_file("doomed")
        .commit()
        .await?;
    assert_bottom_dropped(&ctx, &repo, root, d1, d2, onto).await
}

#[mononoke::fbinit_test]
async fn manifest_rebase_drops_post_merge_noop(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", NINE_LINES)
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", NINE_LINES.replace("l9\n", "L9\n"))
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![d1])
        .add_file("g", "g")
        .commit()
        .await?;
    // onto has d1's change plus an edit of its own.
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file(
            "f",
            NINE_LINES.replace("l9\n", "L9\n").replace("l1\n", "L1\n"),
        )
        .commit()
        .await?;
    assert_bottom_dropped(&ctx, &repo, root, d1, d2, onto).await
}

#[mononoke::fbinit_test]
async fn manifest_rebase_all_dropped(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", "v0")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", "v1")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", "v1")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let rebased = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d1,
        onto,
        MAX_PATHS,
    )
    .await?;
    assert_eq!(rebased.new_head, onto, "nothing left to rebase");
    assert!(rebased.rebased_changesets.is_empty());
    assert!(rebased.rebased_bonsais.is_empty());
    assert_eq!(rebased.dropped, vec![d1]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_prefix_conflict(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("a/x", "x")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("a/b/c", "c")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("a/b", "file, not a dir")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let err = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d1,
        onto,
        MAX_PATHS,
    )
    .await
    .expect_err("a file where the stack needs a directory");
    match err {
        PushrebaseError::Conflicts(conflicts) => {
            assert_eq!(
                conflicts,
                vec![PushrebaseConflict {
                    left: MPath::new("a/b")?,
                    right: MPath::new("a/b/c")?,
                }],
                "left is the destination's path, right the stack's"
            );
        }
        other => panic!("expected Conflicts, got {other}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_requires_derived_manifests(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("file_a", "a")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("server_file", "server")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root]).await?;

    let err = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d1,
        onto,
        MAX_PATHS,
    )
    .await
    .expect_err("onto is not derived");
    assert!(
        matches!(err, PushrebaseError::ManifestNotDerived(cs) if cs == onto),
        "{err}"
    );
    assert!(
        fetch_root_manifest_id(&ctx, &repo, onto).await?.is_none(),
        "the failed call must not have derived onto"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_rejects_bad_shapes(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let root = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("root", "root")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("d1", "d1")
        .commit()
        .await?;
    let side = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("side", "side")
        .commit()
        .await?;
    let merge = CreateCommitContext::new(&ctx, &repo, vec![d1, side])
        .add_file("m", "m")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("onto", "onto")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto, d1]).await?;
    let flags = manifest_rebase_flags(MergeResolutionOverride::ForceOn);

    let err = rebase_stack_onto_manifest(&ctx, &repo, &flags, root, merge, onto, MAX_PATHS)
        .await
        .expect_err("merge in stack");
    assert!(err.to_string().contains("merge commits"), "{err}");

    let err = rebase_stack_onto_manifest(&ctx, &repo, &flags, d1, root, onto, MAX_PATHS)
        .await
        .expect_err("root not an ancestor of head");
    assert!(err.to_string().contains("must be an ancestor"), "{err}");

    let err = rebase_stack_onto_manifest(&ctx, &repo, &flags, root, root, onto, MAX_PATHS)
        .await
        .expect_err("empty stack");
    assert!(err.to_string().contains("empty stack"), "{err}");

    let dated = PushrebaseFlags {
        rewritedates: true,
        ..flags
    };
    let err = rebase_stack_onto_manifest(&ctx, &repo, &dated, root, d1, onto, MAX_PATHS)
        .await
        .expect_err("rewritedates on");
    assert!(err.to_string().contains("rewritedates"), "{err}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_remaps_copy_info(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let a = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("file", "content")
        .commit()
        .await?;
    let b = CreateCommitContext::new(&ctx, &repo, vec![a])
        .add_file_with_copy_info("file_renamed", "content", (a, "file"))
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("server_file", "server")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let rebased = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        b,
        onto,
        MAX_PATHS,
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    let new_a = rebased
        .rebased_changesets
        .iter()
        .find(|pair| pair.id_old == a)
        .expect("bottom rebased")
        .id_new;
    let new_head = rebased.new_head.load(&ctx, repo.repo_blobstore()).await?;
    match new_head
        .file_changes_map()
        .get(&NonRootMPath::new("file_renamed")?)
    {
        Some(FileChange::Change(tc)) => assert_eq!(
            tc.copy_from().map(|(path, cs)| (path.clone(), *cs)),
            Some((NonRootMPath::new("file")?, new_a))
        ),
        other => panic!("expected a tracked change, got {other:?}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_post_merge_noop_beside_a_real_change_keeps_merged_content(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", NINE_LINES)
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", NINE_LINES.replace("l9\n", "L9\n"))
        .add_file("g", "g")
        .commit()
        .await?;
    let onto_content = NINE_LINES.replace("l9\n", "L9\n").replace("l1\n", "L1\n");
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", onto_content.clone())
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let rebased = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d1,
        onto,
        MAX_PATHS,
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert!(rebased.dropped.is_empty(), "g is a real change");
    assert_eq!(
        changed_file_content(&ctx, &repo, rebased.new_head, "f").await?,
        Bytes::from(onto_content),
        "the merged content, not the stack's pre-merge content, is what lands"
    );
    assert_eq!(
        rebased.merged_paths.get(&d1).cloned().unwrap_or_default(),
        vec![NonRootMPath::new("f")?],
        "a merge happened on f even though the result equals the destination"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_sees_destination_files_under_a_replaced_directory(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("dir/a", "a")
        .commit()
        .await?;
    // The stack turns `dir` into a file, over two commits since one bonsai
    // cannot hold both paths.
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .delete_file("dir/a")
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![d1])
        .add_file("dir", "now a file")
        .commit()
        .await?;
    // The destination added a file under `dir` the stack never touches.
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("dir/b", "b")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let err = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d2,
        onto,
        MAX_PATHS,
    )
    .await
    .expect_err("replacing a directory the destination grew must conflict");
    assert!(matches!(err, PushrebaseError::Conflicts(_)), "{err}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_merge_resolution_delete_then_readd_still_lands(
    fb: FacebookInit,
) -> Result<(), Error> {
    // Range mode: a deletion on a merge path is applied as-is, as before.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;

    let base_content = "line1\nline2\nline3\nline4\nline5\n";
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file.txt", base_content)
        .commit()
        .await?;
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("file.txt", "modified_line1\nline2\nline3\nline4\nline5\n")
        .commit()
        .await?;
    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;

    let c1 = CreateCommitContext::new(&ctx, &repo, vec![base])
        .delete_file("file.txt")
        .commit()
        .await?;
    let c2 = CreateCommitContext::new(&ctx, &repo, vec![c1])
        .add_file("file.txt", "line1\nline2\nline3\nline4\nmodified_line5\n")
        .commit()
        .await?;
    let c1_bcs = c1.load(&ctx, repo.repo_blobstore()).await?;
    let c2_bcs = c2.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_merge_test();
    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![c1_bcs, c2_bcs],
        &[],
        None,
    )
    .await?;

    let result_hg = repo.derive_hg_changeset(&ctx, result.head).await?;
    ensure_content(
        &ctx,
        result_hg,
        &repo,
        btreemap! {
            "file.txt".to_string() => "modified_line1\nline2\nline3\nline4\nmodified_line5\n".to_string(),
        },
    )
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_disabled_merge_makes_landed_bottom_a_conflict(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", "v0")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", "v1")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", "v1")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let err = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOff),
        root,
        d1,
        onto,
        MAX_PATHS,
    )
    .await
    .expect_err("without merge resolution an overlap is a conflict even when already applied");
    assert!(matches!(err, PushrebaseError::Conflicts(_)), "{err}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_delete_modify_and_add_add_conflict(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let flags = manifest_rebase_flags(MergeResolutionOverride::ForceOn);

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("h", "h")
        .commit()
        .await?;
    // Destination deleted h; the stack modifies it.
    let modifies = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("h", "h2")
        .commit()
        .await?;
    let deleted = CreateCommitContext::new(&ctx, &repo, vec![root])
        .delete_file("h")
        .commit()
        .await?;
    // Both sides add the same new path with different content.
    let adds = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("n", "stack")
        .commit()
        .await?;
    let also_adds = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("n", "destination")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, deleted, also_adds]).await?;

    let err = rebase_stack_onto_manifest(&ctx, &repo, &flags, root, modifies, deleted, MAX_PATHS)
        .await
        .expect_err("delete/modify");
    assert!(matches!(err, PushrebaseError::Conflicts(_)), "{err}");
    let err = rebase_stack_onto_manifest(&ctx, &repo, &flags, root, adds, also_adds, MAX_PATHS)
        .await
        .expect_err("add/add with different content");
    assert!(matches!(err, PushrebaseError::Conflicts(_)), "{err}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_requires_derived_root_too(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    // Backward shape: deriving `onto` does not derive its descendant `root`.
    let onto = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let root = CreateCommitContext::new(&ctx, &repo, vec![onto])
        .add_file("newer", "newer")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("file_a", "a")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[onto]).await?;

    let err = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d1,
        onto,
        MAX_PATHS,
    )
    .await
    .expect_err("root is not derived");
    assert!(
        matches!(err, PushrebaseError::ManifestNotDerived(cs) if cs == root),
        "{err}"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_rejects_too_many_paths(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("base", "base")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("a", "a")
        .add_file("b", "b")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("server_file", "server")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let err = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d1,
        onto,
        1,
    )
    .await
    .expect_err("two paths over a limit of one");
    assert!(
        matches!(
            err,
            PushrebaseError::TooManyPaths {
                paths: 2,
                max_paths: 1
            }
        ),
        "{err}"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn manifest_rebase_with_content_manifests(fb: FacebookInit) -> Result<(), Error> {
    // The override is process-global: carry the full standard map.
    override_just_knobs(JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:pushrebase_enable_merge_resolution".to_string() => KnobVal::Bool(false),
        "scm/mononoke:pushrebase_merge_resolution_derive_fsnodes".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_range_diff_use_content_manifests".to_string() => KnobVal::Bool(false),
    }));
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", NINE_LINES)
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", NINE_LINES.replace("l9\n", "L9\n"))
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", NINE_LINES.replace("l1\n", "L1\n"))
        .commit()
        .await?;
    for cs_id in [root, onto] {
        repo.repo_derived_data()
            .derive::<RootContentManifestId>(&ctx, cs_id, DerivationPriority::HIGH)
            .await?;
    }

    let rebased = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d1,
        onto,
        MAX_PATHS,
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(
        changed_file_content(&ctx, &repo, rebased.new_head, "f").await?,
        Bytes::from(NINE_LINES.replace("l1\n", "L1\n").replace("l9\n", "L9\n"))
    );
    init_just_knobs_for_test();
    Ok(())
}

#[mononoke::fbinit_test]
async fn falsify_manifest_rebase_cascade_double_merge(fb: FacebookInit) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", NINE_LINES)
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", NINE_LINES.replace("l5\n", "D1\n"))
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![d1])
        .add_file(
            "f",
            NINE_LINES.replace("l5\n", "D1\n").replace("l9\n", "D2\n"),
        )
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", NINE_LINES.replace("l1\n", "ONTO\n"))
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;
    let flags = manifest_rebase_flags(MergeResolutionOverride::ForceOn);

    let rebased =
        rebase_stack_onto_manifest(&ctx, &repo, &flags, root, d2, onto, MAX_PATHS).await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert!(rebased.dropped.is_empty());
    let f = NonRootMPath::new("f")?;
    assert_eq!(
        rebased.merged_paths.get(&d1).cloned().unwrap_or_default(),
        vec![f.clone()],
        "the bottom commit merges against onto"
    );
    assert_eq!(
        rebased.merged_paths.get(&d2).cloned().unwrap_or_default(),
        vec![f],
        "the second commit merges again against the first commit's merged content"
    );
    assert_eq!(
        changed_file_content(&ctx, &repo, rebased.new_head, "f").await?,
        Bytes::from(
            NINE_LINES
                .replace("l1\n", "ONTO\n")
                .replace("l5\n", "D1\n")
                .replace("l9\n", "D2\n")
        ),
        "all three edits survive the cascade"
    );
    let new_d1 = rebased
        .rebased_changesets
        .iter()
        .find(|pair| pair.id_old == d1)
        .expect("bottom rebased")
        .id_new;
    assert_eq!(
        changed_file_content(&ctx, &repo, new_d1, "f").await?,
        Bytes::from(NINE_LINES.replace("l1\n", "ONTO\n").replace("l5\n", "D1\n")),
        "the intermediate commit carries onto's edit plus its own, not the whole stack"
    );
    match rebased.merge_summary {
        MergeResolutionSummary::Succeeded {
            conflict_files_count,
            resolved_files_count,
            ..
        } => {
            assert_eq!((conflict_files_count, resolved_files_count), (1, 1));
        }
        other => panic!("expected Succeeded, got {other:?}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn falsify_manifest_rebase_merging_stack_is_deterministic(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", NINE_LINES)
        .add_file("landed", "v0")
        .commit()
        .await?;
    // Bottom is already in onto and will be dropped.
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("landed", "v1")
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![d1])
        .add_file("f", NINE_LINES.replace("l5\n", "D2\n"))
        .commit()
        .await?;
    let d3 = CreateCommitContext::new(&ctx, &repo, vec![d2])
        .add_file(
            "f",
            NINE_LINES.replace("l5\n", "D2\n").replace("l9\n", "D3\n"),
        )
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("landed", "v1")
        .add_file("f", NINE_LINES.replace("l1\n", "ONTO\n"))
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;
    let flags = manifest_rebase_flags(MergeResolutionOverride::ForceOn);

    let first = rebase_stack_onto_manifest(&ctx, &repo, &flags, root, d3, onto, MAX_PATHS).await?;
    changesets_creation::save_changesets(&ctx, &repo, first.rebased_bonsais.clone()).await?;
    assert_eq!(first.dropped, vec![d1]);
    assert!(
        !first.merged_paths.is_empty(),
        "the run must actually merge, or determinism proves nothing"
    );

    let second = rebase_stack_onto_manifest(&ctx, &repo, &flags, root, d3, onto, MAX_PATHS).await?;
    assert_eq!(second.new_head, first.new_head, "same head");
    assert_eq!(second.dropped, first.dropped, "same drops");
    assert_eq!(second.merged_paths, first.merged_paths, "same merged paths");
    let pairs = |stack: &RebasedStack| {
        let mut v: Vec<_> = stack
            .rebased_changesets
            .iter()
            .map(|p| (p.id_old, p.id_new))
            .collect();
        v.sort_unstable();
        v
    };
    assert_eq!(pairs(&second), pairs(&first), "same old->new mapping");
    Ok(())
}

#[mononoke::fbinit_test]
async fn falsify_manifest_rebase_first_applied_then_later_change(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", "v0")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", "v1")
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![d1])
        .add_file("f", "v2")
        .commit()
        .await?;
    // onto has d1's change and an unrelated one, so base != onto at `f`.
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", "v1")
        .add_file("g", "g")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let rebased = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d2,
        onto,
        MAX_PATHS,
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(
        rebased.dropped,
        vec![d1],
        "the already-applied bottom drops"
    );
    assert!(
        rebased.merged_paths.is_empty(),
        "the later change needs no merge: onto's state is the stack's own earlier state"
    );
    assert_eq!(
        changed_file_content(&ctx, &repo, rebased.new_head, "f").await?,
        Bytes::from("v2"),
        "the later change lands verbatim"
    );
    match rebased.merge_summary {
        MergeResolutionSummary::Succeeded {
            conflict_files_count,
            resolved_files_count,
            ..
        } => assert_eq!(
            (conflict_files_count, resolved_files_count),
            (1, 0),
            "the path overlapped but nothing was merged"
        ),
        other => panic!("expected Succeeded, got {other:?}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn falsify_manifest_rebase_add_then_delete_over_identical_add(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("keep", "keep")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("n", "X")
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![d1])
        .delete_file("n")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("n", "X")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let rebased = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d2,
        onto,
        MAX_PATHS,
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(rebased.dropped, vec![d1], "the add is already in onto");
    assert!(rebased.merged_paths.is_empty());
    let head = rebased.new_head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(head.parents().collect::<Vec<_>>(), vec![onto]);
    assert_eq!(
        head.file_changes_map().get(&NonRootMPath::new("n")?),
        Some(&FileChange::Deletion),
        "the deletion still applies against the destination's copy"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn falsify_manifest_rebase_delete_then_readd_over_identical_delete(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("keep", "keep")
        .add_file("doomed", "x")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .delete_file("doomed")
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![d1])
        .add_file("doomed", "reborn")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .delete_file("doomed")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let rebased = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d2,
        onto,
        MAX_PATHS,
    )
    .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    assert_eq!(rebased.dropped, vec![d1], "the delete is already in onto");
    assert!(rebased.merged_paths.is_empty());
    assert_eq!(
        changed_file_content(&ctx, &repo, rebased.new_head, "doomed").await?,
        Bytes::from("reborn"),
        "the re-add lands as a plain rebase"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn falsify_manifest_rebase_deletion_anywhere_in_the_stack_conflicts(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let flags = manifest_rebase_flags(MergeResolutionOverride::ForceOn);

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", NINE_LINES)
        .commit()
        .await?;
    // onto edits a line the stack does not, so a 3-way merge would be clean.
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", NINE_LINES.replace("l1\n", "ONTO\n"))
        .commit()
        .await?;
    // modify, then delete.
    let m1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", NINE_LINES.replace("l9\n", "L9\n"))
        .commit()
        .await?;
    let m2 = CreateCommitContext::new(&ctx, &repo, vec![m1])
        .delete_file("f")
        .commit()
        .await?;
    // delete, then re-add.
    let r1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .delete_file("f")
        .commit()
        .await?;
    let r2 = CreateCommitContext::new(&ctx, &repo, vec![r1])
        .add_file("f", "reborn")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let err = rebase_stack_onto_manifest(&ctx, &repo, &flags, root, m2, onto, MAX_PATHS)
        .await
        .expect_err("a deletion later in the stack blocks the merge of the earlier change");
    assert!(matches!(err, PushrebaseError::Conflicts(_)), "{err}");

    let err = rebase_stack_onto_manifest(&ctx, &repo, &flags, root, r2, onto, MAX_PATHS)
        .await
        .expect_err("delete-then-re-add against a modified destination is a conflict");
    assert!(matches!(err, PushrebaseError::Conflicts(_)), "{err}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn falsify_manifest_rebase_copy_source_changed_in_onto(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let flags = manifest_rebase_flags(MergeResolutionOverride::ForceOn);

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("src", NINE_LINES)
        .commit()
        .await?;
    // The stack copies `src` without touching it.
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file_with_copy_info("dst", NINE_LINES, (root, "src"))
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("src", NINE_LINES.replace("l1\n", "ONTO\n"))
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let err = rebase_stack_onto_manifest(&ctx, &repo, &flags, root, d1, onto, MAX_PATHS)
        .await
        .expect_err("the copy source is in the stack's path set and the destination moved it");
    match err {
        PushrebaseError::Conflicts(conflicts) => assert_eq!(
            conflicts,
            vec![PushrebaseConflict {
                left: MPath::new("src")?,
                right: MPath::new("src")?,
            }],
            "the conflict names the copy source, not the copy destination"
        ),
        other => panic!("expected Conflicts, got {other}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn falsify_manifest_rebase_file_type_change_on_overlap(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;
    let flags = manifest_rebase_flags(MergeResolutionOverride::ForceOn);

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", NINE_LINES)
        .commit()
        .await?;
    // Destination only flips the executable bit; content is untouched.
    let exec_onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file_with_type("f", NINE_LINES, FileType::Executable)
        .commit()
        .await?;
    // Stack only edits content, keeping the type.
    let edit = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", NINE_LINES.replace("l9\n", "L9\n"))
        .commit()
        .await?;
    // Mirror: the stack flips the bit and the destination edits content.
    let exec_stack = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file_with_type("f", NINE_LINES, FileType::Executable)
        .commit()
        .await?;
    let edit_onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", NINE_LINES.replace("l1\n", "ONTO\n"))
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, exec_onto, edit_onto]).await?;

    let err = rebase_stack_onto_manifest(&ctx, &repo, &flags, root, edit, exec_onto, MAX_PATHS)
        .await
        .expect_err("a type-only change in the destination is not mergeable");
    assert!(matches!(err, PushrebaseError::Conflicts(_)), "{err}");

    let err =
        rebase_stack_onto_manifest(&ctx, &repo, &flags, root, exec_stack, edit_onto, MAX_PATHS)
            .await
            .expect_err("a type-only change in the stack is not mergeable either");
    assert!(matches!(err, PushrebaseError::Conflicts(_)), "{err}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn falsify_manifest_rebase_already_applied_file_to_dir_replacement(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("p", "a file")
        .commit()
        .await?;
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .delete_file("p")
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![d1])
        .add_file("p/c", "c")
        .commit()
        .await?;
    // The destination already has exactly the stack's result.
    let onto1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .delete_file("p")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![onto1])
        .add_file("p/c", "c")
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let err = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d2,
        onto,
        MAX_PATHS,
    )
    .await
    .expect_err("a path that is a file in base and a directory in onto cannot be merged");
    match err {
        PushrebaseError::Conflicts(conflicts) => assert_eq!(
            conflicts,
            vec![PushrebaseConflict {
                left: MPath::new("p")?,
                right: MPath::new("p")?,
            }],
            "only the swapped path is reported; `p/c` collapses onto its prefix"
        ),
        other => panic!("expected Conflicts, got {other}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn falsify_manifest_rebase_dropped_commit_reports_no_merged_paths(
    fb: FacebookInit,
) -> Result<(), Error> {
    init_just_knobs_for_test();
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = TestRepoFactory::new(fb)?.build().await?;

    let root = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("f", NINE_LINES)
        .commit()
        .await?;
    // d1 merges to exactly onto's content, so it is dropped after the merge.
    let d1 = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file("f", NINE_LINES.replace("l9\n", "L9\n"))
        .commit()
        .await?;
    let d2 = CreateCommitContext::new(&ctx, &repo, vec![d1])
        .add_file("g", "g")
        .commit()
        .await?;
    let onto = CreateCommitContext::new(&ctx, &repo, vec![root])
        .add_file(
            "f",
            NINE_LINES.replace("l9\n", "L9\n").replace("l1\n", "L1\n"),
        )
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[root, onto]).await?;

    let rebased = rebase_stack_onto_manifest(
        &ctx,
        &repo,
        &manifest_rebase_flags(MergeResolutionOverride::ForceOn),
        root,
        d2,
        onto,
        MAX_PATHS,
    )
    .await?;
    assert_eq!(rebased.dropped, vec![d1]);
    assert!(
        !rebased.merged_paths.contains_key(&d1),
        "a dropped commit has no successor to attribute merged paths to"
    );
    assert!(rebased.merged_paths.is_empty());
    match rebased.merge_summary {
        MergeResolutionSummary::Succeeded {
            conflict_files_count,
            resolved_files_count,
            ref resolved_paths_sample,
        } => {
            assert_eq!((conflict_files_count, resolved_files_count), (1, 0));
            assert!(resolved_paths_sample.is_empty());
        }
        ref other => panic!("expected Succeeded, got {other:?}"),
    }
    Ok(())
}

// ==================== merge_resolution_take_incoming_prefixes ====================

/// Merge resolution on, with the take-incoming knob set as given.
fn init_just_knobs_for_take_incoming_test(enabled: bool) {
    override_just_knobs(JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:pushrebase_enable_merge_resolution".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_merge_resolution_derive_fsnodes".to_string() => KnobVal::Bool(true),
        "scm/mononoke:pushrebase_range_diff_use_content_manifests".to_string() => KnobVal::Bool(false),
        "scm/mononoke:pushrebase_take_incoming_enabled".to_string() => KnobVal::Bool(enabled),
        "scm/mononoke:pushrebase_max_merge_conflicts".to_string() => KnobVal::Int(100),
        // Below the APK payloads' size on purpose: an LFS pointer taken
        // verbatim is never fetched, so the limit must not apply to it.
        "scm/mononoke:pushrebase_max_merge_file_size".to_string() => KnobVal::Int(2_000),
    }));
}

fn take_incoming_flags(prefixes: &[&str]) -> PushrebaseFlags {
    PushrebaseFlags {
        merge_resolution_take_incoming_prefixes: prefixes
            .iter()
            .map(|p| MPath::from(NonRootMPath::new(p).unwrap()))
            .collect(),
        ..Default::default()
    }
}

/// A prebuilt manifest with one multi-line entry per artifact, in the
/// shape the AOSP prebuilt repos use.
fn manifest(a_build: u32, b_build: u32) -> String {
    format!(
        "{{\n    \"ArtA\": {{\n        \"BUILD\": {a_build},\n        \"HASH\": \"a{a_build}\"\n    }},\n    \"ArtB\": {{\n        \"BUILD\": {b_build},\n        \"HASH\": \"b{b_build}\"\n    }}\n}}\n"
    )
}

/// APK payload above the test's merge size limit.
fn apk(version: u32) -> String {
    format!("apk v{version} ").repeat(400)
}

/// Base with artifacts A and B; server bumps both A (to 2) and B (to 2);
/// client, from the same base, bumps only A (to 3). A same-artifact race.
async fn take_incoming_race(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
) -> Result<(BookmarkKey, BonsaiChangeset), Error> {
    let lfs = GitLfs::canonical_pointer();
    let base = CreateCommitContext::new_root(ctx, repo)
        .add_file("prebuilt_manifest.json", manifest(1, 1))
        .add_file_with_type_and_lfs("ArtA/pkg.apk", apk(1), FileType::Regular, lfs)
        .add_file_with_type_and_lfs("ArtB/pkg.apk", apk(1), FileType::Regular, lfs)
        .add_file("README", "v1\n")
        .commit()
        .await?;
    let server = CreateCommitContext::new(ctx, repo, vec![base])
        .add_file("prebuilt_manifest.json", manifest(2, 2))
        .add_file_with_type_and_lfs("ArtA/pkg.apk", apk(2), FileType::Regular, lfs)
        .add_file_with_type_and_lfs("ArtB/pkg.apk", apk(2), FileType::Regular, lfs)
        .commit()
        .await?;
    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(ctx, server).await?;
    set_bookmark(ctx.clone(), repo, &book, &format!("{hg_server}")).await?;

    let client = CreateCommitContext::new(ctx, repo, vec![base])
        .add_file("prebuilt_manifest.json", manifest(3, 1))
        .add_file_with_type_and_lfs("ArtA/pkg.apk", apk(3), FileType::Regular, lfs)
        .commit()
        .await?;
    Ok((book, client.load(ctx, repo.repo_blobstore()).await?))
}

#[mononoke::fbinit_test]
async fn pushrebase_take_incoming_resolves_same_artifact_race(
    fb: FacebookInit,
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let (book, client_bcs) = take_incoming_race(&ctx, &repo).await?;

    init_just_knobs_for_take_incoming_test(true);
    let flags = take_incoming_flags(&["ArtA", "prebuilt_manifest.json"]);
    let result =
        do_pushrebase_bonsai(&ctx, &repo, &flags, &book, &hashset![client_bcs], &[], None).await?;

    // ArtA is the client's (incoming wins); ArtB, which only the server
    // changed, is the server's; the pointer is the client's, still LFS.
    let result_hg = repo.derive_hg_changeset(&ctx, result.head).await?;
    ensure_content(
        &ctx,
        result_hg,
        &repo,
        btreemap! {
            "prebuilt_manifest.json".to_string() => manifest(3, 2),
            "ArtA/pkg.apk".to_string() => apk(3),
            "ArtB/pkg.apk".to_string() => apk(2),
            "README".to_string() => "v1\n".to_string(),
        },
    )
    .await?;
    let result_bcs = result.head.load(&ctx, repo.repo_blobstore()).await?;
    match result_bcs
        .file_changes_map()
        .get(&NonRootMPath::new("ArtA/pkg.apk")?)
    {
        Some(FileChange::Change(tc)) => assert!(tc.git_lfs().is_lfs_pointer()),
        other => panic!("unexpected ArtA/pkg.apk change: {other:?}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_take_incoming_off_is_todays_conflict(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let (book, client_bcs) = take_incoming_race(&ctx, &repo).await?;

    // Prefixes configured but knob off: the LFS conflict rejects as before.
    init_just_knobs_for_take_incoming_test(false);
    let flags = take_incoming_flags(&["ArtA", "prebuilt_manifest.json"]);
    let res = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &flags,
        &book,
        &hashset![client_bcs.clone()],
        &[],
        None,
    )
    .await;
    assert!(matches!(res, Err(PushrebaseError::Conflicts(_))), "{res:?}");

    // Knob on but neither requested nor prefixes configured: likewise, and
    // the knob is not read.
    init_just_knobs_for_merge_test();
    let res = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &Default::default(),
        &book,
        &hashset![client_bcs],
        &[],
        None,
    )
    .await;
    assert!(matches!(res, Err(PushrebaseError::Conflicts(_))), "{res:?}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_take_incoming_rejects_conflict_outside_prefixes(
    fb: FacebookInit,
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let (book, client_bcs) = take_incoming_race(&ctx, &repo).await?;

    // Only the manifest is covered: the APK conflict is outside the prefixes
    // and the whole land rejects, exactly as today.
    init_just_knobs_for_take_incoming_test(true);
    let flags = take_incoming_flags(&["prebuilt_manifest.json"]);
    let res =
        do_pushrebase_bonsai(&ctx, &repo, &flags, &book, &hashset![client_bcs], &[], None).await;
    assert!(matches!(res, Err(PushrebaseError::Conflicts(_))), "{res:?}");
    Ok(())
}

/// The caller asked for the pushed side (the pushvar), no prefixes configured.
fn take_incoming_requested_flags() -> PushrebaseFlags {
    PushrebaseFlags {
        merge_resolution_take_incoming: true,
        ..Default::default()
    }
}

#[mononoke::fbinit_test]
async fn pushrebase_take_incoming_requested_resolves_without_prefixes(
    fb: FacebookInit,
) -> Result<(), Error> {
    // Same race, no path list: the request alone covers the APK (verbatim)
    // and the manifest (prefer incoming), and leaves the untouched ArtB and
    // README as the server has them.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let (book, client_bcs) = take_incoming_race(&ctx, &repo).await?;

    init_just_knobs_for_take_incoming_test(true);
    let flags = take_incoming_requested_flags();
    let result =
        do_pushrebase_bonsai(&ctx, &repo, &flags, &book, &hashset![client_bcs], &[], None).await?;

    let result_hg = repo.derive_hg_changeset(&ctx, result.head).await?;
    ensure_content(
        &ctx,
        result_hg,
        &repo,
        btreemap! {
            "prebuilt_manifest.json".to_string() => manifest(3, 2),
            "ArtA/pkg.apk".to_string() => apk(3),
            "ArtB/pkg.apk".to_string() => apk(2),
            "README".to_string() => "v1\n".to_string(),
        },
    )
    .await?;
    let result_bcs = result.head.load(&ctx, repo.repo_blobstore()).await?;
    match result_bcs
        .file_changes_map()
        .get(&NonRootMPath::new("ArtA/pkg.apk")?)
    {
        Some(FileChange::Change(tc)) => assert!(tc.git_lfs().is_lfs_pointer()),
        other => panic!("unexpected ArtA/pkg.apk change: {other:?}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_take_incoming_requested_needs_the_knob(fb: FacebookInit) -> Result<(), Error> {
    // The request is honoured only where the repo knob allows it: knob off,
    // the race rejects exactly as today.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let (book, client_bcs) = take_incoming_race(&ctx, &repo).await?;

    init_just_knobs_for_take_incoming_test(false);
    let flags = take_incoming_requested_flags();
    let res =
        do_pushrebase_bonsai(&ctx, &repo, &flags, &book, &hashset![client_bcs], &[], None).await;
    assert!(matches!(res, Err(PushrebaseError::Conflicts(_))), "{res:?}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_take_incoming_requested_still_rejects_both_sides_added(
    fb: FacebookInit,
) -> Result<(), Error> {
    // `-X theirs` resolves content conflicts only: a path new on both sides
    // has no base and is not a merge, so the request does not cover it.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let lfs = GitLfs::canonical_pointer();
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("prebuilt_manifest.json", manifest(1, 1))
        .commit()
        .await?;
    let server = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file_with_type_and_lfs("ArtC/pkg.apk", apk(2), FileType::Regular, lfs)
        .commit()
        .await?;
    let book = BookmarkKey::new("master")?;
    let hg_server = repo.derive_hg_changeset(&ctx, server).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_server}")).await?;
    let client = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file_with_type_and_lfs("ArtC/pkg.apk", apk(3), FileType::Regular, lfs)
        .commit()
        .await?;
    let client_bcs = client.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_take_incoming_test(true);
    let flags = take_incoming_requested_flags();
    let res =
        do_pushrebase_bonsai(&ctx, &repo, &flags, &book, &hashset![client_bcs], &[], None).await;
    assert!(matches!(res, Err(PushrebaseError::Conflicts(_))), "{res:?}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_take_incoming_rejects_already_landed_and_superseded(
    fb: FacebookInit,
) -> Result<(), Error> {
    // The client's exact APK bytes reached the branch earlier (its own land,
    // response lost) and a newer build then landed. A retry must not take
    // the stale bytes back over the newer ones.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let lfs = GitLfs::canonical_pointer();
    let base = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("prebuilt_manifest.json", manifest(1, 1))
        .add_file_with_type_and_lfs("ArtA/pkg.apk", apk(1), FileType::Regular, lfs)
        .commit()
        .await?;
    let landed = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("prebuilt_manifest.json", manifest(3, 1))
        .add_file_with_type_and_lfs("ArtA/pkg.apk", apk(3), FileType::Regular, lfs)
        .commit()
        .await?;
    let newer = CreateCommitContext::new(&ctx, &repo, vec![landed])
        .add_file("prebuilt_manifest.json", manifest(4, 1))
        .add_file_with_type_and_lfs("ArtA/pkg.apk", apk(4), FileType::Regular, lfs)
        .commit()
        .await?;
    let book = BookmarkKey::new("master")?;
    let hg_newer = repo.derive_hg_changeset(&ctx, newer).await?;
    set_bookmark(ctx.clone(), &repo, &book, &format!("{hg_newer}")).await?;

    let retry = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("prebuilt_manifest.json", manifest(3, 1))
        .add_file_with_type_and_lfs("ArtA/pkg.apk", apk(3), FileType::Regular, lfs)
        .commit()
        .await?;
    let retry_bcs = retry.load(&ctx, repo.repo_blobstore()).await?;

    init_just_knobs_for_take_incoming_test(true);
    let flags = take_incoming_flags(&["ArtA", "prebuilt_manifest.json"]);
    let res = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &flags,
        &book,
        &hashset![retry_bcs.clone()],
        &[],
        None,
    )
    .await;
    assert!(matches!(res, Err(PushrebaseError::Conflicts(_))), "{res:?}");

    // And for that reason: without the check the manifest would merge (3
    // over 4 is a clean prefer-incoming) and the pointer would be taken.
    let apk_path = NonRootMPath::new("ArtA/pkg.apk")?;
    let conflicts = vec![PushrebaseConflict::new(
        apk_path.clone().into(),
        apk_path.into(),
    )];
    let server_bcs = fetch_bonsai_range_ancestor_not_included(&ctx, &repo, base, newer).await?;
    let no_excluded = PrefixTrie::new();
    let err = collect_merge_file_info(
        &ctx,
        &repo,
        &conflicts,
        base,
        Bases::ProbeThenRead,
        &server_bcs,
        &[retry_bcs],
        100,
        2_000,
        true,
        &no_excluded,
        Some(TakeIncomingScope::Prefixes(
            &flags.merge_resolution_take_incoming_prefixes,
        )),
    )
    .await
    .expect_err("the superseded pointer must be rejected");
    match err {
        MergeResolutionError::Skipped(reason) => {
            assert!(reason.contains("already landed and superseded"), "{reason}")
        }
        other => panic!("unexpected error: {other}"),
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn pushrebase_take_incoming_manifest_mode_keeps_todays_outcome(
    fb: FacebookInit,
) -> Result<(), Error> {
    // Off-branch baselines go through the manifest-diff rebase, which has
    // no server-side range to rule out superseded content, so a pointer
    // under a take-incoming prefix still conflicts there.
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(fb).await?;
    let (book, client_bcs) = take_incoming_race(&ctx, &repo).await?;
    let client = client_bcs.get_changeset_id();
    let base = client_bcs.parents().next().expect("client has a parent");
    let server = get_bookmark_value(&ctx, &repo, &book)
        .await?
        .expect("server head set");
    derive_content_manifests(&ctx, &repo, &[base, server]).await?;

    init_just_knobs_for_take_incoming_test(true);
    let flags = PushrebaseFlags {
        rewritedates: false,
        merge_resolution_override: MergeResolutionOverride::ForceOn,
        ..take_incoming_flags(&["ArtA", "prebuilt_manifest.json"])
    };
    let res =
        rebase_stack_onto_manifest(&ctx, &repo, &flags, base, client, server, MAX_PATHS).await;
    assert!(matches!(res, Err(PushrebaseError::Conflicts(_))), "{res:?}");

    // A text path under the prefix that merges cleanly still does: the
    // prefix never makes a clean merge worse. Server bumps only ArtB.
    let lfs = GitLfs::canonical_pointer();
    let server_b_only = CreateCommitContext::new(&ctx, &repo, vec![base])
        .add_file("prebuilt_manifest.json", manifest(1, 2))
        .add_file_with_type_and_lfs("ArtB/pkg.apk", apk(2), FileType::Regular, lfs)
        .commit()
        .await?;
    derive_content_manifests(&ctx, &repo, &[server_b_only]).await?;
    let rebased =
        rebase_stack_onto_manifest(&ctx, &repo, &flags, base, client, server_b_only, MAX_PATHS)
            .await?;
    changesets_creation::save_changesets(&ctx, &repo, rebased.rebased_bonsais.clone()).await?;
    let new_hg = repo.derive_hg_changeset(&ctx, rebased.new_head).await?;
    ensure_content(
        &ctx,
        new_hg,
        &repo,
        btreemap! {
            "prebuilt_manifest.json".to_string() => manifest(3, 2),
            "ArtA/pkg.apk".to_string() => apk(3),
            "ArtB/pkg.apk".to_string() => apk(2),
            "README".to_string() => "v1\n".to_string(),
        },
    )
    .await?;
    Ok(())
}

/// Trunk is A-B-C with F a draft child of A, and M merges F into B, so a push
/// of {F, M} has the candidate roots A and B, and B is its merge base with
/// the trunk.
struct RequiredRootMerge {
    a: ChangesetId,
    b: ChangesetId,
    c: ChangesetId,
    f: ChangesetId,
    m: ChangesetId,
    pushed: HashSet<BonsaiChangeset>,
}

async fn required_root_merge(
    ctx: &CoreContext,
    repo: &PushrebaseTestRepo,
) -> Result<RequiredRootMerge, Error> {
    init_just_knobs_for_test();
    let a = CreateCommitContext::new_root(ctx, repo)
        .add_file("a", "a")
        .commit()
        .await?;
    let b = CreateCommitContext::new(ctx, repo, vec![a])
        .add_file("b", "b")
        .commit()
        .await?;
    let c = CreateCommitContext::new(ctx, repo, vec![b])
        .add_file("c", "c")
        .commit()
        .await?;
    let f = CreateCommitContext::new(ctx, repo, vec![a])
        .add_file("f", "f")
        .commit()
        .await?;
    let m = CreateCommitContext::new(ctx, repo, vec![b, f])
        .add_file("m", "m")
        .commit()
        .await?;
    let pushed = hashset![
        f.load(ctx, repo.repo_blobstore()).await?,
        m.load(ctx, repo.repo_blobstore()).await?,
    ];
    Ok(RequiredRootMerge {
        a,
        b,
        c,
        f,
        m,
        pushed,
    })
}

fn assert_bookmark_moved(
    result: Result<impl std::fmt::Debug, PushrebaseError>,
    expected_root: ChangesetId,
) {
    match result {
        Err(PushrebaseError::BookmarkMoved { required_root, .. }) => {
            assert_eq!(required_root, expected_root)
        }
        other => panic!("expected BookmarkMoved, got {other:?}"),
    }
}

#[mononoke::fbinit_test]
async fn required_root_rejects_another_closest_root(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    let merge = required_root_merge(&ctx, &repo).await?;
    let book = master_bookmark();

    // With the bookmark at A, A is the closest root, so B is not the root
    // pushrebase would rebase from.
    bookmark(&ctx, &repo, book.clone()).set_to(merge.a).await?;
    let result = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &PushrebaseFlags::default(),
        &book,
        &merge.pushed,
        &[],
        Some(merge.b),
    )
    .await;
    assert_bookmark_moved(result, merge.b);
    assert_eq!(resolve_cs_id(&ctx, &repo, "master").await?, merge.a);
    Ok(())
}

#[mononoke::fbinit_test]
async fn required_root_lands_from_it(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    let merge = required_root_merge(&ctx, &repo).await?;
    let book = master_bookmark();

    // The bookmark moved forward to C: only M is rebased, and F lands
    // unchanged as its second parent.
    bookmark(&ctx, &repo, book.clone()).set_to(merge.c).await?;
    let outcome = do_pushrebase_bonsai(
        &ctx,
        &repo,
        &PushrebaseFlags::default(),
        &book,
        &merge.pushed,
        &[],
        Some(merge.b),
    )
    .await?;
    assert_eq!(
        outcome
            .rebased_changesets
            .iter()
            .map(|pair| pair.id_old)
            .collect::<Vec<_>>(),
        vec![merge.m]
    );
    let head = outcome.head.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(head.parents().collect::<Vec<_>>(), vec![merge.c, merge.f]);
    assert_eq!(resolve_cs_id(&ctx, &repo, "master").await?, outcome.head);
    Ok(())
}

#[mononoke::fbinit_test]
async fn required_root_rejects_bookmark_moved_back_before_landing(
    fb: FacebookInit,
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: PushrebaseTestRepo = test_repo_factory::build_empty(ctx.fb).await?;
    let merge = required_root_merge(&ctx, &repo).await?;
    let book = master_bookmark();
    let config = PushrebaseFlags::default();

    // The stack is indexed with the bookmark at B, then the bookmark moves
    // back to A before it lands.
    bookmark(&ctx, &repo, book.clone()).set_to(merge.b).await?;
    let mut stack = index_pushrebase_request(&ctx, &repo, &config, &book, &merge.pushed).await?;
    stack.require_root(&book, merge.b)?;
    bookmark(&ctx, &repo, book.clone()).set_to(merge.a).await?;

    let result = rebase_in_loop(&ctx, &repo, &config, &book, &stack, &[]).await;
    assert_bookmark_moved(result, merge.b);
    assert_eq!(resolve_cs_id(&ctx, &repo, "master").await?, merge.a);

    let (tx, rx) = oneshot::channel();
    let requeued = do_batched_pushrebase(
        &ctx,
        &repo,
        &config,
        &book,
        vec![queued_pushrebase_request(&ctx, stack, tx)],
        &[],
    )
    .await;
    assert!(requeued.is_empty());
    let result = rx.await?.map_err(|e| match e.inner() {
        PushrebaseError::BookmarkMoved {
            bookmark,
            required_root,
        } => PushrebaseError::BookmarkMoved {
            bookmark: bookmark.clone(),
            required_root: *required_root,
        },
        other => PushrebaseError::Error(format_err!("{other:?}")),
    });
    assert_bookmark_moved(result, merge.b);
    assert_eq!(resolve_cs_id(&ctx, &repo, "master").await?, merge.a);
    Ok(())
}
