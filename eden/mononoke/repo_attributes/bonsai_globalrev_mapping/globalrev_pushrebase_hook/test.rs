/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::time::Duration;

use anyhow::Error;
use async_trait::async_trait;
use blobstore::Loadable;
use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
use bonsai_globalrev_mapping::BonsaiGlobalrevMappingArc;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::BookmarkTransactionError;
use bookmarks::Bookmarks;
use borrowed::borrowed;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use context::CoreContext;
use fbinit::FacebookInit;
use filestore::FilestoreConfig;
use futures::future::try_join_all;
use maplit::hashset;
use mononoke_macros::mononoke;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::ChangesetId;
use mononoke_types::RepositoryId;
use mononoke_types::globalrev::Globalrev;
use mononoke_types::globalrev::START_COMMIT_GLOBALREV;
use pushrebase::do_pushrebase_bonsai;
use pushrebase_hook::PushrebaseCommitHook;
use pushrebase_hook::PushrebaseHook;
use pushrebase_hook::PushrebaseTransactionHook;
use pushrebase_hook::RebasedChangesets;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedData;
use repo_identity::RepoIdentity;
use repo_identity::RepoIdentityRef;
use sql_ext::Transaction;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;
use tests_utils::bookmark;
use tests_utils::resolve_cs_id;

use crate::GlobalrevPushrebaseHook;

#[facet::container]
#[derive(Clone)]
struct Repo {
    #[facet]
    repo_identity: RepoIdentity,

    #[facet]
    bonsai_globalrev_mapping: dyn BonsaiGlobalrevMapping,

    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    repo_derived_data: RepoDerivedData,

    #[facet]
    repo_blobstore: RepoBlobstore,

    #[facet]
    commit_graph: CommitGraph,

    #[facet]
    commit_graph_writer: dyn CommitGraphWriter,

    #[facet]
    filestore_config: FilestoreConfig,

    #[facet]
    bookmarks: dyn Bookmarks,
}

#[mononoke::fbinit_test]
async fn pushrebase_assigns_globalrevs(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = TestRepoFactory::new(fb)?
        .with_id(RepositoryId::new(1))
        .build()
        .await?;
    borrowed!(ctx, repo);

    let root = CreateCommitContext::new_root(ctx, repo).commit().await?;

    let cs1 = CreateCommitContext::new(ctx, repo, vec![root])
        .commit()
        .await?;

    let cs2 = CreateCommitContext::new(ctx, repo, vec![root])
        .commit()
        .await?
        .load(ctx, repo.repo_blobstore())
        .await?;

    let book = bookmark(ctx, repo, "master").set_to(cs1).await?;

    let hooks = [GlobalrevPushrebaseHook::new(
        repo.bonsai_globalrev_mapping_arc(),
        repo.repo_identity().id(),
        None,
    )];

    let rebased = do_pushrebase_bonsai(
        ctx,
        repo,
        &Default::default(),
        &book,
        &hashset![cs2.clone()],
        &hooks,
        None,
    )
    .await?
    .rebased_changesets;

    let cs2_rebased = rebased
        .iter()
        .find(|e| e.id_old == cs2.get_changeset_id())
        .ok_or_else(|| Error::msg("missing cs2"))?
        .id_new
        .load(ctx, repo.repo_blobstore())
        .await?;

    let cs3 = CreateCommitContext::new(ctx, repo, vec![root])
        .commit()
        .await?
        .load(ctx, repo.repo_blobstore())
        .await?;

    let cs4 = CreateCommitContext::new(ctx, repo, vec![cs3.get_changeset_id()])
        .commit()
        .await?
        .load(ctx, repo.repo_blobstore())
        .await?;

    let rebased = do_pushrebase_bonsai(
        ctx,
        repo,
        &Default::default(),
        &book,
        &hashset![cs3.clone(), cs4.clone()],
        &hooks,
        None,
    )
    .await?
    .rebased_changesets;

    let cs3_rebased = rebased
        .iter()
        .find(|e| e.id_old == cs3.get_changeset_id())
        .ok_or_else(|| Error::msg("missing cs3"))?
        .id_new
        .load(ctx, repo.repo_blobstore())
        .await?;

    let cs4_rebased = rebased
        .iter()
        .find(|e| e.id_old == cs4.get_changeset_id())
        .ok_or_else(|| Error::msg("missing cs4"))?
        .id_new
        .load(ctx, repo.repo_blobstore())
        .await?;

    assert_eq!(
        Globalrev::new(START_COMMIT_GLOBALREV),
        Globalrev::from_bcs(&cs2_rebased)?
    );
    assert_eq!(
        Globalrev::new(START_COMMIT_GLOBALREV + 1),
        Globalrev::from_bcs(&cs3_rebased)?
    );
    assert_eq!(
        Globalrev::new(START_COMMIT_GLOBALREV + 2),
        Globalrev::from_bcs(&cs4_rebased)?
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_pushrebase_race_assigns_monotonic_globalrevs(fb: FacebookInit) -> Result<(), Error> {
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

    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = TestRepoFactory::new(fb)?
        .with_id(RepositoryId::new(1))
        .build()
        .await?;
    borrowed!(ctx, repo);

    let root = CreateCommitContext::new_root(ctx, repo).commit().await?;
    let book = bookmark(ctx, repo, "master").set_to(root).await?;

    let hooks = [
        GlobalrevPushrebaseHook::new(
            repo.bonsai_globalrev_mapping_arc(),
            repo.repo_identity().id(),
            None,
        ),
        Box::new(SleepHook) as Box<dyn PushrebaseHook>,
    ];

    let n: u64 = 10;
    let mut futs = vec![];
    for _ in 0..n {
        let bcs = CreateCommitContext::new(ctx, repo, vec![root])
            .commit()
            .await?
            .load(ctx, repo.repo_blobstore())
            .await?;

        let fut = async {
            let params = Default::default();
            let bcss = hashset![bcs];
            do_pushrebase_bonsai(ctx, repo, &params, &book, &bcss, &hooks, None).await
        };

        futs.push(fut);
    }

    let results = try_join_all(futs).await?;

    // Check that we had some retries
    assert!(results.iter().fold(0, |sum, e| sum + e.retry_num.0) > 0);

    let mut next_cs_id = resolve_cs_id(ctx, repo, "master").await?;
    let mut next_globalrev = START_COMMIT_GLOBALREV + n - 1;

    // Check history backwards and assert that globalrevs are going down

    loop {
        if next_cs_id == root {
            break;
        }

        let cs = next_cs_id.load(ctx, repo.repo_blobstore()).await?;

        assert_eq!(Globalrev::new(next_globalrev), Globalrev::from_bcs(&cs)?);

        next_cs_id = cs
            .parents()
            .next()
            .ok_or_else(|| Error::msg("no parent found"))?;
        next_globalrev -= 1;
    }

    Ok(())
}
