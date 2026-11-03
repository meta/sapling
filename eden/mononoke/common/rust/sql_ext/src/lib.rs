/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![cfg_attr(fbcode_build, feature(once_cell_try))]

mod hasher;
mod mononoke_queries;
#[cfg(not(fbcode_build))]
mod oss;
pub mod replication;

mod sqlite;
mod telemetry;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use derivative::Derivative;
use futures_stats::FutureStats;
use mononoke_types::RepositoryId;
use rusqlite::Connection as SqliteConnection;
use sql::Connection as SqlConnection;
use sql::QueryTelemetry;
use sql::Transaction as SqlTransaction;
use sql::sqlite::SqliteHlcProvider;
use sql_common::sqlite::SqliteCallbacks;
pub use sql_query_telemetry::SqlQueryTelemetry;
pub use sqlite::open_existing_sqlite_path;
pub use sqlite::open_sqlite_in_memory;
pub use sqlite::open_sqlite_path;
use thiserror::Error;
use vec1::Vec1;

pub use crate::mononoke_queries::should_retry_mysql_query as should_retry_query;
use crate::telemetry::TelemetryGranularity;
use crate::telemetry::TransactionTelemetry;
use crate::telemetry::log_query_telemetry;
use crate::telemetry::log_transaction_telemetry;

#[must_use]
pub enum TransactionResult {
    Succeeded(Transaction),
    Failed,
}

pub mod _macro_internal {
    pub use std::collections::hash_map::DefaultHasher;
    pub use std::hash::Hash;
    pub use std::hash::Hasher;
    pub use std::sync::Arc;

    pub use anyhow::Result;
    pub use borrowed::borrowed;
    pub use clientinfo::ClientEntryPoint;
    pub use clientinfo::ClientRequestInfo;
    pub use cloned::cloned;
    pub use futures_stats::TimedTryFutureExt;
    pub use mononoke_types::RepositoryId;
    pub use mononoke_types::Timestamp;
    pub use paste;
    pub use serde_json;
    pub use sql::QueryTelemetry;
    pub use sql::WriteResult;
    pub use sql::queries;
    pub use sql_ext_proc_macros::extract_repo_ids_from_values;
    pub use sql_query_config::SqlQueryConfig;
    pub use sql_query_telemetry::SqlQueryTelemetry;

    pub use crate::Connection;
    pub use crate::ConsistentReadError;
    pub use crate::ConsistentReadOptions;
    pub use crate::SqlConnections;
    pub use crate::Transaction;
    pub use crate::hasher::XxHash3_128;
    pub use crate::mononoke_queries::CacheData;
    pub use crate::mononoke_queries::CachedQueryResult;
    pub use crate::mononoke_queries::query_with_consistency_no_cache;
    pub use crate::mononoke_queries::query_with_retry;
    pub use crate::mononoke_queries::query_with_retry_no_cache;
    pub use crate::telemetry::TelemetryGranularity;
    pub use crate::telemetry::log_query_error;
    pub use crate::telemetry::log_query_telemetry;
    pub use crate::telemetry::log_transaction_telemetry;

    /// Re-exports for `>tuple_list` support in `mononoke_queries!`.
    /// These types are needed to directly match on `sql::Connection`
    /// variants without going through `sql::queries!`.
    pub mod _tl {
        pub use anyhow;
        pub use sql::Connection as InnerSqlConnection;
        pub use sql::ValueWrapper;
        pub use sql::mysql_async;
        pub use sql::rusqlite;
        pub use sql::sql_common::QueryTelemetry as InnerQueryTelemetry;
        pub use sql::sql_common::mysql::OssConnection;
        pub use sql::sqlite::SqliteMultithreaded;
        pub use sql::sqlite::SqliteQueryTelemetry;
        pub use sql::sqlite::SqliteQueryType;
    }
}

/// Wrapper over the SQL transaction that will keep track of telemetry from the
/// entire transaction.
pub struct Transaction {
    pub inner: SqlTransaction,

    pub txn_telemetry: TransactionTelemetry,

    pub sql_query_tel: SqlQueryTelemetry,

    pub shard_name: String,
}

impl Transaction {
    pub fn new(
        sql_txn: SqlTransaction,
        txn_telemetry: TransactionTelemetry,
        sql_query_tel: SqlQueryTelemetry,
        shard_name: String,
    ) -> Self {
        Self {
            inner: sql_txn,
            txn_telemetry,
            sql_query_tel,
            shard_name,
        }
    }

