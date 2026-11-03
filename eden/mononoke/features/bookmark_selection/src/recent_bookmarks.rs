/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use anyhow::Result;
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::Freshness;
use context::CoreContext;
use futures::TryStreamExt;
use mononoke_types::ChangesetId;

/// The distinct bookmarks that moved most recently, newest first.
///
/// `BookmarkUpdateLog` only reads forward from an id, so this starts a window back
/// from the newest entry and widens by doubling until it has enough distinct
/// names. Widening is the common case for a quiet repo, where the newest entries
/// are all one bookmark; `max_scan` stops a repo whose refs all arrived in a
/// single gitimport batch from walking its whole history to find a tenth name
/// that does not exist.
///
/// A bookmark whose most recent entry has no `to_changeset_id` was deleted, so it
/// is dropped rather than resolved.
pub async fn recent_bookmarks(
    ctx: &CoreContext,
    repo: &impl BookmarkUpdateLogRef,
    want: usize,
    mut window: u64,
    max_scan: u64,
    freshness: Freshness,
) -> Result<Vec<(String, ChangesetId)>> {
    let log = repo.bookmark_update_log();
    let Some(newest) = log.get_largest_log_id(ctx.clone(), freshness).await? else {
        return Ok(Vec::new());
    };

    loop {
        let from = newest.saturating_sub(window);
        // Latest entry wins per name, so the map collapses a busy bookmark's many
        // moves into the single position that matters.
        let mut latest: HashMap<String, (u64, Option<ChangesetId>)> = HashMap::new();
        let mut entries = log.read_next_bookmark_log_entries(
            ctx.clone(),
            from.into(),
            window.saturating_mul(2),
            freshness,
        );
        while let Some(entry) = entries.try_next().await? {
            let id: u64 = entry.id.into();
            let slot = latest
                .entry(entry.bookmark_name.to_string())
                .or_insert((0, None));
            if id >= slot.0 {
                *slot = (id, entry.to_changeset_id);
            }
        }

        let mut live: Vec<(u64, String, ChangesetId)> = latest
            .into_iter()
            .filter_map(|(name, (id, csid))| csid.map(|csid| (id, name, csid)))
            .collect();
        live.sort_unstable_by_key(|(id, _, _)| std::cmp::Reverse(*id));

        if live.len() >= want || window >= max_scan || from == 0 {
            return Ok(live
                .into_iter()
                .take(want)
                .map(|(_, name, csid)| (name, csid))
                .collect());
        }
        window = window.saturating_mul(2);
    }
}

#[cfg(test)]
mod test {
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::BookmarkUpdateLog;
    use bookmarks::Bookmarks;
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

        let found = recent_bookmarks(&ctx, &repo, 10, 4096, 262144, Freshness::MostRecent).await?;
        assert_eq!(found, vec![("x".to_string(), b), ("y".to_string(), a)]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_deleted_bookmark_is_dropped(fb: FacebookInit) -> Result<()> {
        let (ctx, repo, a, _) = setup(fb).await?;
        bookmark(&ctx, &repo, "x").set_to(a).await?;
        bookmark(&ctx, &repo, "y").set_to(a).await?;
        bookmark(&ctx, &repo, "x").delete().await?;

        let found = recent_bookmarks(&ctx, &repo, 10, 4096, 262144, Freshness::MostRecent).await?;
        assert_eq!(names(&found), vec!["y"]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_quiet_repo_widens_window(fb: FacebookInit) -> Result<()> {
        let (ctx, repo, a, b) = setup(fb).await?;
        bookmark(&ctx, &repo, "y").set_to(a).await?;
        for cs in [a, b, a, b, a] {
            bookmark(&ctx, &repo, "x").set_to(cs).await?;
        }

        let found = recent_bookmarks(&ctx, &repo, 2, 1, 262144, Freshness::MostRecent).await?;
        assert_eq!(names(&found), vec!["x", "y"]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_max_scan_stops_widening(fb: FacebookInit) -> Result<()> {
        let (ctx, repo, a, b) = setup(fb).await?;
        bookmark(&ctx, &repo, "y").set_to(a).await?;
        for cs in [a, b, a, b, a] {
            bookmark(&ctx, &repo, "x").set_to(cs).await?;
        }

        let found = recent_bookmarks(&ctx, &repo, 2, 1, 2, Freshness::MostRecent).await?;
        assert_eq!(names(&found), vec!["x"]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_repo_without_moves_is_empty(fb: FacebookInit) -> Result<()> {
        let (ctx, repo, _, _) = setup(fb).await?;
        let found = recent_bookmarks(&ctx, &repo, 10, 4096, 262144, Freshness::MostRecent).await?;
        assert!(found.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_maybe_stale_reads_only_the_replica(fb: FacebookInit) -> Result<()> {
        let (ctx, repo, a, b) = setup(fb).await?;
        bookmark(&ctx, &repo, "y").set_to(a).await?;
        for cs in [a, b, a, b, a] {
            bookmark(&ctx, &repo, "x").set_to(cs).await?;
        }

        // Fresh contexts so the counters only see the scan, not the setup writes.
        let stale_ctx = CoreContext::test_mock(fb);
        let found =
            recent_bookmarks(&stale_ctx, &repo, 10, 1, 262144, Freshness::MaybeStale).await?;
        assert_eq!(names(&found), vec!["x", "y"]);
        let stale_counters = stale_ctx.perf_counters();
        assert_eq!(
            stale_counters.get_counter(PerfCounterType::SqlReadsMaster),
            0,
            "a MaybeStale scan should not read from the primary"
        );
        assert!(
            stale_counters.get_counter(PerfCounterType::SqlReadsReplica) > 0,
            "a MaybeStale scan should read from the replica"
        );

        let fresh_ctx = CoreContext::test_mock(fb);
        recent_bookmarks(&fresh_ctx, &repo, 10, 1, 262144, Freshness::MostRecent).await?;
        assert!(
            fresh_ctx
                .perf_counters()
                .get_counter(PerfCounterType::SqlReadsMaster)
                > 0,
            "a MostRecent scan should read from the primary, so the counters are being recorded"
        );
        Ok(())
    }
}
