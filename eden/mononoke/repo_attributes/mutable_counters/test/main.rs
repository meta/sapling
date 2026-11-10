/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use context::CoreContext;
use fbinit::FacebookInit;
use mononoke_macros::mononoke;
use mononoke_types_mocks::repo::REPO_ZERO;
use mutable_counters::MAX_COUNTER_NAME_LENGTH;
use mutable_counters::MutableCounters;
use mutable_counters::SqlMutableCounters;
use mutable_counters::SqlMutableCountersBuilder;
use sql_construct::SqlConstruct;
use sql_ext::Connection;
use sql_ext::SqlConnections;
use sql_ext::open_sqlite_in_memory;

fn create_db() -> Result<SqlMutableCounters> {
    Ok(SqlMutableCountersBuilder::with_sqlite_in_memory()?.build(REPO_ZERO))
}

/// Create a database where any write to the counter named `poisoned` fails
/// inside the transaction, after earlier statements in it have already run.
fn create_db_failing_writes_to(poisoned: &str) -> Result<SqlMutableCounters> {
    let conn = open_sqlite_in_memory()?;
    conn.execute_batch(SqlMutableCountersBuilder::CREATION_QUERY)?;
    conn.execute_batch(&format!(
        "CREATE TRIGGER fail_poisoned_counter BEFORE INSERT ON mutable_counters
         WHEN NEW.name = '{poisoned}'
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END;"
    ))?;
    let connections = SqlConnections::new_single(Connection::with_sqlite(conn)?);
    Ok(SqlMutableCountersBuilder::from_sql_connections(connections).build(REPO_ZERO))
}

#[mononoke::fbinit_test]
async fn test_counter_simple(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mutable_counters = create_db()?;

    mutable_counters
        .set_counter(&ctx, "counter", 1, None)
        .await?;
    assert_eq!(
        mutable_counters.get_counter(&ctx, "counter").await?,
        Some(1)
    );

    mutable_counters
        .set_counter(&ctx, "counter", 2, None)
        .await?;
    assert_eq!(
        mutable_counters.get_counter(&ctx, "counter").await?,
        Some(2)
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_counter_conditional_update(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mutable_counters = create_db()?;

    mutable_counters
        .set_counter(&ctx, "counter", 1, None)
        .await?;
    assert_eq!(
        mutable_counters.get_counter(&ctx, "counter").await?,
        Some(1)
    );

    assert!(
        mutable_counters
            .set_counter(&ctx, "counter", 2, Some(1))
            .await?
    );
    assert_eq!(
        mutable_counters.get_counter(&ctx, "counter").await?,
        Some(2)
    );

    // Not updated because prev_value is incorrect
    assert!(
        !mutable_counters
            .set_counter(&ctx, "counter", 3, Some(1))
            .await?
    );
    assert_eq!(
        mutable_counters.get_counter(&ctx, "counter").await?,
        Some(2)
    );

    // Attempt to update another counter, make sure it doesn't touch first counter
    // and update doesn't succeed as previous value is not set.
    assert!(
        !mutable_counters
            .set_counter(&ctx, "counter2", 3, Some(2))
            .await?,
    );
    assert_eq!(
        mutable_counters.get_counter(&ctx, "counter").await?,
        Some(2)
    );
    assert_eq!(mutable_counters.get_counter(&ctx, "counter2").await?, None);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_counter_initialize_if_absent(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mutable_counters = create_db()?;

    assert!(
        mutable_counters
            .set_counter_if_absent(&ctx, "counter", 1)
            .await?
    );
    assert!(
        !mutable_counters
            .set_counter_if_absent(&ctx, "counter", 2)
            .await?
    );
    assert_eq!(
        mutable_counters.get_counter(&ctx, "counter").await?,
        Some(1)
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_counter_name_length_limit(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mutable_counters = create_db()?;
    let maximum_name = "x".repeat(MAX_COUNTER_NAME_LENGTH);
    let oversized_name = "x".repeat(MAX_COUNTER_NAME_LENGTH + 1);

    assert!(
        mutable_counters
            .set_counter(&ctx, &maximum_name, 1, None)
            .await?
    );
    assert!(
        mutable_counters
            .set_counter_if_absent(&ctx, &oversized_name, 1)
            .await
            .is_err()
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_set_counters_writes_all(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mutable_counters = create_db()?;
    mutable_counters.set_counter(&ctx, "a", 1, None).await?;

    mutable_counters
        .set_counters(&ctx, &[("a", 10), ("b", 20)])
        .await?;

    assert_eq!(
        mutable_counters.get_counter(&ctx, "a").await?,
        Some(10),
        "existing counter should be overwritten"
    );
    assert_eq!(
        mutable_counters.get_counter(&ctx, "b").await?,
        Some(20),
        "missing counter should be created"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_set_counters_failure_writes_nothing(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mutable_counters = create_db_failing_writes_to("b")?;
    mutable_counters.set_counter(&ctx, "a", 1, None).await?;

    // The write to "a" runs first and succeeds inside the transaction; the
    // write to "b" then fails, so the whole transaction must roll back.
    assert!(
        mutable_counters
            .set_counters(&ctx, &[("a", 10), ("b", 20)])
            .await
            .is_err()
    );
    assert_eq!(
        mutable_counters.get_counter(&ctx, "a").await?,
        Some(1),
        "first counter must be rolled back when the second write fails"
    );
    assert_eq!(mutable_counters.get_counter(&ctx, "b").await?, None);

    // The connection is usable again after the rollback.
    mutable_counters.set_counters(&ctx, &[("a", 11)]).await?;
    assert_eq!(mutable_counters.get_counter(&ctx, "a").await?, Some(11));
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_set_counters_invalid_name_writes_nothing(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mutable_counters = create_db()?;
    mutable_counters.set_counter(&ctx, "a", 1, None).await?;
    let oversized_name = "x".repeat(MAX_COUNTER_NAME_LENGTH + 1);

    assert!(
        mutable_counters
            .set_counters(&ctx, &[("a", 10), (&oversized_name, 20)])
            .await
            .is_err()
    );
    assert_eq!(
        mutable_counters.get_counter(&ctx, "a").await?,
        Some(1),
        "no counter should be written when any name is invalid"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_set_counters_empty_is_noop(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mutable_counters = create_db()?;
    mutable_counters.set_counter(&ctx, "a", 1, None).await?;

    mutable_counters.set_counters(&ctx, &[]).await?;

    assert_eq!(
        mutable_counters.get_all_counters(&ctx).await?,
        vec![("a".to_owned(), 1)]
    );
    Ok(())
}
