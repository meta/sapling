/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;

use anyhow::Error;
use context::CoreContext;
use fbinit::FacebookInit;
use mononoke_macros::mononoke;
use mononoke_types::Timestamp;
use mononoke_types_mocks::changesetid as bonsai;
use mononoke_types_mocks::repo::REPO_ZERO;
use repo_metadata_checkpoint::RepoMetadataCheckpoint;
use repo_metadata_checkpoint::RepoMetadataCheckpointEntry;
use repo_metadata_checkpoint::SqlRepoMetadataCheckpointBuilder;
use repo_metadata_checkpoint::should_skip_full_run;
use sql_construct::SqlConstruct;
use sql_ext::Connection;
use sql_ext::SqlConnections;
use sql_ext::open_sqlite_in_memory;

#[mononoke::fbinit_test]
async fn test_add_and_get(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let metadata_info = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());
    let bookmark_name = "JustABookmark";
    let timestamp = Timestamp::now();
    let entry = RepoMetadataCheckpointEntry {
        changeset_id: bonsai::ONES_CSID,
        bookmark_name: bookmark_name.to_string(),
        last_updated_timestamp: timestamp,
    };
    metadata_info
        .add_or_update_entries(vec![entry.clone()])
        .await?;
    let result = metadata_info.get_entry(entry.bookmark_name.clone()).await?;

    assert_eq!(
        result.as_ref().map(|entry| (
            entry.bookmark_name.to_string(),
            entry.changeset_id,
            entry.last_updated_timestamp
        )),
        Some((bookmark_name.to_string(), bonsai::ONES_CSID, timestamp))
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_update_and_get(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let metadata_info = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());
    let bookmark_name = "JustABookmark";
    let timestamp = Timestamp::now();
    let entry = RepoMetadataCheckpointEntry {
        changeset_id: bonsai::ONES_CSID,
        bookmark_name: bookmark_name.to_string(),
        last_updated_timestamp: timestamp,
    };

    metadata_info
        .add_or_update_entries(vec![entry.clone()])
        .await?;
    let result = metadata_info.get_entry(entry.bookmark_name.clone()).await?;
    assert_eq!(result, Some(entry.clone()));

    // Update the changeset id and try storing the same entry
    let updated_timestamp = Timestamp::now();
    let new_entry = RepoMetadataCheckpointEntry {
        changeset_id: bonsai::TWOS_CSID,
        bookmark_name: bookmark_name.to_string(),
        last_updated_timestamp: updated_timestamp,
    };

    metadata_info
        .add_or_update_entries(vec![new_entry.clone()])
        .await?;

    let result = metadata_info
        .get_entry(new_entry.bookmark_name.clone())
        .await?;
    assert_eq!(result, Some(new_entry.clone()));

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_get_without_add(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let metadata_info = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());
    let result = metadata_info.get_entry("master".to_string()).await?;
    assert_eq!(result, None);

    let result = metadata_info.get_all_entries().await?;
    assert_eq!(result.len(), 0);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_get_multiple(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let metadata_info = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());
    let entry = RepoMetadataCheckpointEntry {
        changeset_id: bonsai::ONES_CSID,
        bookmark_name: "master".to_string(),
        last_updated_timestamp: Timestamp::now(),
    };
    let another_entry = RepoMetadataCheckpointEntry {
        changeset_id: bonsai::TWOS_CSID,
        bookmark_name: "release".to_string(),
        last_updated_timestamp: Timestamp::now(),
    };

    metadata_info
        .add_or_update_entries(vec![entry.clone(), another_entry.clone()])
        .await?;

    let result = metadata_info
        .get_all_entries()
        .await?
        .into_iter()
        .map(|entry| entry.bookmark_name);
    assert_eq!(
        HashSet::from_iter(result),
        HashSet::from(["master".to_string(), "release".to_string()])
    );
    Ok(())
}

// Tests for full-run tracking (methods on RepoMetadataCheckpoint)

