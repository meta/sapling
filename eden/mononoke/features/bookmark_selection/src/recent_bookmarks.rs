/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;

use anyhow::Result;
use bookmarks::BookmarkCategory;
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::Freshness;
use context::CoreContext;
use mononoke_types::ChangesetId;

/// Distinct live publishing bookmark names that moved most recently, newest first.
///
/// Reads current bookmarks in one query, ordered by their latest log id.
/// Fetches up to `want` times the category count so same-name keys cannot
/// crowd out distinct names. The newest live key wins for each name.
/// Includes all categories; excludes scratch bookmarks and rows without log ids.
pub async fn recent_bookmarks(
    ctx: &CoreContext,
    repo: &impl BookmarkUpdateLogRef,
    want: usize,
    freshness: Freshness,
) -> Result<Vec<(String, ChangesetId)>> {
    if want == 0 {
        return Ok(Vec::new());
    }
    let limit = u64::try_from(want)?.saturating_mul(BookmarkCategory::ALL.len().try_into()?);
    let mut seen = HashSet::new();
    Ok(repo
        .bookmark_update_log()
        .list_recently_moved_bookmarks(ctx.clone(), limit, freshness)
        .await?
        .into_iter()
        .map(|(key, cs_id)| (key.to_string(), cs_id))
        .filter(|(name, _)| seen.insert(name.clone()))
        .take(want)
        .collect())
}

#[cfg(test)]
mod test {
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::BookmarkKey;
    use bookmarks::BookmarkUpdateLog;
    use bookmarks::BookmarkUpdateReason;
    use bookmarks::Bookmarks;
    use bookmarks::BookmarksRef;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use context::PerfCounterType;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use mononoke_macros::mononoke;
    use repo_blobstore::RepoBlobstore;
    use repo_derived_data::RepoDerivedData;
    use repo_identity::RepoIdentity;
    use tests_utils::BasicTestRepo;
    use tests_utils::CreateCommitContext;
    use tests_utils::bookmark;

    use super::*;

