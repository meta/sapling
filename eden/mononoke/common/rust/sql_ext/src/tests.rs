/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use mononoke_types::RepositoryId;
use sql_query_config::SqlQueryConfig;
use sql_query_telemetry::SqlQueryTelemetry;

use crate::Transaction;
use crate::mononoke_queries;

mononoke_queries! {
    read TestQuery(param_str: String, param_uint: u64) -> (u64, Option<i32>, String, i64) {
        "SELECT 44, NULL, {param_str}, {param_uint}"
    }
    pub(crate) cacheable read TestQuery2() -> (u64, Option<String>) {
        "SELECT 44, NULL"
    }
    pub(super) write TestQuery3(values: (
        val1: i32,
    )) {
        none,
        "INSERT INTO my_table (num, str) VALUES {values}"
    }
    write TestQuery4(id: &str) {
        none,
        mysql("DELETE FROM my_table where id = {id}")
        sqlite("DELETE FROM mytable2 where id = {id}")
    }

    // Test to cover fetching the id from the type, not arg name
    read ReadQuery1(id: RepositoryId) -> (i64) {
        "SELECT x FROM mononoke_queries_test_v3 WHERE ID > {id} LIMIT 10"
    }

    write WriteQuery1(values: (x: i64)) {
        none,
        "INSERT INTO mononoke_queries_test_v3 (x) VALUES {values}"
    }

    // Test to cover fetching two repo ids
    read ReadQuery2(small_repo_id: RepositoryId, large_repo_id: RepositoryId) -> (i64) {
        "SELECT x FROM mononoke_queries_test_v3 WHERE ID > {small_repo_id} AND ID > {large_repo_id} LIMIT 10"
    }

    write WriteQuery2(values: (x: i64, repo_id: RepositoryId)) {
        none,
        "INSERT INTO mononoke_queries_test_v3 (x, repo_id) VALUES {values}"
    }

    write WriteQuery3(values: (
        x: i64,
        a: i64,
        b: i64,
        c: i64,
        d: i64,
        e: i64,
        repo_id: RepositoryId,
    )) {
        none,
        "INSERT INTO mononoke_queries_test_v3 (x, a, b, c, d, e, repo_id) VALUES {values}"
    }

    // >tuple_list queries for testing WHERE (col1, col2) IN ((v1, v2), ...)
    write TupleListInsert(values: (repo_id: RepositoryId, x: i64)) {
        none,
        "INSERT INTO tuple_list_test (repo_id, x) VALUES {values}"
    }

    write TupleListDeleteAll() {
        none,
        "DELETE FROM tuple_list_test WHERE 1=1"
    }

    read TupleListQuery(
        >tuple_list pairs: (repo_id: RepositoryId, x: i64)
    ) -> (RepositoryId, i64) {
        "SELECT repo_id, x FROM tuple_list_test WHERE (repo_id, x) IN {pairs}"
    }

    read TupleListQueryWithScalar(
        min_x: i64,
        >tuple_list pairs: (repo_id: RepositoryId, x: i64)
    ) -> (RepositoryId, i64) {
        "SELECT repo_id, x FROM tuple_list_test WHERE x > {min_x} AND (repo_id, x) IN {pairs}"
    }

    read TupleListQuerySingleCol(
        >tuple_list pairs: (x: i64, repo_id: RepositoryId)
    ) -> (i64, RepositoryId) {
        "SELECT x, repo_id FROM tuple_list_test WHERE (x, repo_id) IN {pairs}"
    }
}

#[cfg(fbcode_build)]
#[cfg(test)]
mod facebook {
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::Context;
    use anyhow::anyhow;
    use anyhow::bail;
    use clientinfo::ClientEntryPoint;
    use clientinfo::ClientInfo;
    use clientinfo::ClientRequestInfo;
    use fbinit::FacebookInit;
    use itertools::Itertools;
    use maplit::hashmap;
    use maplit::hashset;
    use metadata::Metadata;
    use mock_transport::MockLoggerScubaTransport;
    use mononoke_macros::mononoke;
    use mononoke_types::Timestamp;
    use mononoke_xdb_telemetry_logger::MononokeXdbTelemetryLogger;
    use mysql_client::InstanceRequirement;
    use sql::mysql::MysqlQueryTelemetry;
    use sql_tests_lib::mysql_test_lib::TEST_XDB_NAME;
    use sql_tests_lib::mysql_test_lib::setup_mysql_test_connection;

    use super::*;
    use crate::Connection;
    use crate::ConsistentReadError;
    use crate::ConsistentReadOptions;
    use crate::SqlConnections;
    use crate::telemetry::TelemetryGranularity;

    struct TelemetryTestData {
        connections: SqlConnections,
        sql_query_tel: SqlQueryTelemetry,
        cri: ClientRequestInfo,
        temp_path: String,
        mock_transport: MockLoggerScubaTransport,
    }

    #[derive(Debug, Clone, serde::Deserialize, PartialEq)]
    struct ScubaTelemetryLogSample {
        mysql_telemetry: MysqlQueryTelemetry,
        success: bool,
        repo_ids: Vec<RepositoryId>,
        granularity: TelemetryGranularity,
        query_name: Option<String>,
        transaction_query_names: Vec<String>,
        shard_name: String,
        attempt: Option<i64>,
    }