    pub fn add_sql_query_tel(self, sql_query_tel: SqlQueryTelemetry) -> Self {
        Self {
            sql_query_tel,
            ..self
        }
    }

    /// Perform a commit on this transaction
    pub async fn commit(self) -> Result<()> {
        let Transaction {
            inner: sql_txn,
            txn_telemetry,
            sql_query_tel,
            shard_name,
        } = self;

        log_transaction_telemetry(txn_telemetry, sql_query_tel, shard_name.as_ref())?;

        sql_txn.commit().await
    }

    /// Perform a rollback on this transaction
    pub async fn rollback(self) -> Result<()> {
        self.inner.rollback().await
    }

    pub fn from_transaction_query_result(
        sql_txn: SqlTransaction,
        opt_tel: Option<QueryTelemetry>,
        mut txn_telemetry: TransactionTelemetry,
        sql_query_tel: SqlQueryTelemetry,
        query_repo_ids: Vec<RepositoryId>,
        granularity: TelemetryGranularity,
        query_name: &str,
        shard_name: String,
        fut_stats: FutureStats,
        attempt: usize,
    ) -> Result<Self> {
        if let Some(tel) = opt_tel.as_ref() {
            txn_telemetry.add_query_telemetry(tel.clone())
        };

        txn_telemetry.add_repo_ids(query_repo_ids.clone());
        txn_telemetry.add_query_name(query_name);

        log_query_telemetry(
            opt_tel,
            &sql_query_tel,
            granularity,
            &query_repo_ids,
            query_name,
            shard_name.as_ref(),
            fut_stats,
            Some(attempt),
        )?;

        Ok(Transaction::new(
            sql_txn,
            txn_telemetry,
            sql_query_tel,
            shard_name,
        ))
    }
}

pub mod facebook {
    #[cfg(fbcode_build)]
    mod r#impl;

    use std::fmt;
    use std::fmt::Debug;

    #[cfg(fbcode_build)]
    pub use r#impl::PoolConfig;
    #[cfg(fbcode_build)]
    pub use r#impl::SharedConnectionPool;
    #[cfg(fbcode_build)]
    pub use r#impl::create_mysql_connections_sharded;
    #[cfg(fbcode_build)]
    pub use r#impl::create_mysql_connections_unsharded;
    #[cfg(fbcode_build)]
    pub use r#impl::create_oss_mysql_connections_unsharded;
    #[cfg(fbcode_build)]
    pub use r#impl::myadmin::MyAdmin;
    #[cfg(fbcode_build)]
    pub use r#impl::myadmin::MyAdminLagMonitor;
    #[cfg(fbcode_build)]
    pub use r#impl::myadmin::replication_status_chunked;

    #[cfg(not(fbcode_build))]
    pub use crate::oss::MyAdmin;
    #[cfg(not(fbcode_build))]
    pub use crate::oss::MyAdminLagMonitor;
    #[cfg(not(fbcode_build))]
    pub use crate::oss::PoolConfig;
    #[cfg(not(fbcode_build))]
    pub use crate::oss::SharedConnectionPool;
    #[cfg(not(fbcode_build))]
    pub use crate::oss::create_mysql_connections_sharded;
    #[cfg(not(fbcode_build))]
    pub use crate::oss::create_mysql_connections_unsharded;

    /// MySQL global shared connection pool configuration.
    #[derive(Clone, Default)]
    pub struct MysqlOptions {
        pub pool: SharedConnectionPool,
        // pool config is used only once when the shared connection pool is being created
        pub pool_config: PoolConfig,
        pub read_connection_type: ReadConnectionType,
    }

    impl MysqlOptions {
        pub fn per_key_limit(&self) -> Option<usize> {
            #[cfg(not(fbcode_build))]
            {
                None
            }
            #[cfg(fbcode_build)]
            {
                Some(self.pool_config.per_key_limit as usize)
            }
        }
    }

