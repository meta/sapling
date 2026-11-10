/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Mutable counters maintains numeric counters for each Mononoke repository.
//! These are used to maintain simple state about each repo, for example which
//! revisions have been replayed, etc.
//!
//! The counter values themselves are stored in a table in the metadata
//! database.

use anyhow::Result;
use anyhow::ensure;
use anyhow::format_err;
use async_trait::async_trait;
use context::CoreContext;
use context::PerfCounterType;
use metaconfig_types::OssRemoteDatabaseConfig;
use metaconfig_types::OssRemoteMetadataDatabaseConfig;
use metaconfig_types::RemoteDatabaseConfig;
use metaconfig_types::RemoteMetadataDatabaseConfig;
use mononoke_types::RepositoryId;
use sql_construct::SqlConstruct;
use sql_construct::SqlConstructFromMetadataDatabaseConfig;
use sql_ext::SqlConnections;
use sql_ext::Transaction as SqlTransaction;
use sql_ext::TransactionResult;
use sql_ext::mononoke_queries;
use stats::prelude::*;

define_stats! {
    prefix = "mononoke.mutable_counters";
    cur_value: dynamic_singleton_counter("{}.cur_value", (name: String)),
}

/// Maximum supported mutable-counter name length.
pub const MAX_COUNTER_NAME_LENGTH: usize = 640;

pub fn validate_counter_name(name: &str) -> Result<()> {
    ensure!(
        name.len() <= MAX_COUNTER_NAME_LENGTH,
        "mutable counter name is {} bytes; maximum is {}",
        name.len(),
        MAX_COUNTER_NAME_LENGTH,
    );
    Ok(())
}

#[facet::facet]
#[async_trait]
pub trait MutableCounters {
    /// Get the current value of the counter
    async fn get_counter(&self, ctx: &CoreContext, name: &str) -> Result<Option<i64>>;

    async fn get_maybe_stale_counter(&self, ctx: &CoreContext, name: &str) -> Result<Option<i64>>;

    /// Set the current value of the counter. if `prev_value` is not None,
    /// then the value is only updated if the previous value matches.
    async fn set_counter(
        &self,
        ctx: &CoreContext,
        name: &str,
        value: i64,
        prev_value: Option<i64>,
    ) -> Result<bool>;

    /// Unconditionally set several counters atomically: either every
    /// counter is written, or none is and an error is returned. Use this
    /// when readers interpret a group of counters together.
    ///
    /// There is deliberately no default implementation: one built from
    /// repeated `set_counter` calls would not be atomic.
    async fn set_counters(&self, ctx: &CoreContext, counters: &[(&str, i64)]) -> Result<()>;

    /// Initialize a counter if it does not exist yet. Returns whether this
    /// call inserted the counter.
    async fn set_counter_if_absent(
        &self,
        ctx: &CoreContext,
        name: &str,
        value: i64,
    ) -> Result<bool>;

    /// Get the names and values of all the counters for the repository.
    async fn get_all_counters(&self, ctx: &CoreContext) -> Result<Vec<(String, i64)>>;
}

mononoke_queries! {
    write SetCounter(
        repo_id: RepositoryId, name: &str, value: i64
    ) {
        none,
        mysql(
            "REPLACE INTO mutable_counters (repo_id, name, value) VALUES ({repo_id}, {name}, {value})"
        )
        sqlite(
            "REPLACE INTO mutable_counters (repo_id, name, value) VALUES ({repo_id}, CAST({name} AS TEXT), {value})"
        )
    }

    write SetCounterConditionally(
        repo_id: RepositoryId, name: &str, value: i64, prev_value: i64
    ) {
        none,
        mysql(
            "UPDATE mutable_counters SET value = {value}
            WHERE repo_id = {repo_id} AND name = {name} AND value = {prev_value}"
        )
        sqlite(
            "UPDATE mutable_counters SET value = {value}
            WHERE repo_id = {repo_id} AND name = CAST({name} AS TEXT) AND value = {prev_value}"
        )
    }

    write SetCounterIfAbsent(
        repo_id: RepositoryId, name: &str, value: i64
    ) {
        none,
        mysql(
            "INSERT IGNORE INTO mutable_counters (repo_id, name, value)
             VALUES ({repo_id}, {name}, {value})"
        )
        sqlite(
            "INSERT INTO mutable_counters (repo_id, name, value)
             VALUES ({repo_id}, CAST({name} AS TEXT), {value})
             ON CONFLICT(repo_id, name) DO NOTHING"
        )
    }

    read GetCounter(repo_id: RepositoryId, name: &str) -> (i64) {
        mysql(
            "SELECT value FROM mutable_counters WHERE repo_id = {repo_id} and name = {name}"
        )
        sqlite(
            "SELECT value FROM mutable_counters WHERE repo_id = {repo_id} and name = CAST({name} AS TEXT)"
        )
    }

    read GetCountersForRepo(repo_id: RepositoryId) -> (String, i64) {
        "SELECT name, value FROM mutable_counters WHERE repo_id = {repo_id} ORDER BY name"
    }
}

pub struct SqlMutableCounters {
    repo_id: RepositoryId,
    connections: SqlConnections,
}

pub struct SqlMutableCountersBuilder {
    connections: SqlConnections,
}

impl SqlConstruct for SqlMutableCountersBuilder {
    const LABEL: &'static str = "mutable_counters";

    const CREATION_QUERY: &'static str = include_str!("../schemas/sqlite-mutable-counters.sql");

    fn from_sql_connections(connections: SqlConnections) -> Self {
        Self { connections }
    }
}