    #[facet::container]
    #[derive(Clone)]
    struct TestRepo {
        #[delegate(
            dyn BonsaiHgMapping,
            dyn Bookmarks,
            CommitGraph,
            dyn CommitGraphWriter,
            FilestoreConfig,
            RepoBlobstore,
            RepoDerivedData,
            RepoIdentity,
        )]
        repo: BasicTestRepo,

        #[facet]
        bookmark_update_log: dyn BookmarkUpdateLog,
    }

    async fn setup(fb: FacebookInit) -> Result<(CoreContext, TestRepo, ChangesetId, ChangesetId)> {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let a = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a", "a")
            .commit()
            .await?;
        let b = CreateCommitContext::new(&ctx, &repo, vec![a])
            .add_file("b", "b")
            .commit()
            .await?;
        Ok((ctx, repo, a, b))
    }

    fn names(found: &[(String, ChangesetId)]) -> Vec<&str> {
        found.iter().map(|(name, _)| name.as_str()).collect()
    }

    #[mononoke::fbinit_test]
    async fn test_latest_move_wins_newest_first(fb: FacebookInit) -> Result<()> {
        let (ctx, repo, a, b) = setup(fb).await?;
        bookmark(&ctx, &repo, "x").set_to(a).await?;
        bookmark(&ctx, &repo, "y").set_to(a).await?;
        bookmark(&ctx, &repo, "x").set_to(b).await?;

        let found = recent_bookmarks(&ctx, &repo, 10, Freshness::MostRecent).await?;
        assert_eq!(found, vec![("x".to_string(), b), ("y".to_string(), a)]);
        let found = recent_bookmarks(&ctx, &repo, 1, Freshness::MostRecent).await?;
        assert_eq!(found, vec![("x".to_string(), b)]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_deleted_bookmark_is_dropped_before_limit(fb: FacebookInit) -> Result<()> {
        let (ctx, repo, a, _) = setup(fb).await?;
        bookmark(&ctx, &repo, "x").set_to(a).await?;
        bookmark(&ctx, &repo, "y").set_to(a).await?;
        bookmark(&ctx, &repo, "x").delete().await?;

        let found = recent_bookmarks(&ctx, &repo, 1, Freshness::MostRecent).await?;
        assert_eq!(names(&found), vec!["y"]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_quiet_repo_uses_one_query(fb: FacebookInit) -> Result<()> {
        let (ctx, repo, a, b) = setup(fb).await?;
        bookmark(&ctx, &repo, "y").set_to(a).await?;
        for cs in [a, b, a, b, a] {
            bookmark(&ctx, &repo, "x").set_to(cs).await?;
        }

        let query_ctx = CoreContext::test_mock(fb);
        let found = recent_bookmarks(&query_ctx, &repo, 10, Freshness::MaybeStale).await?;
        assert_eq!(names(&found), vec!["x", "y"]);
        assert_eq!(
            query_ctx
                .perf_counters()
                .get_counter(PerfCounterType::SqlReadsReplica),
            1,
        );
        assert_eq!(
            query_ctx
                .perf_counters()
                .get_counter(PerfCounterType::SqlReadsMaster),
            0,
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_category_duplicates_do_not_consume_name_limit(fb: FacebookInit) -> Result<()> {
        let (ctx, repo, a, b) = setup(fb).await?;
        bookmark(&ctx, &repo, "old").set_to(a).await?;
        let branch = BookmarkKey::new("busy")?;
        for category in BookmarkCategory::ALL {
            let key = BookmarkKey::with_name_and_category(branch.name().clone(), *category);
            let mut txn = repo.bookmarks().create_transaction(ctx.clone());
            txn.force_set(&key, b, BookmarkUpdateReason::TestMove)?;
            assert!(txn.commit().await?.is_some());
        }
        let scratch = BookmarkKey::new("scratch")?;
        let mut txn = repo.bookmarks().create_transaction(ctx.clone());
        txn.create_scratch(&scratch, b)?;
        assert!(txn.commit().await?.is_some());

        let query_ctx = CoreContext::test_mock(fb);
        assert_eq!(
            recent_bookmarks(&query_ctx, &repo, 2, Freshness::MaybeStale).await?,
            vec![("busy".to_string(), b), ("old".to_string(), a)]
        );
        assert_eq!(
            query_ctx
                .perf_counters()
                .get_counter(PerfCounterType::SqlReadsReplica),
            1
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_repo_without_moves_is_empty(fb: FacebookInit) -> Result<()> {
        let (ctx, repo, _, _) = setup(fb).await?;
        let found = recent_bookmarks(&ctx, &repo, 10, Freshness::MostRecent).await?;
        assert!(found.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_zero_limit_does_not_query(fb: FacebookInit) -> Result<()> {
        let (_, repo, _, _) = setup(fb).await?;
        let query_ctx = CoreContext::test_mock(fb);
        assert!(
            recent_bookmarks(&query_ctx, &repo, 0, Freshness::MostRecent)
                .await?
                .is_empty()
        );
        assert_eq!(
            query_ctx
                .perf_counters()
                .get_counter(PerfCounterType::SqlReadsMaster),
            0
        );
        assert_eq!(
            query_ctx
                .perf_counters()
                .get_counter(PerfCounterType::SqlReadsReplica),
            0
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_most_recent_reads_only_the_primary(fb: FacebookInit) -> Result<()> {
        let (ctx, repo, a, _) = setup(fb).await?;
        bookmark(&ctx, &repo, "x").set_to(a).await?;
        let query_ctx = CoreContext::test_mock(fb);
        let found = recent_bookmarks(&query_ctx, &repo, 10, Freshness::MostRecent).await?;
        assert_eq!(names(&found), vec!["x"]);
        assert_eq!(
            query_ctx
                .perf_counters()
                .get_counter(PerfCounterType::SqlReadsMaster),
            1
        );
        assert_eq!(
            query_ctx
                .perf_counters()
                .get_counter(PerfCounterType::SqlReadsReplica),
            0
        );
        Ok(())
    }
}