#[mononoke::fbinit_test]
async fn test_full_run_info_get_without_set(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let checkpoint = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());

    // Should return None when no timestamp has been set
    let result = checkpoint.get_last_full_run_timestamp().await?;
    assert_eq!(result, None);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_full_run_info_set_and_get(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let checkpoint = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());

    let timestamp = Timestamp::from_timestamp_secs(1000000);

    // Set the timestamp
    checkpoint.record_full_run(timestamp, None).await?;

    // Should return the timestamp we set
    let result = checkpoint.get_last_full_run_timestamp().await?;
    assert_eq!(result, Some(timestamp));

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_full_run_info_update(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let checkpoint = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());

    let timestamp1 = Timestamp::from_timestamp_secs(1000000);
    let timestamp2 = Timestamp::from_timestamp_secs(2000000);

    // Set initial timestamp
    checkpoint.record_full_run(timestamp1, None).await?;
    assert_eq!(
        checkpoint.get_last_full_run_timestamp().await?,
        Some(timestamp1)
    );

    // Update to new timestamp
    checkpoint.record_full_run(timestamp2, None).await?;
    assert_eq!(
        checkpoint.get_last_full_run_timestamp().await?,
        Some(timestamp2)
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_full_run_info_repo_id(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let checkpoint = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());

    // Verify repo_id is correctly stored
    assert_eq!(checkpoint.repo_id(), REPO_ZERO);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_get_fallback_bookmark_none(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let checkpoint = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());

    assert_eq!(
        checkpoint.get_fallback_bookmark().await?,
        None,
        "No fallback bookmark should be stored before one is set"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_set_fallback_bookmark_without_full_run_is_a_no_op(
    fb: FacebookInit,
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let checkpoint = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());

    checkpoint.set_fallback_bookmark("dev1".to_string()).await?;
    assert_eq!(
        checkpoint.get_fallback_bookmark().await?,
        None,
        "A repo without a recorded full run has no row to store the fallback bookmark in"
    );
    assert_eq!(checkpoint.get_last_full_run_timestamp().await?, None);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_fallback_bookmark_and_full_run_timestamp_are_independent(
    fb: FacebookInit,
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let checkpoint = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());
    let timestamp1 = Timestamp::from_timestamp_secs(1000000);
    let timestamp2 = Timestamp::from_timestamp_secs(2000000);

    checkpoint.record_full_run(timestamp1, None).await?;
    checkpoint.set_fallback_bookmark("dev1".to_string()).await?;
    assert_eq!(
        checkpoint.get_last_full_run_timestamp().await?,
        Some(timestamp1),
        "Storing a fallback bookmark should keep the full run timestamp"
    );

    checkpoint.record_full_run(timestamp2, None).await?;
    assert_eq!(
        checkpoint.get_fallback_bookmark().await?,
        Some("dev1".to_string()),
        "Recording a full run should keep the stored fallback bookmark"
    );

    checkpoint.set_fallback_bookmark("dev2".to_string()).await?;
    assert_eq!(
        checkpoint.get_fallback_bookmark().await?,
        Some("dev2".to_string()),
        "Storing a new fallback bookmark should replace the previous one"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_record_full_run_with_fallback(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let checkpoint = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());
    let first = Timestamp::from_timestamp_secs(1000000);
    let second = Timestamp::from_timestamp_secs(2000000);

    checkpoint
        .record_full_run(first, Some("dev1".to_string()))
        .await?;
    assert_eq!(checkpoint.get_last_full_run_timestamp().await?, Some(first));
    assert_eq!(
        checkpoint.get_fallback_bookmark().await?,
        Some("dev1".to_string())
    );

    checkpoint
        .record_full_run(second, Some("dev2".to_string()))
        .await?;
    assert_eq!(
        checkpoint.get_last_full_run_timestamp().await?,
        Some(second)
    );
    assert_eq!(
        checkpoint.get_fallback_bookmark().await?,
        Some("dev2".to_string())
    );

    checkpoint.record_full_run(first, None).await?;
    assert_eq!(checkpoint.get_last_full_run_timestamp().await?, Some(first));
    assert_eq!(
        checkpoint.get_fallback_bookmark().await?,
        Some("dev2".to_string())
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_record_full_run_without_fallback_creates_row(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let checkpoint = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());
    let timestamp = Timestamp::from_timestamp_secs(1000000);
    checkpoint.record_full_run(timestamp, None).await?;
    assert_eq!(
        checkpoint.get_last_full_run_timestamp().await?,
        Some(timestamp)
    );
    assert_eq!(checkpoint.get_fallback_bookmark().await?, None);
    let next_timestamp = Timestamp::from_timestamp_secs(2000000);
    checkpoint
        .record_full_run(next_timestamp, Some("dev1".to_string()))
        .await?;
    assert_eq!(
        checkpoint.get_last_full_run_timestamp().await?,
        Some(next_timestamp)
    );
    assert_eq!(
        checkpoint.get_fallback_bookmark().await?,
        Some("dev1".to_string())
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_record_full_run_insert_failure_leaves_no_row(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let conn = open_sqlite_in_memory()?;
    conn.execute_batch(SqlRepoMetadataCheckpointBuilder::CREATION_QUERY)?;
    conn.execute_batch(
        "CREATE TRIGGER reject_insert BEFORE INSERT ON repo_metadata_full_run_info
         BEGIN SELECT RAISE(ABORT, 'injected insert failure'); END;",
    )?;
    let checkpoint = SqlRepoMetadataCheckpointBuilder::from_sql_connections(
        SqlConnections::new_single(Connection::with_sqlite(conn)?),
    )
    .build(REPO_ZERO, ctx.sql_query_telemetry());
    assert!(
        checkpoint
            .record_full_run(Timestamp::now(), Some("dev1".to_string()))
            .await
            .is_err()
    );
    assert_eq!(checkpoint.get_last_full_run_timestamp().await?, None);
    assert_eq!(checkpoint.get_fallback_bookmark().await?, None);
    assert!(!should_skip_full_run(&checkpoint, 2_592_000).await?);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_record_full_run_failure_preserves_both_fields(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let conn = open_sqlite_in_memory()?;
    conn.execute_batch(SqlRepoMetadataCheckpointBuilder::CREATION_QUERY)?;
    conn.execute_batch(
        "CREATE TRIGGER reject_fallback BEFORE UPDATE OF fallback_bookmark
         ON repo_metadata_full_run_info
         BEGIN SELECT RAISE(ABORT, 'injected fallback failure'); END;",
    )?;
    let checkpoint = SqlRepoMetadataCheckpointBuilder::from_sql_connections(
        SqlConnections::new_single(Connection::with_sqlite(conn)?),
    )
    .build(REPO_ZERO, ctx.sql_query_telemetry());
    let first = Timestamp::from_timestamp_secs(1000000);
    let second = Timestamp::from_timestamp_secs(2000000);
    checkpoint
        .record_full_run(first, Some("dev1".to_string()))
        .await?;
    assert!(
        checkpoint
            .record_full_run(second, Some("rejected".to_string()))
            .await
            .is_err()
    );
    assert_eq!(checkpoint.get_last_full_run_timestamp().await?, Some(first));
    assert_eq!(
        checkpoint.get_fallback_bookmark().await?,
        Some("dev1".to_string())
    );
    Ok(())
}

// Tests for full-run periodicity decision logic (should_skip_full_run)

#[mononoke::fbinit_test]
async fn test_should_skip_full_run_no_prior_run(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let checkpoint = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());

    // No timestamp set — should not skip
    let skip = should_skip_full_run(&checkpoint, 2_592_000).await?;
    assert!(!skip);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_should_skip_full_run_recent_run(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let checkpoint = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());

    // Set timestamp to now — elapsed ~0s, well under 30-day threshold → skip
    checkpoint.record_full_run(Timestamp::now(), None).await?;
    let skip = should_skip_full_run(&checkpoint, 2_592_000).await?;
    assert!(skip);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_should_skip_full_run_old_run(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let checkpoint = SqlRepoMetadataCheckpointBuilder::with_sqlite_in_memory()?
        .build(REPO_ZERO, ctx.sql_query_telemetry());

    // Set timestamp to 60 days ago — well over 30-day threshold → should not skip
    let sixty_days_ago =
        Timestamp::from_timestamp_secs(Timestamp::now().timestamp_seconds() - 60 * 24 * 3600);
    checkpoint.record_full_run(sixty_days_ago, None).await?;
    let skip = should_skip_full_run(&checkpoint, 2_592_000).await?;
    assert!(!skip);

    Ok(())
}