    impl Debug for MysqlOptions {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(
                f,
                "MySQL pool with config {:?}, connection type: {:?}",
                self.pool_config, self.read_connection_type
            )
        }
    }

    /// Mirrors facebook::db::InstanceRequirement enum for DBLocator
    #[derive(Copy, Clone, Debug, Default)]
    pub enum ReadConnectionType {
        /// Choose master or replica, whatever is closest and available.
        /// Use this if both master and replica are in the same region, and reads
        /// should we served by both.
        Closest,
        /// Choose replicas only, avoiding the master, even if it means going to a
        /// remote region.
        #[default]
        ReplicaOnly,
        /// Choose master only (typically for writes). Will never connect to replica.
        Master,
        /// Choose closer first and inside the same region, replicas first.
        /// In case both master and replica in the same region - all reads
        /// will be routed to the replica.
        ReplicaFirst,
        /// Choose replicas that satisfy a lower bound HLC value in order to
        /// perform consistent read-your-writes operations
        ReadAfterWriteConsistency,
    }
}

/// Wrapper over the SQL connection, needed for telemetry
#[derive(Clone, Debug)]
pub struct Connection {
    pub inner: SqlConnection,
    /// Name of the shard (i.e. DB) the connection belongs to
    pub shard_name: String,
}

impl From<Connection> for SqlConnection {
    fn from(conn: Connection) -> Self {
        conn.inner
    }
}

impl Connection {
    pub async fn start_transaction(&self, sql_query_tel: SqlQueryTelemetry) -> Result<Transaction> {
        let sql_txn = self.inner.start_transaction().await?;
        let txn_telemetry = Default::default();
        Ok(Transaction::new(
            sql_txn,
            txn_telemetry,
            sql_query_tel,
            self.shard_name.clone(),
        ))
    }

    pub fn sql_connection(&self) -> &SqlConnection {
        &self.inner
    }

    pub fn shard_name(&self) -> &str {
        &self.shard_name
    }

    pub fn with_sqlite(con: SqliteConnection) -> Result<Self> {
        let shard_name = con.db_name(0)?;
        let inner = SqlConnection::with_sqlite(con);
        Ok(Connection { inner, shard_name })
    }

    /// Given a `rusqlite::Connection` create a connection to Sqlite database that might be used
    /// by this crate, and add callbacks for when operations happen.
    pub fn with_sqlite_callbacks(
        con: SqliteConnection,
        callbacks: Box<dyn SqliteCallbacks>,
    ) -> Result<Self> {
        let shard_name = con.db_name(0)?;
        let inner = SqlConnection::with_sqlite_callbacks(con, callbacks);
        Ok(Connection { inner, shard_name })
    }

    pub fn with_sqlite_hlc_provider_and_callbacks(
        con: SqliteConnection,
        hlc_provider: Arc<Box<SqliteHlcProvider>>,
        callbacks: Box<dyn SqliteCallbacks>,
    ) -> Result<Self> {
        let shard_name = con.db_name(0)?;
        let inner =
            SqlConnection::with_sqlite_hlc_provider_and_callbacks(con, hlc_provider, callbacks);
        Ok(Connection { inner, shard_name })
    }
}

/// Struct to store a set of write, read and read-only connections for a shard.
#[derive(Clone)]
pub struct SqlConnections {
    /// Write connection to the master
    pub write_connection: Connection,
    /// Read connection
    pub read_connection: Connection,
    /// Read master connection
    pub read_master_connection: Connection,
}

impl SqlConnections {
    /// Create SqlConnections from a single connection.
    pub fn new_single(connection: Connection) -> Self {
        Self {
            write_connection: connection.clone(),
            read_connection: connection.clone(),
            read_master_connection: connection,
        }
    }
}

impl From<SqlConnections> for sql::SqlConnections {
    fn from(conn: SqlConnections) -> Self {
        Self {
            write_connection: conn.write_connection.inner,
            read_connection: conn.read_connection.inner,
            read_master_connection: conn.read_master_connection.inner,
        }
    }
}

/// Struct to store a set of write, read and read-only connections for multiple shards.
#[derive(Clone)]
pub struct SqlShardedConnections {
    /// Write connections to the master for each shard
    pub write_connections: Vec1<Connection>,
    /// Read connections for each shard
    pub read_connections: Vec1<Connection>,
    /// Read master connections for each shard
    pub read_master_connections: Vec1<Connection>,
}

impl From<Vec1<SqlConnections>> for SqlShardedConnections {
    fn from(shard_connections: Vec1<SqlConnections>) -> Self {
        let (head, last) = shard_connections.split_off_last();
        let (write_connections, read_connections, read_master_connections) =
            itertools::multiunzip(head.into_iter().map(|conn| {
                (
                    conn.write_connection,
                    conn.read_connection,
                    conn.read_master_connection,
                )
            }));

        Self {
            read_connections: Vec1::from_vec_push(read_connections, last.read_connection),
            read_master_connections: Vec1::from_vec_push(
                read_master_connections,
                last.read_master_connection,
            ),
            write_connections: Vec1::from_vec_push(write_connections, last.write_connection),
        }
    }
}