    #[mononoke::fbinit_test]
    async fn test_basic_scuba_logging(fb: FacebookInit) -> Result<()> {
        let TelemetryTestData {
            connections,
            sql_query_tel,
            cri,
            temp_path,
            mock_transport: _mock_transport,
        } = setup_scuba_logging_test(fb).await?;

        let connection = connections.write_connection;

        let _res =
            WriteQuery1::query(&connection, sql_query_tel.clone(), &[(&1i64,), (&2i64,)]).await?;

        let expected_repo_id = 1;
        let _res = ReadQuery1::query(
            &connection,
            sql_query_tel,
            &RepositoryId::new(expected_repo_id),
        )
        .await?;

        // Values that we expect to always be the same.
        let expected_values: HashMap<String, serde_json::Value> = hashmap! {
              "client_correlator" => serde_json::Value::String(cri.correlator.to_string()),
              "client_entry_point" => serde_json::Value::String(ClientEntryPoint::Tests.to_string()),
              "repo_id" => serde_json::Value::Number(expected_repo_id.into()),
        }
        .into_iter()
        .map(|(k, v)| (String::from(k), v))
        .collect();

        // Columns expected to be logged in all samples.
        let expected_in_all: HashSet<String> = hashset! {
          "avg_rru",
          "build_revision",
          "build_rule",
          "client_correlator",
          "client_entry_point",
          "client_identities",
          "client_main_id",
          "completion_time_us",
          "cpu_rru",
          "datacenter",
          "delay_rru",
          "delay_rru",
          "full_delay_rru",
          "granularity",
          "instance_type",
          "max_rru",
          "min_rru",
          "overlimit_delay_rru",
          "region_datacenter_prefix",
          "region",
          "server_hostname",
          "session_uuid",
          "shard_name",
          "some_delay_rru",
          "success",
          "task_full_delay_rru",
          "task_some_delay_rru",
          "time",
        }
        .into_iter()
        .map(String::from)
        .collect();

        // Columns expected in some samples, but not necessarily all.
        let expected_in_some: HashSet<String> = hashset! {
            "attempt",
            "query_name",
            "read_tables",
            "signal_time_ENQUEUE",
            "wait_count_ENQUEUE",
            "wait_time_ENQUEUE",
            "write_tables",
            "repo_ids",
        }
        .into_iter()
        .map(String::from)
        .collect();

        // Read the temp file and print its content
        let content = std::fs::read_to_string(&temp_path)?;

        // Extract and print all columns from the scuba logs
        let columns = extract_all_scuba_columns(&content, expected_in_all, expected_values);

        // For debugging purposes. By default will only print if test fails.
        println!("All columns logged in scuba samples: {columns:#?}");

        assert!(
            expected_in_some.is_subset(&columns),
            "Expected columns that should be in at least one sample are missing"
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_transaction_scuba_logging(fb: FacebookInit) -> Result<()> {
        let TelemetryTestData {
            connections,
            sql_query_tel,
            temp_path,
            mock_transport,
            ..
        } = setup_scuba_logging_test(fb).await?;

        let connection = connections.write_connection;

        let _res =
            WriteQuery1::query(&connection, sql_query_tel.clone(), &[(&1i64,), (&2i64,)]).await?;

        let txn = connection.start_transaction(sql_query_tel.clone()).await?;

        // Query with Repo ID 1
        let (txn, _res) = ReadQuery1::query_with_transaction(txn, &RepositoryId::new(1)).await?;
        // Query with Repo ID 2
        let (txn, _res) =
            ReadQuery2::query_with_transaction(txn, &RepositoryId::new(2), &RepositoryId::new(3))
                .await?;

        txn.commit().await?;

        // Verify logs from Scuba file
        let mut scuba_file_logs = deserialize_scuba_log_file(&temp_path)?;
        clear_instance_type(&mut scuba_file_logs);

        println!("scuba_file_logs: {scuba_file_logs:#?}");

        // Verify logs from mock transport
        let mut mock_transport_logs =
            parse_captured_logs(&mock_transport).context("Parsing logger samples")?;
        clear_instance_type(&mut mock_transport_logs);

        println!("mock_transport_logs: {mock_transport_logs:#?}");

        // Expected logs
        let expected_logs = vec![
            ScubaTelemetryLogSample {
                success: true,
                repo_ids: vec![],
                granularity: TelemetryGranularity::Query,
                query_name: Some("WriteQuery1".to_string()),
                shard_name: TEST_XDB_NAME.to_string(),
                mysql_telemetry: MysqlQueryTelemetry {
                    read_tables: hashset! {},
                    write_tables: hashset! {"mononoke_queries_test_v3".to_string()},
                    ..Default::default()
                },
                transaction_query_names: vec![],
                attempt: Some(1),
            },
            ScubaTelemetryLogSample {
                success: true,
                repo_ids: vec![1.into()],
                granularity: TelemetryGranularity::TransactionQuery,
                query_name: Some("ReadQuery1".to_string()),
                shard_name: TEST_XDB_NAME.to_string(),
                mysql_telemetry: MysqlQueryTelemetry {
                    read_tables: hashset! {"mononoke_queries_test_v3".to_string()},
                    write_tables: hashset! {},
                    ..Default::default()
                },
                transaction_query_names: vec![],
                attempt: Some(1),
            },
            ScubaTelemetryLogSample {
                success: true,
                repo_ids: vec![2.into(), 3.into()],
                granularity: TelemetryGranularity::TransactionQuery,
                query_name: Some("ReadQuery2".to_string()),
                shard_name: TEST_XDB_NAME.to_string(),
                mysql_telemetry: MysqlQueryTelemetry {
                    read_tables: hashset! {"mononoke_queries_test_v3".to_string()},
                    write_tables: hashset! {},
                    ..Default::default()
                },
                transaction_query_names: vec![],
                attempt: Some(1),
            },
            // TODO(T223577767): test transaction-level metadata, e.g. run multiple queries
            // for different repos and ensure they are all logged together.
            // Expect a new sample for transaction level
            ScubaTelemetryLogSample {
                success: true,
                repo_ids: vec![1.into(), 2.into(), 3.into()],
                granularity: TelemetryGranularity::Transaction,
                query_name: None,
                shard_name: TEST_XDB_NAME.to_string(),
                mysql_telemetry: MysqlQueryTelemetry {
                    read_tables: hashset! {"mononoke_queries_test_v3".to_string()},
                    write_tables: hashset! {},
                    ..Default::default()
                },
                transaction_query_names: vec!["ReadQuery1", "ReadQuery2"]
                    .into_iter()
                    .map(String::from)
                    .sorted()
                    .collect::<Vec<String>>(),
                attempt: Some(1),
            },
        ];

        // Assert both Scuba file and mock transport have the same expected logs
        pretty_assertions::assert_eq!(
            expected_logs,
            scuba_file_logs,
            "Scuba file logs don't match"
        );
        pretty_assertions::assert_eq!(
            expected_logs,
            mock_transport_logs,
            "Mock transport logs don't match"
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_multiple_repo_ids_in_write_query(fb: FacebookInit) -> Result<()> {
        let TelemetryTestData {
            connections,
            sql_query_tel,
            temp_path,
            mock_transport,
            ..
        } = setup_scuba_logging_test(fb).await?;

        let connection = connections.write_connection;

        let _res = WriteQuery2::query(
            &connection,
            sql_query_tel.clone(),
            &[
                (&1i64, &RepositoryId::new(100)),
                (&2i64, &RepositoryId::new(200)),
                (&1i64, &RepositoryId::new(100)),
            ],
        )
        .await?;

        let _res = WriteQuery3::query(
            &connection,
            sql_query_tel.clone(),
            &[
                (
                    &1i64,
                    &1i64,
                    &1i64,
                    &1i64,
                    &1i64,
                    &1i64,
                    &RepositoryId::new(100),
                ),
                (
                    &1i64,
                    &1i64,
                    &1i64,
                    &1i64,
                    &1i64,
                    &1i64,
                    &RepositoryId::new(200),
                ),
                (
                    &1i64,
                    &1i64,
                    &1i64,
                    &1i64,
                    &1i64,
                    &1i64,
                    &RepositoryId::new(300),
                ),
            ],
        )
        .await?;

        // Verify logs from Scuba file
        let mut scuba_file_logs = deserialize_scuba_log_file(&temp_path)?;
        clear_instance_type(&mut scuba_file_logs);

        println!("scuba_file_logs: {scuba_file_logs:#?}");

        // Verify logs from mock transport
        let mut mock_transport_logs =
            parse_captured_logs(&mock_transport).context("Parsing logger samples")?;
        clear_instance_type(&mut mock_transport_logs);

        println!("mock_transport_logs: {mock_transport_logs:#?}");

        // Expected logs
        let expected_logs = vec![
            ScubaTelemetryLogSample {
                success: true,
                repo_ids: vec![100.into(), 200.into()],
                granularity: TelemetryGranularity::Query,
                query_name: Some("WriteQuery2".to_string()),
                shard_name: TEST_XDB_NAME.to_string(),
                mysql_telemetry: MysqlQueryTelemetry {
                    read_tables: hashset! {},
                    write_tables: hashset! {"mononoke_queries_test_v3".to_string()},
                    ..Default::default()
                },
                transaction_query_names: vec![],
                attempt: Some(1),
            },
            ScubaTelemetryLogSample {
                success: true,
                repo_ids: vec![100.into(), 200.into(), 300.into()],
                granularity: TelemetryGranularity::Query,
                query_name: Some("WriteQuery3".to_string()),
                shard_name: TEST_XDB_NAME.to_string(),
                mysql_telemetry: MysqlQueryTelemetry {
                    read_tables: hashset! {},
                    write_tables: hashset! {"mononoke_queries_test_v3".to_string()},
                    ..Default::default()
                },
                transaction_query_names: vec![],
                attempt: Some(1),
            },
        ];

        // Assert both Scuba file and mock transport have the same expected logs
        pretty_assertions::assert_eq!(
            expected_logs,
            scuba_file_logs,
            "Scuba file logs don't match"
        );
        pretty_assertions::assert_eq!(
            expected_logs,
            mock_transport_logs,
            "Mock transport logs don't match"
        );

        Ok(())
    }

    // Tests that the query is retried to the replica the max amount of times
    // provided if the replica is significantly stale.
    #[mononoke::fbinit_test]
    async fn test_query_with_consistency_fails_with_replica_lagging(
        fb: FacebookInit,
    ) -> Result<()> {
        let TelemetryTestData {
            connections,
            sql_query_tel,
            temp_path,
            mock_transport,
            ..
        } = setup_scuba_logging_test(fb).await?;

        let repo_id = RepositoryId::new(1);
        let cons_read_opts = ConsistentReadOptions {
            max_attempts: 3,
            ..ConsistentReadOptions::default()
        };

        // Date in 2050 to ensure that the HLC check will never succeed.
        let target_lower_bound_hlc = Timestamp::from_timestamp_secs(2547031987);

        let return_early_if: Arc<Box<dyn for<'a> Fn(&'a Vec<_>) -> bool + Send + Sync>> =
            Arc::new(Box::new(|_: &Vec<_>| false));

        let res = ReadQuery1::query_with_consistency(
            &connections,
            sql_query_tel,
            Some(target_lower_bound_hlc),
            Some(return_early_if),
            cons_read_opts,
            &repo_id,
        )
        .await;

        let err = res
            .err()
            .ok_or(anyhow!("Query should have failed with replica lagging"))?;

        let retry_err = err
            .downcast::<ConsistentReadError>()
            .context("downcasting to ConsistentReadError")?;
        match retry_err {
            ConsistentReadError::ReplicaLagging => (),
            _ => bail!("Query should have failed with replica lagging"),
        };

        // Verify logs from Scuba file
        let mut scuba_file_logs = deserialize_scuba_log_file(&temp_path)?;
        clear_instance_type(&mut scuba_file_logs);

        println!("scuba_file_logs: {scuba_file_logs:#?}");

        // Verify logs from mock transport
        let mut mock_transport_logs =
            parse_captured_logs(&mock_transport).context("Parsing logger samples")?;
        clear_instance_type(&mut mock_transport_logs);

        println!("mock_transport_logs: {mock_transport_logs:#?}");

        let expected_log_for_each_attempt = ScubaTelemetryLogSample {
            success: true,
            repo_ids: vec![1.into()],
            granularity: TelemetryGranularity::ConsistentReadQuery,
            query_name: Some("ReadQuery1".to_string()),
            shard_name: TEST_XDB_NAME.to_string(),
            mysql_telemetry: MysqlQueryTelemetry {
                write_tables: hashset! {},
                read_tables: hashset! {"mononoke_queries_test_v3".to_string()},
                ..Default::default()
            },
            transaction_query_names: vec![],
            attempt: Some(1),
        };

        let scuba_query_logs: Vec<_> = scuba_file_logs
            .iter()
            .filter(|log| log.granularity == TelemetryGranularity::ConsistentReadQuery)
            .collect();

        let mock_transport_query_logs: Vec<_> = mock_transport_logs
            .iter()
            .filter(|log| log.granularity == TelemetryGranularity::ConsistentReadQuery)
            .collect();

        // We expect exactly 3 query attempts (max_attempts: 3)
        assert_eq!(
            scuba_query_logs.len(),
            3,
            "Expected 3 ConsistentReadQuery logs from Scuba file, got {}",
            scuba_query_logs.len()
        );
        assert_eq!(
            mock_transport_query_logs.len(),
            3,
            "Expected 3 ConsistentReadQuery logs from mock transport, got {}",
            mock_transport_query_logs.len()
        );

        // Verify each query log matches expected format
        for log in &scuba_query_logs {
            pretty_assertions::assert_eq!(
                &expected_log_for_each_attempt,
                *log,
                "Scuba file query log doesn't match expectation"
            );
        }

        for log in &mock_transport_query_logs {
            pretty_assertions::assert_eq!(
                &expected_log_for_each_attempt,
                *log,
                "Mock transport query log doesn't match expectation"
            );
        }

        Ok(())
    }

    // Tests that the result is returned early when the `return_early_if` callback
    // evaluates to true (i.e. result matches caller expectations) regardless
    // of the replica's HLC.
    #[mononoke::fbinit_test]
    async fn test_query_with_consistency_returns_early_when_data_is_found(
        fb: FacebookInit,
    ) -> Result<()> {
        let TelemetryTestData {
            connections,
            sql_query_tel,
            temp_path,
            mock_transport,
            ..
        } = setup_scuba_logging_test(fb).await?;

        let repo_id = RepositoryId::new(1);
        let cons_read_opts = ConsistentReadOptions {
            max_attempts: 3,
            ..ConsistentReadOptions::default()
        };
        // Date in 2050 to ensure that the HLC check will never succeed.
        let target_lower_bound_hlc = Timestamp::from_timestamp_secs(2547031987);

        let return_early_if: Arc<Box<dyn for<'a> Fn(&'a Vec<_>) -> bool + Send + Sync>> =
            // Callback to always return and simulate scenario where the data
            // requested were found on the first try
            Arc::new(Box::new(|_: &Vec<_>| true));

        let res = ReadQuery1::query_with_consistency(
            &connections,
            sql_query_tel,
            Some(target_lower_bound_hlc),
            Some(return_early_if),
            cons_read_opts,
            &repo_id,
        )
        .await;

        let res = res.ok().ok_or(anyhow!("Query should have succeeded"))?;
        println!("res: {res:?}");
        assert_eq!(res.len(), 10, "query should return 10 rows");

        // Verify logs from Scuba file
        let mut scuba_file_logs = deserialize_scuba_log_file(&temp_path)?;
        clear_instance_type(&mut scuba_file_logs);

        println!("scuba_file_logs: {scuba_file_logs:#?}");

        // Verify logs from mock transport
        let mut mock_transport_logs =
            parse_captured_logs(&mock_transport).context("Parsing logger samples")?;
        clear_instance_type(&mut mock_transport_logs);

        println!("mock_transport_logs: {mock_transport_logs:#?}");

        let expected_logs = vec![
            // Single log for the one and only query that ran
            ScubaTelemetryLogSample {
                success: true,
                repo_ids: vec![1.into()],
                granularity: TelemetryGranularity::ConsistentReadQuery,
                query_name: Some("ReadQuery1".to_string()),
                shard_name: TEST_XDB_NAME.to_string(),
                mysql_telemetry: MysqlQueryTelemetry {
                    write_tables: hashset! {},
                    read_tables: hashset! {"mononoke_queries_test_v3".to_string()},
                    ..Default::default()
                },
                transaction_query_names: vec![],
                attempt: Some(1),
            },
            ScubaTelemetryLogSample {
                success: true,
                repo_ids: vec![1.into()],
                granularity: TelemetryGranularity::ConsistentRead,
                query_name: Some("ReadQuery1".to_string()),
                shard_name: TEST_XDB_NAME.to_string(),
                mysql_telemetry: MysqlQueryTelemetry {
                    write_tables: hashset! {},
                    read_tables: hashset! {"mononoke_queries_test_v3".to_string()},
                    ..Default::default()
                },
                transaction_query_names: vec![],
                attempt: Some(1),
            },
        ];

        // Assert both Scuba file and mock transport have the same expected logs
        pretty_assertions::assert_eq!(expected_logs, scuba_file_logs, "Raw scuba logs don't match");
        pretty_assertions::assert_eq!(
            expected_logs,
            scuba_file_logs,
            "Schematized logger logs don't match"
        );

        Ok(())
    }

    // Test one of the happy paths, where the query is retried multiple times
    // until the replica has caught up.
    #[mononoke::fbinit_test]
    async fn test_query_with_consistency_retries_until_replica_has_caught_up(
        fb: FacebookInit,
    ) -> Result<()> {
        let TelemetryTestData {
            connections,
            sql_query_tel,
            temp_path,
            mock_transport,
            ..
        } = setup_scuba_logging_test(fb).await?;

        let repo_id = RepositoryId::new(1);

        // Set a lower bound HLC of query start time + 200 ms. With the right
        // retry options(e.g. delay, max_attempts), we can ensure the number of
        // queries is always greater than 1 and less than MAX_ATTEMPTS.
        const ARTIFICIAL_DELAY_MS: i64 = 200;
        let now = Timestamp::now().timestamp_nanos();
        let target_lower_bound_hlc =
            Timestamp::from_timestamp_nanos(now + (ARTIFICIAL_DELAY_MS * (1000 * 1000)));

        let return_early_if: Arc<Box<dyn for<'a> Fn(&'a Vec<_>) -> bool + Send + Sync>> =
            // Don't return early to ensure the query finishes only based on the
            // replica's HLC
            Arc::new(Box::new(|_: &Vec<_>| false));

        const MAX_ATTEMPTS: usize = 10;
        let cons_read_opts = ConsistentReadOptions {
            // Higher max attempts just to be safe
            max_attempts: MAX_ATTEMPTS,
            interval: Duration::from_millis(100),
            ..ConsistentReadOptions::default()
        };

        let res = ReadQuery1::query_with_consistency(
            &connections,
            sql_query_tel,
            Some(target_lower_bound_hlc),
            Some(return_early_if),
            cons_read_opts,
            &repo_id,
        )
        .await;

        let res = res.ok().ok_or(anyhow!("Query should have succeeded"))?;
        println!("res: {res:?}");
        assert_eq!(res.len(), 10, "query should return 10 rows");

        // Verify logs from Scuba file
        let mut scuba_file_logs = deserialize_scuba_log_file(&temp_path)?;
        clear_instance_type(&mut scuba_file_logs);

        println!("scuba_file_logs: {scuba_file_logs:#?}");

        // Verify logs from mock transport
        let mut mock_transport_logs =
            parse_captured_logs(&mock_transport).context("Parsing logger samples")?;
        clear_instance_type(&mut mock_transport_logs);

        println!("mock_transport_logs: {mock_transport_logs:#?}");

        assert!(
            scuba_file_logs.len() > 1,
            "Expected at least 2 queries to run"
        );
        assert!(
            scuba_file_logs.len() <= MAX_ATTEMPTS,
            "Expected at most{MAX_ATTEMPTS} queries to run"
        );

        assert!(
            mock_transport_logs.len() > 1,
            "Expected at least 2 queries to run"
        );
        assert!(
            mock_transport_logs.len() <= MAX_ATTEMPTS,
            "Expected at most{MAX_ATTEMPTS} queries to run"
        );

        let expected_log = ScubaTelemetryLogSample {
            success: true,
            repo_ids: vec![1.into()],
            granularity: TelemetryGranularity::ConsistentReadQuery,
            query_name: Some("ReadQuery1".to_string()),
            shard_name: TEST_XDB_NAME.to_string(),
            mysql_telemetry: MysqlQueryTelemetry {
                write_tables: hashset! {},
                read_tables: hashset! {"mononoke_queries_test_v3".to_string()},
                ..Default::default()
            },
            transaction_query_names: vec![],
            attempt: Some(1),
        };

        // Pop the last log for ConsistentRead granularity from both sources
        let scuba_file_cons_read_log = scuba_file_logs
            .pop()
            .ok_or(anyhow!("Expected ConsistentRead log from Scuba file"))?;

        let mock_transport_cons_read_log = mock_transport_logs
            .pop()
            .ok_or(anyhow!("Expected ConsistentRead log from mock transport"))?;

        let expected_final_attempt = scuba_file_logs.len();

        let expected_cons_read_log = ScubaTelemetryLogSample {
            success: true,
            repo_ids: vec![1.into()],
            granularity: TelemetryGranularity::ConsistentRead,
            query_name: Some("ReadQuery1".to_string()),
            shard_name: TEST_XDB_NAME.to_string(),
            mysql_telemetry: MysqlQueryTelemetry {
                write_tables: hashset! {},
                read_tables: hashset! {"mononoke_queries_test_v3".to_string()},
                ..Default::default()
            },
            transaction_query_names: vec![],
            attempt: Some(expected_final_attempt as i64),
        };

        // Check all the query logs from both sources
        scuba_file_logs
            .iter()
            .for_each(|log| pretty_assertions::assert_eq!(expected_log, *log));

        mock_transport_logs
            .iter()
            .for_each(|log| pretty_assertions::assert_eq!(expected_log, *log));

        // Check the ConsistentRead logs from both sources
        pretty_assertions::assert_eq!(
            expected_cons_read_log,
            scuba_file_cons_read_log,
            "Scuba file ConsistentRead log doesn't match expectation"
        );

        pretty_assertions::assert_eq!(
            expected_cons_read_log,
            mock_transport_cons_read_log,
            "Mock transport ConsistentRead log doesn't match expectation"
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_tuple_list_query(fb: FacebookInit) -> Result<()> {
        let master_sql_connection: sql::Connection = setup_mysql_test_connection(
            fb,
            Some(
                "CREATE TABLE IF NOT EXISTS tuple_list_test(
                     id INT AUTO_INCREMENT PRIMARY KEY,
                     repo_id INT UNSIGNED NOT NULL,
                     x INT NOT NULL
                 )",
            ),
            InstanceRequirement::Master,
        )
        .await?;

        let connection = Connection {
            inner: master_sql_connection,
            shard_name: TEST_XDB_NAME.to_string(),
        };

        let sql_query_tel = SqlQueryTelemetry::new(fb, Metadata::default(), None);

        // Clean up any leftover data from previous test runs.
        TupleListDeleteAll::query(&connection, sql_query_tel.clone()).await?;

        // Insert test data: (repo_id=1, x=10), (repo_id=1, x=20), (repo_id=2, x=30), (repo_id=3, x=40)
        TupleListInsert::query(
            &connection,
            sql_query_tel.clone(),
            &[
                (&RepositoryId::new(1), &10i64),
                (&RepositoryId::new(1), &20i64),
                (&RepositoryId::new(2), &30i64),
                (&RepositoryId::new(3), &40i64),
            ],
        )
        .await?;

        // Test 1: Query matching two specific (repo_id, x) pairs
        let results = TupleListQuery::query(
            &connection,
            sql_query_tel.clone(),
            &[(RepositoryId::new(1), 10i64), (RepositoryId::new(2), 30i64)],
        )
        .await?;

        assert_eq!(results.len(), 2, "Should match exactly 2 rows");
        let result_set: HashSet<(i32, i64)> = results
            .iter()
            .map(|(repo_id, x)| (repo_id.id(), *x))
            .collect();
        assert!(
            result_set.contains(&(1, 10)),
            "Should contain (repo_id=1, x=10)"
        );
        assert!(
            result_set.contains(&(2, 30)),
            "Should contain (repo_id=2, x=30)"
        );

        // Test 2: Query with a pair that doesn't exist — no cross-product false positives
        let results = TupleListQuery::query(
            &connection,
            sql_query_tel.clone(),
            &[(RepositoryId::new(1), 30i64)], // repo_id=1 exists, x=30 exists, but not together
        )
        .await?;

        assert_eq!(
            results.len(),
            0,
            "Should not match any rows (no cross-product)"
        );

        // Test 3: Query all inserted pairs
        let results = TupleListQuery::query(
            &connection,
            sql_query_tel.clone(),
            &[
                (RepositoryId::new(1), 10i64),
                (RepositoryId::new(1), 20i64),
                (RepositoryId::new(2), 30i64),
                (RepositoryId::new(3), 40i64),
            ],
        )
        .await?;

        assert_eq!(results.len(), 4, "Should match all 4 rows");

        // Test 4: TupleListQueryWithScalar — combine scalar param with tuple_list
        let results = TupleListQueryWithScalar::query(
            &connection,
            sql_query_tel.clone(),
            &15i64, // min_x > 15
            &[
                (RepositoryId::new(1), 10i64), // x=10 < 15, should be filtered out
                (RepositoryId::new(1), 20i64), // x=20 > 15, should match
                (RepositoryId::new(2), 30i64), // x=30 > 15, should match
            ],
        )
        .await?;

        assert_eq!(results.len(), 2, "Scalar filter should exclude x=10");
        let result_set: HashSet<(i32, i64)> = results
            .iter()
            .map(|(repo_id, x)| (repo_id.id(), *x))
            .collect();
        assert!(
            result_set.contains(&(1, 20)),
            "Should contain (repo_id=1, x=20)"
        );
        assert!(
            result_set.contains(&(2, 30)),
            "Should contain (repo_id=2, x=30)"
        );

        // Test 5: Empty tuple_list should return empty results
        let results = TupleListQuery::query(&connection, sql_query_tel.clone(), &[]).await?;

        assert_eq!(results.len(), 0, "Empty tuple_list should return no rows");

        Ok(())
    }

    /// Runs a query that always fails with `make_err()` through the retry wrapper and returns
    /// how many times it was executed.
    async fn count_executions_of_failing_query(
        fb: FacebookInit,
        make_err: impl Fn() -> anyhow::Error + Send + Sync,
    ) -> usize {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        use crate::_macro_internal::query_with_retry_no_cache;

        let sql_query_tel = SqlQueryTelemetry::new(fb, Metadata::default(), None);
        let executions = AtomicUsize::new(0);
        let res: Result<((), usize)> = query_with_retry_no_cache(
            |_attempt| {
                executions.fetch_add(1, Ordering::SeqCst);
                let err = make_err();
                async move { Err::<(), _>(err) }
            },
            "test_shard",
            "TestRetryPolicy",
            &sql_query_tel,
            TelemetryGranularity::Query,
            &[],
        )
        .await;
        assert!(res.is_err());
        executions.into_inner()
    }

    /// FIXME: BUG! This did execute a query whose first attempt failed with a non-retryable
    /// error a second time (after a 10-15 s backoff) but should have returned the error after
    /// the first execution: only the retryable MySQL errors selected by
    /// `should_retry_mysql_query` are meant to be retried.
    #[mononoke::fbinit_test]
    async fn test_query_with_retry_no_cache_does_not_retry_non_retryable_errors(
        fb: FacebookInit,
    ) -> Result<()> {
        // Let the retry backoff elapse instantly.
        tokio::time::pause();

        // Not a MysqlError at all, so `should_retry_mysql_query` is false for it.
        let executions = count_executions_of_failing_query(fb, || {
            anyhow!("Duplicate entry '1' for key 'PRIMARY'")
        })
        .await;
        // FIXME: BUG! The permanent error is executed twice instead of once.
        assert_eq!(executions, 2);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_query_with_retry_no_cache_retries_deadlocks(fb: FacebookInit) -> Result<()> {
        // Let the retry backoff elapse instantly.
        tokio::time::pause();

        // Errno 1213 (deadlock) is one of the errors `should_retry_mysql_query` retries.
        let executions = count_executions_of_failing_query(fb, || {
            mysql_client::MysqlError::QueryResultError {
                err_type: mysql_client::OperationResult::Failed,
                detail: "Deadlock found when trying to get lock".to_owned(),
                mysql_errno: 1213,
            }
            .into()
        })
        .await;
        assert_eq!(executions, 2);
        Ok(())
    }

    async fn setup_scuba_logging_test(fb: FacebookInit) -> Result<TelemetryTestData> {
        // Create mock transport
        let mock_transport = MockLoggerScubaTransport::new(false);
        // Override the transport before creating logger instance
        MononokeXdbTelemetryLogger::override_scuba_transport_for_testing(
            mock_transport.as_raw_transport_ptr() as usize,
        );

        // Set log file in SQL_TELEMETRY_SCUBA_FILE_PATH environment variable
        let temp_file = tempfile::NamedTempFile::new()?;
        let temp_path = temp_file.path().to_str().unwrap().to_string();
        unsafe {
            std::env::set_var("SQL_TELEMETRY_SCUBA_FILE_PATH", &temp_path);
        }

        let master_sql_connection: sql::Connection = setup_mysql_test_connection(
            fb,
            Some(
                "CREATE TABLE IF NOT EXISTS mononoke_queries_test_v3(
                     x INT,
                     repo_id INT UNSIGNED,
                     y DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                     test CHAR(64),
                     id INT AUTO_INCREMENT,
                     a INT,
                     b INT,
                     c INT,
                     d INT,
                     e INT,
                     f INT,
                     PRIMARY KEY(id)
                 );",
            ),
            InstanceRequirement::Master,
        )
        .await?;

        let master_connection = Connection {
            inner: master_sql_connection,
            shard_name: TEST_XDB_NAME.to_string(),
        };

        // TODO(T237287313): try InstanceRequirement::ReadAfterWriteConsistency
        let read_sql_connection =
            setup_mysql_test_connection(fb, None, InstanceRequirement::ReplicaOnly).await?;

        let read_connection = Connection {
            inner: read_sql_connection,
            shard_name: TEST_XDB_NAME.to_string(),
        };
        let connections = SqlConnections {
            write_connection: master_connection.clone(),
            read_master_connection: master_connection,
            read_connection,
        };

        let client_info = ClientInfo::new_with_entry_point(ClientEntryPoint::Tests)?;
        let cri = client_info
            .request_info
            .clone()
            .expect("client request info missing");

        let mut metadata = Metadata::default();
        metadata.add_client_info(client_info);

        let sql_query_tel = SqlQueryTelemetry::new(fb, metadata, None);

        Ok(TelemetryTestData {
            connections,
            sql_query_tel,
            cri,
            temp_path,
            mock_transport,
        })
    }

    /// Clears the `instance_type` field from all samples.
    /// Whether the test XDB tier returns `instance_type` in MySQL response
    /// attributes depends on infra configuration that has historically been
    /// unstable (see T223577767), so we normalize it away before comparing.
    fn clear_instance_type(logs: &mut [ScubaTelemetryLogSample]) {
        for log in logs.iter_mut() {
            log.mysql_telemetry.instance_type = None;
        }
    }

    /// Parse MononokeXdbTelemetryWrappedLoggerScubaStruct samples from mock transport
    /// into ScubaTelemetryLogSample objects.
    fn parse_captured_logs(
        mock_transport: &MockLoggerScubaTransport,
    ) -> Result<Vec<ScubaTelemetryLogSample>> {
        // Deserialize captured logs using the Wrapped type
        let wrapped_samples: Vec<log_entry::MononokeXdbTelemetryWrappedLoggerScubaStruct> =
            mock_transport
                .get_captured_logs()
                .expect("Failed to deserialize logs");
        wrapped_samples
            .into_iter()
            .map(|wrapped| {
                // Extract the payload from the wrapped struct
                let sample = wrapped
                    .payload
                    .ok_or_else(|| anyhow!("Payload missing from wrapped logger struct"))?;

                // Extract fields from the logger struct
                let success = sample
                    .success_thrift_safe
                    .map(|s| s == 1)
                    .ok_or_else(|| anyhow!("success field missing"))?;

                let granularity = sample
                    .granularity_thrift_safe
                    .ok_or_else(|| anyhow!("granularity field missing"))
                    .and_then(|g| {
                        serde_json::from_str::<TelemetryGranularity>(&format!("\"{g}\""))
                            .context("Failed to parse granularity")
                    })?;

                let query_name = sample.query_name_thrift_safe;

                let shard_name = sample
                    .shard_name_thrift_safe
                    .ok_or_else(|| anyhow!("shard_name field missing"))?;

                // Parse repo_ids from Vec<String> and deduplicate
                let repo_ids: Vec<RepositoryId> = sample
                    .repo_ids_thrift_safe
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|id| id.parse::<i32>().ok().map(RepositoryId::new))
                            .collect::<HashSet<_>>()
                            .into_iter()
                            .sorted()
                            .collect()
                    })
                    .unwrap_or_default();

                // Parse transaction_query_names from Vec<String>
                let transaction_query_names: Vec<String> = sample
                    .transaction_query_names_thrift_safe
                    .map(|names| names.into_iter().sorted().collect())
                    .unwrap_or_default();

                // Parse read_tables and write_tables from Vec<String>
                let read_tables: HashSet<String> = sample
                    .read_tables_thrift_safe
                    .map(|tables| tables.into_iter().collect())
                    .unwrap_or_default();

                let write_tables: HashSet<String> = sample
                    .write_tables_thrift_safe
                    .map(|tables| tables.into_iter().collect())
                    .unwrap_or_default();

                let instance_type = sample.instance_type_thrift_safe;

                // Build MysqlQueryTelemetry with the extracted fields
                let mysql_telemetry = MysqlQueryTelemetry {
                    read_tables,
                    write_tables,
                    instance_type,
                    ..Default::default()
                };

                let attempt = sample.attempt_thrift_safe;

                Ok(ScubaTelemetryLogSample {
                    mysql_telemetry,
                    success,
                    repo_ids,
                    granularity,
                    query_name,
                    transaction_query_names,
                    shard_name,
                    attempt,
                })
            })
            .collect::<Result<Vec<_>>>()
    }

    /// Extracts all column names from scuba samples in the log content
    fn extract_all_scuba_columns(
        log_content: &str,
        expected_in_all: HashSet<String>,
        expected_values: HashMap<String, serde_json::Value>,
    ) -> HashSet<String> {
        log_content
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .fold(HashSet::new(), |mut all_columns, json| {
            let sample_columns = extract_columns_from_sample(&json, &expected_values);

            assert!(
                expected_in_all.is_subset(&sample_columns),
                "Expected columns that should be in all samples are missing: {0:#?}. Sample: {1:#?}",
                expected_in_all
                    .difference(&sample_columns)
                    .collect::<Vec<_>>(),
                    log_content
            );

            all_columns.extend(sample_columns);
            all_columns
        })
    }

    /// Extracts column names from a single scuba sample
    fn extract_columns_from_sample(
        sample: &serde_json::Value,
        expected_values: &HashMap<String, serde_json::Value>,
    ) -> HashSet<String> {
        // Check each category (normal, int, double, normvector)
        if let Some(obj) = sample.as_object() {
            return obj
                .iter()
                .fold(HashSet::new(), |mut acc, (_category, value)| {
                    if let Some(category_obj) = value.as_object() {
                        expected_values.iter().for_each(|(exp_key, exp_v)| {
                            // Check if the key is inside the value object
                            // and if it is, assert the value is the same as expected
                            if let Some(value) = category_obj.get(exp_key) {
                                assert_eq!(
                                    exp_v,
                                    value,
                                    "Expected value {0} for key {1} but got {2}",
                                    exp_v,
                                    exp_key,
                                    value.as_str().unwrap_or_default()
                                );
                            };
                        });
                        // Add each column name from this category
                        acc.extend(category_obj.keys().cloned());
                    }
                    acc
                });
        }

        HashSet::new()
    }

    /// Reads the scuba log file and parses all samples as ScubaTelemetryLogSample
    fn deserialize_scuba_log_file(scuba_log_file: &str) -> Result<Vec<ScubaTelemetryLogSample>> {
        use std::fs::File;
        use std::io::BufRead;
        use std::io::BufReader;

        let file = File::open(scuba_log_file)?;
        let reader = BufReader::new(file);

        // Collect all lines first (not efficient for very large files, but works for test logs)
        let lines: Vec<String> = reader.lines().collect::<std::io::Result<_>>()?;

        // Parse each line as a ScubaTelemetryLogSample object
        let mysql_tels: Vec<ScubaTelemetryLogSample> = lines
            .into_iter()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(&line)
                    .map_err(anyhow::Error::from)
                    .and_then(|json| {
                        // Scuba groups the logs by type (e.g. int, normal), so
                        // let's remove those and flatten the sample into a single
                        // json object.
                        let flattened_log = json
                            .as_object()
                            .iter()
                            .flat_map(|obj| {
                                obj.iter().flat_map(|(_, category_values)| {
                                    category_values.as_object().into_iter().flat_map(
                                        |category_obj| {
                                            category_obj.iter().map(|(k, v)| (k.clone(), v.clone()))
                                        },
                                    )
                                })
                            })
                            .collect::<serde_json::Value>();

                        // println!("flattened_log: {flattened_log:#?}");

                        let success: bool = flattened_log["success"]
                            .as_i64()
                            .map(|i| i == 1)
                            .expect("success should always be logged");
                        let granularity = serde_json::from_value::<TelemetryGranularity>(
                            flattened_log["granularity"].clone(),
                        )?;

                        let query_name: Option<String> =
                            flattened_log["query_name"].as_str().map(String::from);

                        let shard_name: String = flattened_log["shard_name"]
                            .as_str()
                            .map(String::from)
                            .ok_or(anyhow!("Failed to parse shard_name from logs"))?;

                        let repo_ids: Vec<RepositoryId> = flattened_log["repo_ids"]
                            .as_array()
                            .map(|ids| {
                                ids.iter()
                                    .filter_map(|id| {
                                        id.as_str()
                                            .and_then(|s| s.parse::<i32>().ok())
                                            .map(RepositoryId::new)
                                    })
                                    .sorted()
                                    .collect()
                            })
                            .unwrap_or_default();

                        let transaction_query_names: Vec<String> =
                            flattened_log["transaction_query_names"]
                                .as_array()
                                .map(|ids| {
                                    ids.iter()
                                        .filter_map(|id| id.as_str())
                                        .map(String::from)
                                        .sorted()
                                        .collect()
                                })
                                .unwrap_or_default();

                        let attempt: Option<i64> = flattened_log["attempt"].as_i64();

                        // Now deserialize that into a MysqlQueryTelemetry object
                        let mysql_tel =
                            serde_json::from_value::<MysqlQueryTelemetry>(flattened_log)
                                .context("Deserializing MysqlQueryTelemetry")?;

                        Ok(ScubaTelemetryLogSample {
                            mysql_telemetry: mysql_tel,
                            success,
                            repo_ids,
                            granularity,
                            query_name,
                            transaction_query_names,
                            shard_name,
                            attempt,
                        })
                    })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(mysql_tels)
    }
}
