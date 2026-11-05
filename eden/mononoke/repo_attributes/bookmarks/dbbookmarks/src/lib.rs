/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

pub mod builder;
pub mod store;
mod subscription;
pub mod transaction;

pub use crate::builder::SqlBookmarksBuilder;
pub use crate::store::ArcSqlBookmarks;
pub use crate::store::SqlBookmarks;
pub use crate::store::SqlBookmarksRef;

#[cfg(test)]
mod test {
    use std::cmp::Reverse;
    use std::collections::BTreeMap;
    use std::collections::HashSet;

    use anyhow::Result;
    use ascii::AsciiString;
    use bookmarks::Bookmark;
    use bookmarks::BookmarkCategory;
    use bookmarks::BookmarkKey;
    use bookmarks::BookmarkKind;
    use bookmarks::BookmarkPagination;
    use bookmarks::BookmarkPrefix;
    use bookmarks::BookmarkUpdateLog;
    use bookmarks::BookmarkUpdateReason;
    use bookmarks::Bookmarks;
    use bookmarks::Freshness;
    use context::CoreContext;
    use context::PerfCounterType;
    use fbinit::FacebookInit;
    use futures::stream::TryStreamExt;
    use mononoke_macros::mononoke;
    use mononoke_types::ChangesetId;
    use mononoke_types::RepositoryId;
    use mononoke_types_mocks::changesetid::ONES_CSID;
    use mononoke_types_mocks::changesetid::TWOS_CSID;
    use mononoke_types_mocks::repo::REPO_ZERO;
    use sql_construct::SqlConstruct;

    use super::*;