impl SqlConstructFromMetadataDatabaseConfig for SqlMutableCountersBuilder {
    fn remote_database_config(
        remote: &RemoteMetadataDatabaseConfig,
    ) -> Option<&RemoteDatabaseConfig> {
        Some(&remote.bookmarks)
    }
    fn oss_remote_database_config(
        remote: &OssRemoteMetadataDatabaseConfig,
    ) -> Option<&OssRemoteDatabaseConfig> {
        Some(&remote.bookmarks)
    }
}

impl SqlMutableCountersBuilder {
    pub fn build(self, repo_id: RepositoryId) -> SqlMutableCounters {
        SqlMutableCounters {
            repo_id,
            connections: self.connections,
        }
    }
}

#[async_trait]
impl MutableCounters for SqlMutableCounters {
    async fn get_counter(&self, ctx: &CoreContext, name: &str) -> Result<Option<i64>> {
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsMaster);
        let conn = &self.connections.read_master_connection;
        let counter =
            GetCounter::query(conn, ctx.sql_query_telemetry(), &self.repo_id, &name).await?;
        Ok(counter.first().map(|entry| entry.0))
    }

    async fn get_maybe_stale_counter(&self, ctx: &CoreContext, name: &str) -> Result<Option<i64>> {
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsReplica);
        let conn = &self.connections.read_connection;
        let counter =
            GetCounter::query(conn, ctx.sql_query_telemetry(), &self.repo_id, &name).await?;
        Ok(counter.first().map(|entry| entry.0))
    }

    async fn set_counter(
        &self,
        ctx: &CoreContext,
        name: &str,
        value: i64,
        prev_value: Option<i64>,
    ) -> Result<bool> {
        validate_counter_name(name)?;
        let conn = &self.connections.write_connection;
        let txn = conn.start_transaction(ctx.sql_query_telemetry()).await?;

        let txn_result =
            Self::set_counter_on_txn(ctx, self.repo_id, name, value, prev_value, txn).await?;
        match txn_result {
            TransactionResult::Succeeded(txn) => {
                txn.commit().await?;
                STATS::cur_value.set_value(ctx.fb, value, (name.to_owned(),));
                Ok(true)
            }
            TransactionResult::Failed => Ok(false),
        }
    }

    async fn set_counters(&self, ctx: &CoreContext, counters: &[(&str, i64)]) -> Result<()> {
        if counters.is_empty() {
            return Ok(());
        }
        counters
            .iter()
            .try_for_each(|(name, _)| validate_counter_name(name))?;

        let conn = &self.connections.write_connection;
        let mut txn = conn.start_transaction(ctx.sql_query_telemetry()).await?;
        // Any early return below drops `txn` uncommitted, which rolls back
        // the counters already written in it.
        for (name, value) in counters {
            txn = match Self::set_counter_on_txn(ctx, self.repo_id, name, *value, None, txn).await?
            {
                TransactionResult::Succeeded(txn) => txn,
                TransactionResult::Failed => {
                    return Err(format_err!(
                        "failed to set mutable counter {name:?} to {value}; no counter in {counters:?} was written",
                    ));
                }
            };
        }
        txn.commit().await?;

        counters.iter().for_each(|(name, value)| {
            STATS::cur_value.set_value(ctx.fb, *value, ((*name).to_owned(),))
        });
        Ok(())
    }

    async fn set_counter_if_absent(
        &self,
        ctx: &CoreContext,
        name: &str,
        value: i64,
    ) -> Result<bool> {
        validate_counter_name(name)?;
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);
        let result = SetCounterIfAbsent::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &self.repo_id,
            &name,
            &value,
        )
        .await?;
        // INSERT IGNORE has unambiguous row accounting even if a client uses
        // CLIENT_FOUND_ROWS: an insert affects one row and an existing primary
        // key affects none. Name validation above prevents the only input
        // warning (truncation) that IGNORE could otherwise suppress.
        let inserted = result.affected_rows() == 1;
        let observed_value = if inserted {
            value
        } else {
            GetCounter::query(
                &self.connections.write_connection,
                ctx.sql_query_telemetry(),
                &self.repo_id,
                &name,
            )
            .await?
            .first()
            .map(|entry| entry.0)
            .ok_or_else(|| {
                format_err!(
                    "mutable counter insert was ignored, but counter {name:?} does not exist"
                )
            })?
        };
        STATS::cur_value.set_value(ctx.fb, observed_value, (name.to_owned(),));
        Ok(inserted)
    }

    async fn get_all_counters(&self, ctx: &CoreContext) -> Result<Vec<(String, i64)>> {
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsMaster);
        let conn = &self.connections.read_master_connection;
        let counters =
            GetCountersForRepo::query(conn, ctx.sql_query_telemetry(), &self.repo_id).await?;
        Ok(counters.into_iter().collect())
    }
}

impl SqlMutableCounters {
    pub async fn set_counter_on_txn(
        ctx: &CoreContext,
        repo_id: RepositoryId,
        name: &str,
        value: i64,
        prev_value: Option<i64>,
        txn: SqlTransaction,
    ) -> Result<TransactionResult> {
        validate_counter_name(name)?;
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);
        let (txn, result) = if let Some(prev_value) = prev_value {
            SetCounterConditionally::query_with_transaction(
                txn,
                &repo_id,
                &name,
                &value,
                &prev_value,
            )
            .await?
        } else {
            SetCounter::query_with_transaction(txn, &repo_id, &name, &value).await?
        };

        Ok(if result.affected_rows() >= 1 {
            TransactionResult::Succeeded(txn)
        } else {
            TransactionResult::Failed
        })
    }
}
