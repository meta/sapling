/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use context::CoreContext;
use fbinit::FacebookInit;
use mercurial_types_mocks::nodehash::make_hg_cs_id;
use mononoke_macros::mononoke;
use mononoke_types_mocks::repo::REPO_ZERO;
use sql_construct::SqlConstruct;

use super::SelectBySuccessorChainFollowingSplits;
use super::SqlHgMutationStore;
use crate::HgMutationEntry;
use crate::HgMutationStore;
use crate::SqlHgMutationStoreBuilder;

async fn check_split_fold_query_rows(ctx: &CoreContext, store: &SqlHgMutationStore) -> Result<()> {
    let entries = (0..6)
        .flat_map(|round| {
            let predecessor = make_hg_cs_id(round * 4);
            let first = make_hg_cs_id(round * 4 + 1);
            let second = make_hg_cs_id(round * 4 + 2);
            let head = make_hg_cs_id(round * 4 + 3);
            let folded = make_hg_cs_id(round * 4 + 4);
            [
                HgMutationEntry::new(
                    head,
                    vec![predecessor],
                    vec![first, second],
                    "split".to_owned(),
                    "test".to_owned(),
                    0,
                    0,
                    vec![],
                ),
                HgMutationEntry::new(
                    folded,
                    vec![first, second, head],
                    vec![],
                    "fold".to_owned(),
                    "test".to_owned(),
                    0,
                    0,
                    vec![],
                ),
            ]
        })
        .collect();
    let predecessor = make_hg_cs_id(24);
    store
        .add_entries(ctx, (0..=24).map(make_hg_cs_id).collect(), entries)
        .await?;
    let rows = SelectBySuccessorChainFollowingSplits::query(
        &store.connections.read_connection,
        ctx.sql_query_telemetry(),
        &store.repo_id,
        &store.mutation_chain_limit,
        &predecessor,
    )
    .await?;
    assert_eq!(
        rows.len(),
        24,
        "six splits plus six three-input folds have 24 predecessor rows"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn split_query_error_preserves_requested_commit_context(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let changeset_id = make_hg_cs_id(3);
    let complete = SqlHgMutationStoreBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
    assert!(
        complete
            .all_predecessors_following_splits(&ctx, changeset_id)
            .await?
            .is_empty()
    );

    let sqlite = sql::rusqlite::Connection::open_in_memory()?;
    // Replica selection must succeed before the missing mutation table fails the query.
    sqlite.execute_batch(
        "CREATE TABLE hg_mutation_changesets (repo_id INTEGER, changeset_id BLOB)",
    )?;
    let connection = sql_ext::Connection {
        inner: sql::Connection::with_sqlite(sqlite),
        shard_name: "private-sqlite-error".to_owned(),
    };
    let store = SqlHgMutationStore::new(
        REPO_ZERO,
        sql_ext::SqlConnections {
            read_connection: connection.clone(),
            read_master_connection: connection.clone(),
            write_connection: connection,
        },
        20,
    );
    let error = store
        .all_predecessors_following_splits(&ctx, changeset_id)
        .await
        .expect_err("missing mutation schema cannot return a successful empty history");
    assert_eq!(
        error.to_string(),
        format!("Error fetching split mutation chain for: {changeset_id}")
    );
    let error_chain = format!("{error:#}");
    assert!(
        error_chain.contains("SelectBySuccessorChainFollowingSplitsImpl")
            && error_chain.contains("no such table: hg_mutation_"),
        "the split query's missing-schema error must remain in the chain: {error_chain}"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn split_fold_query_deduplicates_before_returning_rows(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let store = SqlHgMutationStoreBuilder::with_sqlite_in_memory()?.with_repo_id(REPO_ZERO);
    check_split_fold_query_rows(&ctx, &store).await
}