    #[mononoke::fbinit_test]
    async fn test_recently_moved_filters_before_limit(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlBookmarksBuilder::with_sqlite_in_memory()?;
        let store = builder.clone().with_repo_id(REPO_ZERO);
        let other_repo = RepositoryId::new(1);
        let branch = BookmarkKey::new("same")?;
        let tag = BookmarkKey::with_name_and_category(branch.name().clone(), BookmarkCategory::Tag);
        let note =
            BookmarkKey::with_name_and_category(branch.name().clone(), BookmarkCategory::Note);
        let scratch = BookmarkKey::new("scratch")?;
        let no_log = BookmarkKey::new("no_log")?;
        let other = BookmarkKey::new("other")?;
        let rows = [
            (
                &REPO_ZERO,
                &Some(1),
                &branch,
                &ONES_CSID,
                &BookmarkKind::PullDefaultPublishing,
            ),
            (
                &REPO_ZERO,
                &Some(2),
                &tag,
                &TWOS_CSID,
                &BookmarkKind::Publishing,
            ),
            (
                &REPO_ZERO,
                &Some(3),
                &note,
                &ONES_CSID,
                &BookmarkKind::Publishing,
            ),
            // Even a scratch row with a log id must not consume the limit.
            (
                &REPO_ZERO,
                &Some(100),
                &scratch,
                &ONES_CSID,
                &BookmarkKind::Scratch,
            ),
            (
                &REPO_ZERO,
                &None,
                &no_log,
                &ONES_CSID,
                &BookmarkKind::Publishing,
            ),
            (
                &other_repo,
                &Some(200),
                &other,
                &ONES_CSID,
                &BookmarkKind::Publishing,
            ),
        ];
        let values: Vec<_> = rows
            .iter()
            .map(|(repo, log_id, key, cs, kind)| {
                (*repo, *log_id, key.name(), key.category(), *cs, *kind)
            })
            .collect();
        crate::transaction::InsertBookmarks::query(
            store.write_connection(),
            ctx.sql_query_telemetry(),
            &values,
        )
        .await?;

        assert_eq!(
            store
                .list_recently_moved_bookmarks(ctx.clone(), 1, Freshness::MostRecent)
                .await?,
            vec![(note.clone(), ONES_CSID)]
        );
        assert_eq!(
            store
                .list_recently_moved_bookmarks(ctx.clone(), 10, Freshness::MostRecent)
                .await?,
            vec![(note, ONES_CSID), (tag, TWOS_CSID), (branch, ONES_CSID)]
        );
        assert!(
            store
                .list_recently_moved_bookmarks(ctx.clone(), 0, Freshness::MostRecent)
                .await?
                .is_empty()
        );
        assert_eq!(
            builder
                .with_repo_id(other_repo)
                .list_recently_moved_bookmarks(ctx, 10, Freshness::MostRecent)
                .await?,
            vec![(other, ONES_CSID)]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_recently_moved_matches_live_log_entries(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let store = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
        let x = BookmarkKey::new("x")?;
        let y = BookmarkKey::new("y")?;
        let deleted = BookmarkKey::new("deleted")?;
        for (key, cs) in [
            (&y, ONES_CSID),
            (&x, ONES_CSID),
            (&deleted, ONES_CSID),
            (&x, TWOS_CSID),
            (&x, ONES_CSID),
            (&x, TWOS_CSID),
        ] {
            let mut txn = store.create_transaction(ctx.clone());
            txn.force_set(key, cs, BookmarkUpdateReason::TestMove)?;
            assert!(txn.commit().await?.is_some());
        }
        let mut txn = store.create_transaction(ctx.clone());
        txn.force_delete(&deleted, BookmarkUpdateReason::TestMove)?;
        assert!(txn.commit().await?.is_some());

        let log = store
            .read_next_bookmark_log_entries(ctx.clone(), 0.into(), 100, Freshness::MostRecent)
            .try_collect::<Vec<_>>()
            .await?;
        let latest: BTreeMap<_, _> = log
            .into_iter()
            .map(|entry| (entry.bookmark_name, (entry.id, entry.to_changeset_id)))
            .collect();
        let mut live: Vec<_> = latest
            .into_iter()
            .filter_map(|(key, (id, cs))| cs.map(|cs| (id, key, cs)))
            .collect();
        live.sort_unstable_by_key(|entry| Reverse(entry.0));
        let expected: Vec<_> = live.into_iter().map(|(_, key, cs)| (key, cs)).collect();
        assert_eq!(expected, vec![(x, TWOS_CSID), (y, ONES_CSID)]);
        for limit in [0, 1, 2, 10] {
            assert_eq!(
                store
                    .list_recently_moved_bookmarks(ctx.clone(), limit, Freshness::MostRecent)
                    .await?,
                expected
                    .iter()
                    .take(limit as usize)
                    .cloned()
                    .collect::<Vec<_>>()
            );
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_recently_moved_freshness_uses_distinct_databases(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let primary = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
        let replica = SqlBookmarksBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
        let key = BookmarkKey::new("x")?;
        for (store, cs) in [(&primary, ONES_CSID), (&replica, TWOS_CSID)] {
            let mut txn = store.create_transaction(ctx.clone());
            txn.force_set(&key, cs, BookmarkUpdateReason::TestMove)?;
            assert!(txn.commit().await?.is_some());
        }
        let mut connections = primary.connections.clone();
        connections.read_connection = replica.connections.read_connection.clone();
        let store = SqlBookmarksBuilder::from_sql_connections(connections).with_repo_id(REPO_ZERO);

        for (freshness, cs, master_reads, replica_reads) in [
            (Freshness::MaybeStale, TWOS_CSID, 0, 1),
            (Freshness::MostRecent, ONES_CSID, 1, 0),
        ] {
            let query_ctx = CoreContext::test_mock(fb);
            assert_eq!(
                store
                    .list_recently_moved_bookmarks(query_ctx.clone(), 10, freshness)
                    .await?,
                vec![(key.clone(), cs)]
            );
            assert_eq!(
                query_ctx
                    .perf_counters()
                    .get_counter(PerfCounterType::SqlReadsMaster),
                master_reads
            );
            assert_eq!(
                query_ctx
                    .perf_counters()
                    .get_counter(PerfCounterType::SqlReadsReplica),
                replica_reads
            );
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_update_kind_compatibility(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let store = SqlBookmarksBuilder::with_sqlite_in_memory()
            .unwrap()
            .with_repo_id(REPO_ZERO);
        let scratch_name = BookmarkKey::new("book1").unwrap();
        let publishing_name = BookmarkKey::new("book2").unwrap();
        let pull_default_name = BookmarkKey::new("book3").unwrap();

        let conn = store.connections.write_connection.clone();

        let rows = vec![
            (
                &REPO_ZERO,
                &scratch_name,
                &ONES_CSID,
                &BookmarkKind::Scratch,
            ),
            (
                &REPO_ZERO,
                &publishing_name,
                &ONES_CSID,
                &BookmarkKind::Publishing,
            ),
            (
                &REPO_ZERO,
                &pull_default_name,
                &ONES_CSID,
                &BookmarkKind::PullDefaultPublishing,
            ),
        ];

        crate::transaction::insert_bookmarks(&ctx, &conn, rows).await?;

        // Using 'create_scratch' to replace a non-scratch bookmark should fail.
        let mut txn = store.create_transaction(ctx.clone());
        txn.create_scratch(&publishing_name, ONES_CSID)?;
        assert!(txn.commit().await?.is_none());

        // Using 'create' to replace a scratch bookmark should fail.
        let mut txn = store.create_transaction(ctx.clone());
        txn.create(&scratch_name, ONES_CSID, BookmarkUpdateReason::TestMove)?;
        assert!(txn.commit().await?.is_none());

        // Using 'update_scratch' to update a publishing bookmark should fail.
        let mut txn = store.create_transaction(ctx.clone());
        txn.update_scratch(&publishing_name, TWOS_CSID, ONES_CSID)?;
        assert!(txn.commit().await?.is_none());

        // Using 'update_scratch' to update a pull-default bookmark should fail.
        let mut txn = store.create_transaction(ctx.clone());
        txn.update_scratch(&pull_default_name, TWOS_CSID, ONES_CSID)?;
        assert!(txn.commit().await?.is_none());

        // Using 'update' to update a publishing bookmark should succeed.
        let mut txn = store.create_transaction(ctx.clone());
        txn.update(
            &publishing_name,
            TWOS_CSID,
            ONES_CSID,
            BookmarkUpdateReason::TestMove,
        )?;
        assert!(txn.commit().await?.is_some());

        // Using 'update' to update a pull-default bookmark should succeed.
        let mut txn = store.create_transaction(ctx.clone());
        txn.update(
            &pull_default_name,
            TWOS_CSID,
            ONES_CSID,
            BookmarkUpdateReason::TestMove,
        )?;
        assert!(txn.commit().await?.is_some());

        // Using 'update' to update a scratch bookmark should fail.
        let mut txn = store.create_transaction(ctx.clone());
        txn.update(
            &scratch_name,
            TWOS_CSID,
            ONES_CSID,
            BookmarkUpdateReason::TestMove,
        )?;
        assert!(txn.commit().await?.is_none());

        // Using 'update_scratch' to update a scratch bookmark should succeed.
        let mut txn = store.create_transaction(ctx.clone());
        txn.update_scratch(&scratch_name, TWOS_CSID, ONES_CSID)?;
        assert!(txn.commit().await?.is_some());

        Ok(())
    }

    fn mock_bookmarks_response(
        bookmarks: &BTreeMap<BookmarkKey, (BookmarkKind, ChangesetId)>,
        prefix: &BookmarkPrefix,
        categories: &[BookmarkCategory],
        kinds: &[BookmarkKind],
        pagination: &BookmarkPagination,
        limit: u64,
    ) -> Vec<(Bookmark, ChangesetId)> {
        let range = prefix.to_range().with_pagination(pagination.clone());
        bookmarks
            .range(range)
            .filter_map(|(key, (kind, csid))| {
                let category = key.category();
                if kinds.iter().any(|k| kind == k) && categories.iter().any(|c| category == c) {
                    let bookmark = Bookmark {
                        key: key.clone(),
                        kind: *kind,
                    };
                    Some((bookmark, *csid))
                } else {
                    None
                }
            })
            .take(limit as usize)
            .collect()
    }

    async fn insert_then_query(
        fb: FacebookInit,
        bookmarks: &BTreeMap<BookmarkKey, (BookmarkKind, ChangesetId)>,
        query_freshness: Freshness,
        query_prefix: &BookmarkPrefix,
        query_categories: &[BookmarkCategory],
        query_kinds: &[BookmarkKind],
        query_pagination: &BookmarkPagination,
        query_limit: u64,
    ) -> Vec<(Bookmark, ChangesetId)> {
        let ctx = CoreContext::test_mock(fb);
        let repo_id = RepositoryId::new(123);

        let store = SqlBookmarksBuilder::with_sqlite_in_memory()
            .unwrap()
            .with_repo_id(repo_id);

        let rows = bookmarks
            .iter()
            .map(|(bookmark, (kind, changeset_id))| (&repo_id, bookmark, changeset_id, kind));

        crate::transaction::insert_bookmarks(&ctx, &store.connections.write_connection, rows)
            .await
            .expect("insert failed");

        let response = store
            .list(
                ctx,
                query_freshness,
                query_prefix,
                query_categories,
                query_kinds,
                query_pagination,
                query_limit,
            )
            .try_collect::<Vec<_>>();

        response.await.expect("query failed")
    }

    #[mononoke::quickcheck_test]
    async fn responses_match(
        fb: FacebookInit,
        bookmarks: BTreeMap<BookmarkKey, (BookmarkKind, ChangesetId)>,
        freshness: Freshness,
        categories: HashSet<BookmarkCategory>,
        kinds: HashSet<BookmarkKind>,
        prefix_char: Option<ascii_ext::AsciiChar>,
        after: Option<BookmarkKey>,
        limit: u64,
    ) -> bool {
        // Test that requests return what is expected.
        let categories: Vec<_> = categories.into_iter().collect();
        let kinds: Vec<_> = kinds.into_iter().collect();
        let prefix = match prefix_char {
            Some(ch) => BookmarkPrefix::new_ascii(AsciiString::from(&[ch.0][..])),
            None => BookmarkPrefix::empty(),
        };
        let pagination = match after {
            Some(key) => BookmarkPagination::After(key.into_name()),
            None => BookmarkPagination::FromStart,
        };
        let mut have = insert_then_query(
            fb,
            &bookmarks,
            freshness,
            &prefix,
            categories.as_slice(),
            kinds.as_slice(),
            &pagination,
            limit,
        )
        .await;
        let mut want = mock_bookmarks_response(
            &bookmarks,
            &prefix,
            categories.as_slice(),
            kinds.as_slice(),
            &pagination,
            limit,
        );
        have.sort_by_key(|(_, csid)| *csid);
        want.sort_by_key(|(_, csid)| *csid);
        have == want
    }
}