#[derive(Debug, Derivative)]
#[derivative(Default)]
pub struct ConsistentReadOptions {
    /// Maximum number of retry attempts.
    #[derivative(Default(value = "3"))]
    pub max_attempts: usize,

    /// Interval between retry attempts (in milliseconds)
    #[derivative(Default(value = "Duration::from_millis(50)"))]
    pub interval: Duration,

    /// Random delay added to retry backoff.
    #[derivative(Default(value = "Duration::from_millis(10)"))]
    pub jitter: Duration,

    /// Base multiplier for exponential backoff.
    #[derivative(Default(value = "1.2"))]
    pub exp_backoff_base: f64,

    /// Buffer added to replica's HLC timestamp for testing (in nanoseconds).
    ///
    /// Used to simulate replication lag in tests by artificially lowering
    /// the replica's timestamp. Set to a negative value to trigger "fake
    /// replication lag" conditions for testing retry logic.
    #[derivative(Default(value = "0"))]
    pub hlc_drift_tolerance_ns: i64,
}

/// Errors that can occur during consistent read operations using HLC (Hybrid Logical Clock)
///
/// This error type is used by `query_with_consistency_no_cache` to handle scenarios where
/// we need to ensure that a replica has caught up to the master's state before serving reads.
/// The function uses HLC timestamps to determine if the replica is sufficiently up-to-date
/// and retries the query if needed.
#[derive(Debug, Error)]
pub enum ConsistentReadError {
    /// The replica has not yet caught up to the required HLC timestamp.
    ///
    /// This error is returned when the replica's HLC timestamp is older than the start
    /// time of the operation plus the configured buffer. When this error occurs, the
    /// query is retried with exponential backoff until the replica catches up or the
    /// maximum number of attempts is reached.
    #[error("Replica was not yet up-to-date")]
    ReplicaLagging,

    /// The MySQL query response did not include an HLC timestamp.
    ///
    /// This error occurs when the database doesn't provide HLC information needed
    /// for consistency checks. Unlike `ReplicaLagging`, this error is not retried
    /// as it indicates a configuration or infrastructure issue rather than a
    /// temporary state.
    #[error("Response was missing HLC attribute")]
    MissingHLC,

    /// An underlying query error occurred during the consistent read operation.
    ///
    /// This wraps any other errors that can occur during query execution, such as
    /// network errors, SQL syntax errors, or database connection issues. These
    /// errors are not retried to avoid masking the underlying problem.
    #[error("Underlying error: {0}")]
    QueryError(#[from] anyhow::Error),
}

/// Helper function to determine if the `query_with_consistency` method should
/// be used for a given callsite and determine the appropriate parameters.
///
/// Will return `None` if the callsite should NOT use the `query_with_consistency` method.
pub fn consistent_read_options(
    client_correlator: Option<&str>,
    callsite: Option<&str>,
) -> Result<Option<ConsistentReadOptions>> {
    // Callsites that don't require the most recent bookmark value should
    // read from a replica. More context on D81212709.
    let should_query_with_consistency = justknobs::eval(
        "scm/mononoke:retry_query_from_replica_with_consistency_check",
        client_correlator,
        callsite,
    );

    if !should_query_with_consistency {
        return Ok(None);
    }

    let max_attempts = justknobs::get_as::<usize>(
        "scm/mononoke:retry_query_from_replica_with_consistency_check_max_attempts",
        callsite,
    );

    let interval = Duration::from_millis(justknobs::get_as::<u64>(
        "scm/mononoke:retry_query_from_replica_with_consistency_check_interval_ms",
        callsite,
    ));

    let jitter = Duration::from_millis(justknobs::get_as::<u64>(
        "scm/mononoke:retry_query_from_replica_with_consistency_check_jitter",
        callsite,
    ));

    let hlc_drift_tolerance_ns = justknobs::get_as::<i64>(
        "scm/mononoke:retry_query_from_replica_with_consistency_check_hlc_drift_tolerance_ns",
        callsite,
    );

    let cons_read_opts = ConsistentReadOptions {
        interval,
        max_attempts,
        jitter,
        hlc_drift_tolerance_ns,
        ..ConsistentReadOptions::default()
    };

    Ok(Some(cons_read_opts))
}
