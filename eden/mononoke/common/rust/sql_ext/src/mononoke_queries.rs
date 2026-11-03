/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use caching_ext::*;
use futures_retry::retry;
use itertools::Itertools;
use maplit::hashmap;
use maplit::hashset;
use memcache::KeyGen;
use mononoke_types::RepositoryId;
use mononoke_types::Timestamp;
use sql::QueryTelemetry;
use sql_query_config::CachingConfig;
use sql_query_telemetry::SqlQueryTelemetry;

use crate::ConsistentReadError;
use crate::ConsistentReadOptions;
use crate::TelemetryGranularity;
use crate::telemetry::log_consistent_read_query_error;
use crate::telemetry::log_query_error;

const RETRY_ATTEMPTS: usize = 2;

// This wraps around rust/shed/sql::queries, check that macro: https://fburl.com/code/semq9xm3
/// Define SQL queries that automatically retry on certain errors.
///
/// Caching can be enabled on a read query by:
/// - Adding "cacheable" keyword to your query.
/// - Make sure all parameters (input) to the query implement the Hash trait.
/// - Making sure the return values (output) implement Serialize, Deserialize, and
///   bincode::{Encode, Decode}.
///
/// Queries that return no rows are not cached to allow later retries to succeed.
#[macro_export]
macro_rules! mononoke_queries {
    () => {};

    // Read query with a single expression. Redirect to read query with same expression for mysql and sqlite.
    (
        $vi:vis read $name:ident (
            $( $pname:ident: $ptype:ty ),* $(,)*
            $( >list $lname:ident: $ltype:ty )*
        ) -> ($( $rtype:ty ),* $(,)*) { $q:expr }
        $( $rest:tt )*
    ) => {
        $crate::mononoke_queries! {
            $vi read $name (
                $( $pname: $ptype, )*
                $( >list $lname: $ltype )*
            ) -> ($( $rtype ),*) { mysql($q) sqlite($q) }
            $( $rest )*
        }
    };

    // >tuple_list read query with single expression. Redirect to full form.
    (
        $vi:vis read $name:ident (
            $( $pname:ident: $ptype:ty ),* $(,)*
            >tuple_list $tlname:ident: ($( $col:ident: $col_type:ty ),+)
        ) -> ($( $rtype:ty ),* $(,)*) { $q:expr }
        $( $rest:tt )*
    ) => {
        $crate::mononoke_queries! {
            $vi read $name (
                $( $pname: $ptype, )*
                >tuple_list $tlname: ($( $col: $col_type ),+)
            ) -> ($( $rtype ),*) { mysql($q) sqlite($q) }
            $( $rest )*
        }
    };

    // >tuple_list full read query. Bypasses sql::queries! to directly match
    // on Connection variants, enabling WHERE (col1, col2) IN ((v1, v2), ...)
    // queries that sql::queries! does not support.
    (
        $vi:vis read $name:ident (
            $( $pname:ident: $ptype:ty ),* $(,)*
            >tuple_list $tlname:ident: ($( $col:ident: $col_type:ty ),+)
        ) -> ($( $rtype:ty ),* $(,)*) { mysql($mysql_q:expr) sqlite($sqlite_q:expr) }
        $( $rest:tt )*
    ) => {
        #[allow(non_snake_case)]
        $vi mod $name {
            #[allow(unused_imports)]
            use super::*;

            #[allow(unused_imports)]
            use $crate::_macro_internal::*;

            #[allow(dead_code)]
            pub async fn query(
                connection: &Connection,
                sql_query_tel: SqlQueryTelemetry,
                $( $pname: &$ptype, )*
                $tlname: &[( $( $col_type, )+ )],
            ) -> Result<Vec<($( $rtype, )*)>> {
                if $tlname.is_empty() {
                    return Ok(Vec::new());
                }
                let res = _query_impl(
                    connection,
                    sql_query_tel,
                    $( $pname, )*
                    $tlname,
                )
                .await?;
                Ok(res.0)
            }

            async fn _query_impl(
                connection: &Connection,
                sql_query_tel: SqlQueryTelemetry,
                $( $pname: &$ptype, )*
                $tlname: &[( $( $col_type, )+ )],
            ) -> Result<(Vec<($( $rtype, )*)>, Option<QueryTelemetry>)> {
                let query_name = stringify!($name);
                let shard_name = connection.shard_name();
                let repo_ids = $crate::extract_repo_ids_from_queries!($($pname: $ptype; )*);

                let client_request_info = sql_query_tel.client_request_info()
                    .map(|cri| serde_json::to_string(cri)).transpose()?;

                let ((res, opt_tel, fut_stats), attempt) = query_with_retry_no_cache(
                    |_attempt| {
                        borrowed!(client_request_info);
                        async move {
                            let (fut_stats, (res, opt_tel)) = _execute_query(
                                connection.sql_connection(),
                                client_request_info.as_deref(),
                                $( $pname, )*
                                $tlname,
                            )
                            .try_timed()
                            .await?;
                            Ok((res, opt_tel, fut_stats))
                        }
                    },
                    shard_name,
                    query_name,
                    &sql_query_tel,
                    TelemetryGranularity::Query,
                    &repo_ids,
                ).await?;

                log_query_telemetry(
                    opt_tel.clone(),
                    &sql_query_tel,
                    TelemetryGranularity::Query,
                    &repo_ids,
                    query_name,
                    shard_name.as_ref(),
                    fut_stats,
                    Some(attempt),
                )?;

                Ok((res, opt_tel))
            }

            async fn _execute_query(
                connection: &_tl::InnerSqlConnection,
                comment: Option<&str>,
                $( $pname: &$ptype, )*
                $tlname: &[( $( $col_type, )+ )],
            ) -> Result<(Vec<($( $rtype, )*)>, Option<QueryTelemetry>)> {
                match connection {
                    _tl::InnerSqlConnection::Mysql(conn) => {
                        let mut query = _build_mysql_query($( $pname, )* $tlname)?;
                        if let Some(comment) = comment {
                            query.insert_str(0, &format!("/* {} */", comment));
                        }
                        let (res, tel) = conn.read_query(query).await
                            .map_err(_tl::anyhow::Error::from)?;

                        #[cfg(fbcode_build)]
                        { Ok((res, tel.map(_tl::InnerQueryTelemetry::MySQL))) }
                        #[cfg(not(fbcode_build))]
                        { Ok((res, tel)) }
                    }
                    _tl::InnerSqlConnection::OssMysql(conn) => {
                        let query = _build_mysql_query($( $pname, )* $tlname)?;
                        let mut con = _tl::OssConnection::get_conn_counted(
                            conn.pool.clone(), &conn.stats,
                        ).await?;
                        let (mut res, _tel) = conn.read_query(&mut con, &query).await
                            .map_err(_tl::anyhow::Error::from)?;
                        let result = res
                            .map(|row| _row_to_tuple(row))
                            .await?
                            .into_iter()
                            .collect::<std::result::Result<Vec<($( $rtype, )*)>, _tl::anyhow::Error>>()?;
                        Ok((result, None))
                    }
                    _tl::InnerSqlConnection::Sqlite(multithread_con) => {
                        let res = _sqlite_query(multithread_con, $( $pname, )* $tlname).await?;
                        let sqlite_tel = multithread_con
                            .hlc_ts_lower_bound()
                            .map(_tl::SqliteQueryTelemetry::new)
                            .map(_tl::InnerQueryTelemetry::Sqlite);
                        Ok((res, sqlite_tel))
                    }
                }
            }

            fn _build_mysql_query(
                $( $pname: &$ptype, )*
                $tlname: &[( $( $col_type, )+ )],
            ) -> std::result::Result<String, _tl::anyhow::Error> {
                use std::fmt::Write as _;
                use _tl::mysql_async::prelude::ToValue;

                // Pre-allocate: estimate ~30 bytes per tuple element.
                let mut tuple_str = String::with_capacity($tlname.len() * 30 + 2);
                write!(&mut tuple_str, "(")?;
                let mut _first_tuple = true;
                for ($( $col, )+) in $tlname {
                    if _first_tuple { _first_tuple = false; } else { write!(&mut tuple_str, ", ")?; }
                    write!(&mut tuple_str, "(")?;
                    let mut _first_col = true;
                    $(
                        if _first_col { _first_col = false; } else { write!(&mut tuple_str, ", ")?; }
                        write!(&mut tuple_str, "{}", ToValue::to_value($col).as_sql(false))?;
                    )+
                    write!(&mut tuple_str, ")")?;
                }
                write!(&mut tuple_str, ")")?;

                Ok(format!(
                    $mysql_q,
                    $( $pname = ToValue::to_value(&$pname).as_sql(false), )*
                    $tlname = tuple_str,
                ))
            }

            fn _row_to_tuple(row: _tl::mysql_async::Row) -> std::result::Result<($( $rtype, )*), _tl::anyhow::Error> {
                use _tl::mysql_async::prelude::FromValue;
                #[allow(clippy::mixed_read_write_in_expression)]
                let mut idx = 0;
                let res = (
                    $({
                        let res: _tl::mysql_async::Value = row.get(idx)
                            .ok_or_else(|| _tl::anyhow::anyhow!("Failed to get column at index {}", idx))?;
                        idx += 1;
                        <$rtype as FromValue>::from_value_opt(res)
                            .map_err(|err| _tl::anyhow::anyhow!(
                                "Failed to parse column {} as `{}`: {}", idx - 1, stringify!($rtype), err
                            ))?
                    },)*
                );
                let _ = idx;
                Ok(res)
            }

            async fn _sqlite_query(
                multithread_con: &_tl::SqliteMultithreaded,
                $( $pname: &$ptype, )*
                $tlname: &[( $( $col_type, )+ )],
            ) -> std::result::Result<Vec<($( $rtype, )*)>, _tl::anyhow::Error> {
                use std::fmt::Write as _;
                use _tl::mysql_async::prelude::ToValue;
                use _tl::mysql_async::prelude::FromValue;

                // Build named params for scalar params and tuple list elements.
                // Count: scalar params + (tuple_count * columns_per_tuple).
                let _num_cols = {
                    let mut _n = 0u32;
                    $( let _ = stringify!($col); _n += 1; )+
                    _n as usize
                };
                let mut params: Vec<(String, _tl::ValueWrapper)> =
                    Vec::with_capacity($tlname.len() * _num_cols);
                $(
                    params.push((
                        format!(":{}", stringify!($pname)),
                        _tl::ValueWrapper(ToValue::to_value($pname)),
                    ));
                )*
                for (i, ($( $col, )+)) in $tlname.iter().enumerate() {
                    $(
                        params.push((
                            format!(":{}_{}_{}",  stringify!($tlname), i, stringify!($col)),
                            _tl::ValueWrapper(ToValue::to_value($col)),
                        ));
                    )+
                }

                // Build the tuple list placeholder for SQLite.
                // SQLite supports row values: WHERE (a, b) IN (VALUES (:p0, :p1), (:p2, :p3))
                let mut tl_str = String::new();
                write!(&mut tl_str, "(VALUES ")?;
                for i in 0..$tlname.len() {
                    if i > 0 { write!(&mut tl_str, ", ")?; }
                    write!(&mut tl_str, "(")?;
                    let mut _first = true;
                    $(
                        if _first { _first = false; } else { write!(&mut tl_str, ", ")?; }
                        write!(&mut tl_str, ":{}_{}_{}",  stringify!($tlname), i, stringify!($col))?;
                    )+
                    write!(&mut tl_str, ")")?;
                }
                write!(&mut tl_str, ")")?;

                let query = format!(
                    $sqlite_q,
                    $( $pname = format!(":{}", stringify!($pname)), )*
                    $tlname = tl_str,
                );

                let con = multithread_con.acquire_sqlite_connection(
                    _tl::SqliteQueryType::Read,
                ).await?;

                let mut ref_params: Vec<(&str, &dyn _tl::rusqlite::types::ToSql)> = Vec::new();
                for idx in 0..params.len() {
                    ref_params.push((&params[idx].0, &params[idx].1));
                }

                let mut stmt = con.prepare(&query)?;
                let rows = stmt.query_map(
                    &ref_params[..],
                    |row| {
                        #[allow(clippy::mixed_read_write_in_expression)]
                        {
                            let mut idx = 0;
                            let res = (
                                $({
                                    let res: _tl::ValueWrapper = row.get(idx)?;
                                    idx += 1;
                                    <$rtype as FromValue>::from_value_opt(res.0)
                                        .map_err(|err| _tl::rusqlite::Error::FromSqlConversionFailure(
                                            idx - 1,
                                            _tl::rusqlite::types::Type::Blob,
                                            Box::new(err),
                                        ))?
                                },)*
                            );
                            let _ = idx;
                            Ok(res)
                        }
                    }
                )?.collect::<std::result::Result<Vec<_>, _>>()?;

                Ok(rows)
            }
        }

        $crate::mononoke_queries! { $( $rest )* }
    };

    // Read query with a single expression and cache. Redirect to read query with same expression for mysql and sqlite.
    (
        $vi:vis cacheable read $name:ident (
            $( $pname:ident: $ptype:ty ),* $(,)*
            $( >list $lname:ident: $ltype:ty )*
        ) -> ($( $rtype:ty ),* $(,)*) { $q:expr }
        $( $rest:tt )*
    ) => {
        $crate::mononoke_queries! {
            $vi cacheable read $name (
                $( $pname: $ptype, )*
                $( >list $lname: $ltype )*
            ) -> ($( $rtype ),*) { mysql($q) sqlite($q) }
            $( $rest )*
        }
    };

    // Full read query without cache. Call `sql::queries!` and re-export stuff, wrapped in retries, on a new module.
    (
        $vi:vis read $name:ident (
            $( $pname:ident: $ptype:ty ),* $(,)*
            $( >list $lname:ident: $ltype:ty )*
        ) -> ($( $rtype:ty ),* $(,)*) { mysql($mysql_q:expr) sqlite($sqlite_q:expr) }
        $( $rest:tt )*
    ) => {
        $crate::_macro_internal::paste::item! {
            $crate::_macro_internal::queries! {
                pub read [<$name Impl>] (
                    $( $pname: $ptype, )*
                    $( >list $lname: $ltype )*
                ) -> ($( $rtype ),*) { mysql($mysql_q) sqlite($sqlite_q) }
            }

            #[allow(non_snake_case)]
            $vi mod $name {
                #[allow(unused_imports)]
                use super::*;

                #[allow(unused_imports)]
                use $crate::_macro_internal::*;

                 #[allow(dead_code)]
                pub async fn query(
                    connection: &Connection,
                    sql_query_tel: SqlQueryTelemetry,
                    $( $pname: &$ptype, )*
                    $( $lname: &[ $ltype ], )*
                ) -> Result<Vec<($( $rtype, )*)>> {
                    let res = query_impl(
                        connection,
                        sql_query_tel,
                        TelemetryGranularity::Query,
                        $( $pname, )*
                        $( $lname, )*
                    )
                    .await?;

                    Ok(res.0)
                }
                #[allow(dead_code)]
                async fn query_impl(
                    connection: &Connection,
                    sql_query_tel: SqlQueryTelemetry,
                    granularity: TelemetryGranularity,
                    $( $pname: &$ptype, )*
                    $( $lname: &[ $ltype ], )*
                ) -> Result<(Vec<($( $rtype, )*)>, Option<QueryTelemetry>)> {
                    let query_name = stringify!($name);
                    let shard_name = connection.shard_name();
                    // Check if any parameter is a RepositoryId and pass it to telemetry
                    let repo_ids = $crate::extract_repo_ids_from_queries!($($pname: $ptype; )*);

                    let client_request_info = sql_query_tel.client_request_info()
                        .map(|cri| serde_json::to_string(cri)).transpose()?;

                    let ((res, opt_tel, fut_stats), attempt) = query_with_retry_no_cache(
                        |_attempt| {
                            borrowed!(client_request_info);
                            async move {
                                let (fut_stats, (res, opt_tel)) = [<$name Impl>]::commented_query(
                                    connection.sql_connection(),
                                    client_request_info.as_deref(),
                                    $( $pname, )*
                                    $( $lname, )*
                                )
                                .try_timed()
                                .await?;

                                Ok((res, opt_tel, fut_stats))
                            }
                        },
                        shard_name,
                        query_name,
                        &sql_query_tel,
                        granularity,
                        &repo_ids,
                    ).await?;

                    log_query_telemetry(
                        opt_tel.clone(),
                        &sql_query_tel,
                        granularity,
                        &repo_ids,
                        query_name,
                        shard_name.as_ref(),
                        fut_stats,
                        Some(attempt),
                    )?;

                    Ok((res, opt_tel))
                }

                #[allow(dead_code)]
                pub async fn query_with_transaction(
                    transaction: Transaction,
                    $( $pname: &$ptype, )*
                    $( $lname: &[ $ltype ], )*
                ) -> Result<(Transaction, Vec<($( $rtype, )*)>)> {
                    let query_name = stringify!($name);
                    let query_repo_ids = $crate::extract_repo_ids_from_queries!($($pname: $ptype; )*);
                    $crate::read_query_with_transaction!(
                        $name,
                        transaction,
                        query_repo_ids,
                        query_name,
                        ($( $pname: $ptype ),*),
                        ($( $lname: $ltype )*)
                    )
                }

                /// Read query that ensures the result is "consistent" by ensuring
                /// **at least** one of the following assumptions hold:
                ///
                /// 1. The result was served from a replica that was updated
                /// at a timestamp equal to or higher the provided `target_lower_bound_hlc`.
                ///
                /// 2. The result matches the expectation defined in the
                /// `return_early_if` callback.
                ///
                /// # Arguments
                ///
                /// * `target_lower_bound_hlc` - The minimum HLC (Hybrid Logical Clock) timestamp
                ///   that the replica must have for the result to be considered up-to-date. If `None`,
                ///   defaults to `Timestamp::now()`. Used to ensure read-after-write consistency by
                ///   verifying that the replica has caught up to at least this timestamp before
                ///   serving the query result.
                ///
                /// * `return_early_if` - Optional callback function that can bypass HLC consistency
                ///   checks by evaluating the query result directly. If provided and returns `true`
                ///   for the result, the function will return immediately without checking HLC
                ///   timestamps. This allows callers to define custom consistency conditions based
                ///   on the actual data returned (e.g. checking if a specific record exists).
                #[allow(dead_code)]
                pub async fn query_with_consistency<'a>(
                    connections: &SqlConnections,
                    sql_query_tel: SqlQueryTelemetry,
                    target_lower_bound_hlc: Option<Timestamp>,
                    return_early_if: Option<Arc<Box<dyn Fn(&Vec<($( $rtype, )*)>) -> bool + Send + Sync>>>,
                    cons_read_opts: ConsistentReadOptions,
                    $( $pname: &'a $ptype, )*
                    $( $lname: &'a [ $ltype ], )*
                ) -> Result<Vec<($( $rtype, )*)>> {
                    // TODO(T237287313): when we're able to manually set the
                    // `hlc_ts_lower_bound` attribute **in the query**, take a
                    // `SqlConnections` to use one with a DbLocator with
                    // `InstanceRequirement::ReadAfterWriteConsistency`.
                    // This means that the query will wait a specific time to
                    // allow the replica to catch up with the master.
                    let connection = &connections.read_connection;
                    let granularity = TelemetryGranularity::ConsistentReadQuery;

                    let query_name = stringify!($name);
                    let shard_name = connection.shard_name();
                    // Check if any parameter is a RepositoryId and pass it to telemetry
                    let repo_ids = $crate::extract_repo_ids_from_queries!($($pname: $ptype; )*);

                    let (fut_stats, (final_res, opt_tel, attempt)) = {
                        cloned!(sql_query_tel);
                        async {
                            let (res, cons_read_attempt) = query_with_consistency_no_cache(
                                |_attempt| {

                                    cloned!(sql_query_tel);
                                    async move {
                                        query_impl(
                                            connection,
                                            sql_query_tel,
                                            granularity,
                                            $( $pname, )*
                                            $( $lname, )*
                                        ).await
                                    }
                                },
                                target_lower_bound_hlc,
                                return_early_if,
                                cons_read_opts,
                                shard_name,
                                query_name,
                                &sql_query_tel,
                                granularity,
                                &repo_ids,
                            ).await;

                            if let Err(ConsistentReadError::MissingHLC) = res {
                                // If the query failed because the HLC was missing,
                                // fallback to the primary connection
                                let (res, opt_tel) = query_impl(
                                    &connections.read_master_connection,
                                    sql_query_tel,
                                    granularity,
                                    $( $pname, )*
                                    $( $lname, )*
                                ).await?;
                                return anyhow::Ok((res, opt_tel, cons_read_attempt + 1));
                            };

                            let (res, opt_tel) = res?;
                            anyhow::Ok((res, opt_tel, cons_read_attempt))
                        }
                    }
                    .try_timed()
                    .await?;

                    log_query_telemetry(
                        opt_tel,
                        &sql_query_tel,
                        TelemetryGranularity::ConsistentRead,
                        &repo_ids,
                        query_name,
                        shard_name.as_ref(),
                        fut_stats,
                        Some(attempt),
                    )?;

                    Ok(final_res)

                }

            }

            $crate::mononoke_queries! { $( $rest )* }
        }
    };

    // Full read query. Call `sql::queries!` and re-export stuff, wrapped in retries, on a new module.
    (
        $vi:vis cacheable read $name:ident (
            $( $pname:ident: $ptype:ty ),* $(,)*
            $( >list $lname:ident: $ltype:ty )*
        ) -> ($( $rtype:ty ),* $(,)*) { mysql($mysql_q:expr) sqlite($sqlite_q:expr) }
        $( $rest:tt )*
    ) => {
        $crate::_macro_internal::paste::item! {
            $crate::_macro_internal::queries! {
                pub read [<$name Impl>] (
                    $( $pname: $ptype, )*
                    $( >list $lname: $ltype )*
                ) -> ($( $rtype ),*) { mysql($mysql_q) sqlite($sqlite_q) }
            }

            #[allow(non_snake_case)]
            $vi mod $name {
                #[allow(unused_imports)]
                use super::*;

                #[allow(unused_imports)]
                use $crate::_macro_internal::*;

                #[allow(dead_code)]
                pub async fn query(
                    config: &SqlQueryConfig,
                    cache_ttl: Option<std::time::Duration>,
                    connection: &Connection,
                    sql_query_tel: SqlQueryTelemetry,
                    $( $pname: &$ptype, )*
                    $( $lname: &[ $ltype ], )*
                ) -> Result<Vec<($( $rtype, )*)>> {
                    // Prepare cache data
                    let mut hasher = XxHash3_128::with_seed(0);
                    $(
                        $pname.hash(&mut hasher);
                    )*
                    $(
                        $lname.hash(&mut hasher);
                    )*
                    stringify!($name).hash(&mut hasher);
                    stringify!($mysql_q).hash(&mut hasher);
                    stringify!($sqlite_q).hash(&mut hasher);
                    let key = hasher.finish_128();
                    let data = CacheData {key, config: config.caching.as_ref(), cache_ttl };

                    let query_name = stringify!($name);
                    let shard_name = connection.shard_name();
                    let granularity = TelemetryGranularity::Query;

                    // Check if any parameter is a RepositoryId and pass it to telemetry
                    let repo_ids = $crate::extract_repo_ids_from_queries!($($pname: $ptype; )*);

                    // Execute query with caching
                    // Note: For cache hits, no DB-level telemetry is logged since the query doesn't run.
                    // Telemetry is logged inside the closure only when there's a cache miss.
                    let (cached_res, _attempt) = query_with_retry(
                        data,
                        |_attempt| {
                            borrowed!(sql_query_tel);
                            cloned!(repo_ids);
                            let cri = sql_query_tel.client_request_info();
                            async move {
                                // Convert ClientRequestInfo to string if present
                                let cri_str = cri.map(|cri| serde_json::to_string(&cri)).transpose()?;


                                let (fut_stats, (res, opt_tel)) = [<$name Impl>]::commented_query(
                                    connection.sql_connection(),
                                    cri_str.as_deref(),
                                    $( $pname, )*
                                    $( $lname, )*

                                )
                                .try_timed()
                                .await?;

                                log_query_telemetry(
                                    opt_tel,
                                    &sql_query_tel,
                                    granularity,
                                    &repo_ids,
                                    query_name,
                                    shard_name.as_ref(),
                                    fut_stats,
                                    None,
                                )?;

                                Ok(CachedQueryResult(res))
                            }
                        },
                        shard_name,
                        query_name,
                        &sql_query_tel,
                        granularity,
                        &repo_ids,
                    ).await?;

                    Ok(cached_res.0)
                }

                #[allow(dead_code)]
                pub async fn query_with_transaction(
                    transaction: Transaction,
                    $( $pname: &$ptype, )*
                    $( $lname: &[ $ltype ], )*
                ) -> Result<(Transaction, Vec<($( $rtype, )*)>)> {
                    let query_name = stringify!($name);
                    let query_repo_ids = $crate::extract_repo_ids_from_queries!($($pname: $ptype; )*);
                    $crate::read_query_with_transaction!(
                        $name,
                        transaction,
                        query_repo_ids,
                        query_name,
                        ($( $pname: $ptype ),*),
                        ($( $lname: $ltype )*)
                    )
                }

            }

            $crate::mononoke_queries! { $( $rest )* }
        }
    };

    // Write query with a single expression. Redirect to write query with same expression for mysql and sqlite.
    (
        $vi:vis write $name:ident (
            values: ($( $vname:ident: $vtype:ty ),* $(,)*)
            $( , $pname:ident: $ptype:ty )* $(,)*
        ) { $qtype:ident, $q:expr }
        $( $rest:tt )*
    ) => {
        $crate::mononoke_queries! {
            $vi write $name (
                values: ( $( $vname: $vtype ),* )
                $( , $pname: $ptype )*
            ) { $qtype, mysql($q) sqlite($q) }
            $( $rest )*
        }
    };

    // Full write query with a list of values. Call `sql::queries!` and re-export stuff, wrapped in retries, on a new module.
    (
        $vi:vis write $name:ident (
            values: ($( $vname:ident: $vtype:ty ),* $(,)*)
            $( , $pname:ident: $ptype:ty )* $(,)*
        ) { $qtype:ident, mysql($mysql_q:expr) sqlite($sqlite_q:expr) }
        $( $rest:tt )*
    ) => {
        $crate::_macro_internal::paste::item! {
            $crate::_macro_internal::queries! {
                pub write [<$name Impl>] (
                    values: ( $( $vname: $vtype ),* )
                    $( , $pname: $ptype )*
                ) { $qtype, mysql($mysql_q) sqlite($sqlite_q) }
            }

            #[allow(non_snake_case)]
            $vi mod $name {
                #[allow(unused_imports)]
                use super::*;

                #[allow(unused_imports)]
                use $crate::_macro_internal::*;

                #[allow(dead_code)]
                pub async fn query(
                    connection: &Connection,
                    sql_query_tel: SqlQueryTelemetry,
                    values: &[($( & $vtype, )*)],
                    $( $pname: &$ptype ),*
                ) -> Result<WriteResult> {
                    let query_name = stringify!($name);
                    let cri = sql_query_tel.client_request_info();
                    // Convert ClientRequestInfo to string if present
                    let cri_str = cri.map(|cri| serde_json::to_string(&cri)).transpose()?;
                    let shard_name = connection.shard_name();

                    let granularity = TelemetryGranularity::Query;

                    // Extract repo IDs from values parameter
                    let values_repo_ids: Vec<RepositoryId> =
                        $crate::_macro_internal::extract_repo_ids_from_values!(($($vtype,)*));
                    // Check if any parameter is a RepositoryId and pass it to telemetry
                    let repo_ids: Vec<RepositoryId> =
                        $crate::extract_repo_ids_from_queries!($($pname: $ptype; )*)
                        .into_iter()
                        .chain(values_repo_ids)
                        .collect();


                    let (fut_stats, (write_res, attempt)) = query_with_retry_no_cache(
                        |_attempt| [<$name Impl>]::commented_query(
                            connection.sql_connection(),
                            cri_str.as_deref(),
                            values
                            $( , $pname )*
                        ),
                        shard_name,
                        query_name,
                        &sql_query_tel,
                        granularity,
                        &repo_ids,
                    )
                    .try_timed()
                    .await?;

                    let opt_tel = write_res.query_telemetry().clone();

                    log_query_telemetry(
                        opt_tel,
                        &sql_query_tel,
                        granularity,
                        &repo_ids,
                        &query_name,
                        shard_name.as_ref(),
                        fut_stats,
                        Some(attempt),
                    )?;

                    Ok(write_res)

                }

                #[allow(dead_code)]
                pub async fn query_with_transaction(
                    transaction: Transaction,
                    values: &[($( & $vtype, )*)],
                    $( $pname: & $ptype ),*
                ) -> Result<(Transaction, WriteResult)> {
                    let query_name = stringify!($name);

                    let Transaction {
                        inner: sql_txn,
                        txn_telemetry,
                        sql_query_tel,
                        shard_name,
                    } = transaction;

                    let cri = sql_query_tel.client_request_info();
                    // Convert ClientRequestInfo to string if present
                    let cri_str = cri.map(|cri| serde_json::to_string(&cri)).transpose()?;

                    let granularity = TelemetryGranularity::TransactionQuery;

                    // Extract repo IDs from values parameter
                    let values_repo_ids: Vec<RepositoryId> =
                        $crate::_macro_internal::extract_repo_ids_from_values!(($($vtype,)*));
                    // Check if any parameter is a RepositoryId and pass it to telemetry
                    let query_repo_ids: Vec<RepositoryId> =
                        $crate::extract_repo_ids_from_queries!($($pname: $ptype; )*)
                        .into_iter()
                        .chain(values_repo_ids)
                        .collect();


                    let (fut_stats, (sql_txn, write_res)) = [<$name Impl>]::commented_query_with_transaction(
                        sql_txn,
                        cri_str.as_deref(),
                        values
                        $( , $pname )*
                    )
                    .try_timed()
                    .await
                    .inspect_err(|e| {
                        log_query_error(
                            &sql_query_tel,
                            &e,
                            granularity,
                            &query_repo_ids,
                            &query_name,
                            shard_name.as_ref(),
                            1, // attempt number
                            false,
                        )
                    })?;

                    let opt_tel = write_res.query_telemetry().clone();

                   let txn = Transaction::from_transaction_query_result(
                        sql_txn,
                        opt_tel,
                        txn_telemetry,
                        sql_query_tel,
                        query_repo_ids,
                        granularity,
                        query_name,
                        shard_name,
                        fut_stats,
                        1, // attempt number
                    )?;

                    Ok((txn, write_res))

                }
            }

            $crate::mononoke_queries! { $( $rest )* }
        }
    };

    // Write query with a single expression. Redirect to write query with same expression for mysql and sqlite.
    (
        $vi:vis write $name:ident (
            $( $pname:ident: $ptype:ty ),* $(,)*
            $( >list $lname:ident: $ltype:ty )*
        ) { $qtype:ident, $q:expr }
        $( $rest:tt )*
    ) => {
        $crate::mononoke_queries! {
            $vi write $name (
                $( $pname: $ptype, )*
                $( >list $lname: $ltype )*
            ) { $qtype, mysql($q) sqlite($q) }
            $( $rest )*
        }
    };

    // Full write query without a list of values. Call `sql::queries!` and re-export stuff, wrapped in retries, on a new module.
    (
        $vi:vis write $name:ident (
            $( $pname:ident: $ptype:ty ),* $(,)*
            $( >list $lname:ident: $ltype:ty )*
        ) { $qtype:ident, mysql($mysql_q:expr) sqlite($sqlite_q:expr) }
        $( $rest:tt )*
    ) => {
        $crate::_macro_internal::paste::item! {
            $crate::_macro_internal::queries! {
                pub write [<$name Impl>] (
                    $( $pname: $ptype, )*
                    $( >list $lname: $ltype )*
                ) { $qtype, mysql($mysql_q) sqlite($sqlite_q) }
            }

            #[allow(non_snake_case)]
            $vi mod $name {
                #[allow(unused_imports)]
                use super::*;

                #[allow(unused_imports)]
                use $crate::_macro_internal::*;

                #[allow(dead_code)]
                pub async fn query(
                    connection: &Connection,
                    sql_query_tel: SqlQueryTelemetry,
                    $( $pname: &$ptype, )*
                    $( $lname: &[ $ltype ], )*
                ) -> Result<WriteResult> {
                    let query_name = stringify!($name);
                    let cri = sql_query_tel.client_request_info();
                    // Convert ClientRequestInfo to string if present
                    let cri_str = cri.map(|cri| serde_json::to_string(&cri)).transpose()?;
                    let shard_name = connection.shard_name();

                    let granularity = TelemetryGranularity::Query;

                    // Check if any parameter is a RepositoryId and pass it to telemetry
                    let repo_ids = $crate::extract_repo_ids_from_queries!($($pname: $ptype; )*);

                    let (fut_stats, (write_res, attempt)) = query_with_retry_no_cache(
                        |_attempt| [<$name Impl>]::commented_query(
                            connection.sql_connection(),
                            cri_str.as_deref(),
                            $( $pname, )*
                            $( $lname, )*
                        ),
                        shard_name,
                        query_name,
                        &sql_query_tel,
                        granularity,
                        &repo_ids,
                    )
                    .try_timed()
                    .await?;
                    let opt_tel = write_res.query_telemetry().clone();

                    log_query_telemetry(
                        opt_tel,
                        &sql_query_tel,
                        granularity,
                        &repo_ids,
                        &query_name,
                        shard_name.as_ref(),
                        fut_stats,
                        Some(attempt),
                    )?;

                    Ok(write_res)
                }

                // TODO(T223577767): extract duplication from
                // `query_with_transaction` from write queries with values
                #[allow(dead_code)]
                pub async fn query_with_transaction(
                    transaction: Transaction,
                    $( $pname: &$ptype, )*
                    $( $lname: &[ $ltype ], )*
                ) -> Result<(Transaction, WriteResult)> {
                    let query_name = stringify!($name);

                    let Transaction {
                        inner: sql_txn,
                        txn_telemetry,
                        sql_query_tel,
                        shard_name,
                    } = transaction;

                    let cri = sql_query_tel.client_request_info();
                    // Convert ClientRequestInfo to string if present
                    let cri_str = cri.map(|cri| serde_json::to_string(&cri)).transpose()?;

                    let granularity = TelemetryGranularity::TransactionQuery;

                    // Check if any parameter is a RepositoryId and pass it to telemetry
                    let query_repo_ids = $crate::extract_repo_ids_from_queries!($($pname: $ptype; )*);


                    let (fut_stats, (sql_txn, write_res)) =
                        [<$name Impl>]::commented_query_with_transaction(
                            sql_txn,
                            cri_str.as_deref()
                            $( , $pname )*
                            $( , $lname )*
                        )
                        .try_timed()
                        .await
                        .inspect_err(|e| {
                        log_query_error(
                            &sql_query_tel,
                            &e,
                            granularity,
                            &query_repo_ids,
                            &query_name,
                            shard_name.as_ref(),
                            1, // attempt number
                            false,
                        )
                    })?;

                    let opt_tel = write_res.query_telemetry().clone();

                    let txn = Transaction::from_transaction_query_result(
                        sql_txn,
                        opt_tel,
                        txn_telemetry,
                        sql_query_tel,
                        query_repo_ids,
                        granularity,
                        &query_name,
                        shard_name,
                        fut_stats,
                        1, // attempt number
                    )?;

                    Ok((txn, write_res))
                }
            }

            $crate::mononoke_queries! { $( $rest )* }
        }
    };

}

// Helper macro to generate the body of query_with_transaction for read queries
#[macro_export]
macro_rules! read_query_with_transaction {
    (
        $name:ident,
        $transaction:ident,
        $query_repo_ids:ident,
        $query_name:ident,
        ($( $pname:ident: $ptype:ty ),*),
        ($( $lname:ident: $ltype:ty )*)
    ) => {{
        let granularity = TelemetryGranularity::TransactionQuery;

       let Transaction {
            inner: sql_txn,
            txn_telemetry,
            sql_query_tel,
            shard_name,
        } = $transaction;

        let cri = sql_query_tel.client_request_info();
        let cri_str = cri.map(|cri| serde_json::to_string(&cri)).transpose()?;

        let (fut_stats, (sql_txn, (res, opt_tel))) = paste::expr! {
            [<$name Impl>]::commented_query_with_transaction(
                sql_txn,
                cri_str.as_deref(),
                $( $pname, )*
                $( $lname, )*
            )
        }
        .try_timed()
        .await
        .inspect_err(|e| {
            log_query_error(
                &sql_query_tel,
                &e,
                granularity,
                &$query_repo_ids,
                &$query_name,
                shard_name.as_ref(),
                1, // attempt number
                false,
            )
        })?;

        let txn = Transaction::from_transaction_query_result(
            sql_txn,
            opt_tel,
            txn_telemetry,
            sql_query_tel,
            $query_repo_ids,
            granularity,
            $query_name,
            shard_name,
            fut_stats,
            1, // attempt number
        )?;

        Ok((txn, res))
    }};
}

// Helper macro to extract RepositoryId from query parameters
#[macro_export]
macro_rules! extract_repo_ids_from_queries {
    // Base case: no parameters
    () => {{
        Vec::<RepositoryId>::new()
    }};

    // Match RepositoryId with additional parameters
    ($pname:ident: RepositoryId; $($rest:tt)*) => {{
        vec![*$pname]
            .into_iter()
            .chain(
                $crate::extract_repo_ids_from_queries!($($rest)*)
            )
            .collect::<Vec<_>>()
    }};

    // Skip non-RepositoryId parameter and continue with the rest
    ($pname:ident: $ptype:ty; $($rest:tt)*) => {
        $crate::extract_repo_ids_from_queries!($($rest)*)
    };
}

#[cfg(fbcode_build)]
/// See https://fburl.com/sv/uk8w71td for error descriptions
fn retryable_mysql_errno(errno: u32) -> bool {
    match errno {
        // Deadlock error, advice is restarting transaction, so it's retryable
        1213 => true,
        // Admission control errors
        // Safe to retry on writes as well as the query didn't even start
        1914..=1916 => true,
        _ => false,
    }
}

#[cfg(fbcode_build)]
/// Classifies the errors returned by MySQL as retryable or not
/// useful for retry logic that has to cover greater span than single query
/// like retrying the whole transaction.
pub fn should_retry_mysql_query(err: &anyhow::Error) -> bool {
    use MysqlError::*;
    use mysql_client::MysqlError;
    match err.downcast_ref::<MysqlError>() {
        Some(ConnectionOperationError { mysql_errno, .. })
        | Some(QueryResultError { mysql_errno, .. }) => retryable_mysql_errno(*mysql_errno),
        _ => false,
    }
}

#[cfg(not(fbcode_build))]
pub fn should_retry_mysql_query(_err: &anyhow::Error) -> bool {
    false
}

type Key = u128;

pub struct CacheData<'a> {
    pub key: Key,
    pub config: Option<&'a CachingConfig>,
    pub cache_ttl: Option<Duration>,
}

struct QueryCacheStore<'a, F, T> {
    key: Key,
    cache_config: &'a CachingConfig,
    cachelib: CachelibHandler<CachedQueryResult<Vec<T>>>,
    memcache: MemcacheHandler,
    fetcher: F,
    cache_ttl: Option<Duration>,
}

impl<F, T> EntityStore<CachedQueryResult<Vec<T>>> for QueryCacheStore<'_, F, T> {
    fn cachelib(&self) -> &CachelibHandler<CachedQueryResult<Vec<T>>> {
        &self.cachelib
    }

    fn keygen(&self) -> &KeyGen {
        &self.cache_config.keygen
    }

    fn memcache(&self) -> &MemcacheHandler {
        &self.memcache
    }

    fn cache_determinator(&self, v: &CachedQueryResult<Vec<T>>) -> CacheDisposition {
        if v.0.is_empty() {
            CacheDisposition::Ignore
        } else {
            CacheDisposition::Cache(self.cache_ttl.map_or(CacheTtl::NoTtl, CacheTtl::Ttl))
        }
    }

    caching_ext::impl_singleton_stats!("sql");
}

#[async_trait]
impl<T, F, Fut> KeyedEntityStore<Key, CachedQueryResult<Vec<T>>> for QueryCacheStore<'_, F, T>
where
    T: Send + 'static,
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = Result<CachedQueryResult<Vec<T>>>> + Send,
{
    fn get_cache_key(&self, key: &Key) -> String {
        // We just need a unique representation of the key as a String.
        // Let's use base64 as it's smaller than just .to_string()
        base64::engine::general_purpose::STANDARD.encode(key.to_ne_bytes())
    }

    async fn get_from_db(
        &self,
        keys: HashSet<Key>,
    ) -> Result<HashMap<Key, CachedQueryResult<Vec<T>>>> {
        let key = keys.into_iter().exactly_one()?;
        anyhow::ensure!(key == self.key, "Fetched invalid key {key}");
        let val = (self.fetcher)().await?;
        Ok(hashmap! { key => val })
    }
}

#[derive(Clone)]
#[derive(bincode::Encode, bincode::Decode)]
pub struct CachedQueryResult<T>(pub T);

impl<T> MemcacheEntity for CachedQueryResult<Vec<T>>
where
    T: serde::Serialize + for<'a> serde::Deserialize<'a>,
{
    fn serialize(&self) -> Bytes {
        serde_cbor::to_vec(&self.0)
            .expect("Should serialize cleanly")
            .into()
    }

    fn deserialize(bytes: Bytes) -> McResult<Self> {
        match serde_cbor::from_slice(bytes.as_ref()) {
            Ok(ok) => Ok(Self(ok)),
            Err(_) => Err(McErrorKind::Deserialization),
        }
    }
}

pub async fn query_with_retry_no_cache<T, Fut>(
    do_query: impl Fn(usize) -> Fut + Send + Sync,
    shard_name: &str,
    query_name: &str,
    sql_query_tel: &SqlQueryTelemetry,
    granularity: TelemetryGranularity,
    repo_ids: &[RepositoryId],
) -> Result<(T, usize)>
where
    T: Send + 'static,
    Fut: Future<Output = Result<T>>,
{
    if justknobs::eval("scm/mononoke:sql_disable_auto_retries", None, None) {
        return Ok((do_query(0).await?, 0));
    }
    let (res, attempt) = retry(do_query, Duration::from_secs(10))
        .exponential_backoff(1.2)
        .jitter(Duration::from_secs(5))
        .max_attempts(RETRY_ATTEMPTS)
        .retry_if(|_attempt, e| should_retry_mysql_query(e))
        .inspect_err(|attempt, e| {
            log_query_error(
                sql_query_tel,
                e,
                granularity,
                repo_ids,
                query_name,
                shard_name,
                attempt,
                attempt < RETRY_ATTEMPTS && should_retry_mysql_query(e),
            )
        })
        .await?;
    Ok((res, attempt))
}

pub async fn query_with_retry<T, Fut>(
    cache_data: CacheData<'_>,
    do_query: impl Fn(usize) -> Fut + Send + Sync,
    shard_name: &str,
    query_name: &str,
    sql_query_tel: &SqlQueryTelemetry,
    granularity: TelemetryGranularity,
    repo_ids: &[RepositoryId],
) -> Result<(CachedQueryResult<Vec<T>>, usize)>
where
    T: Send + bincode::Encode + bincode::Decode<()> + Clone + 'static,
    CachedQueryResult<Vec<T>>: MemcacheEntity,
    Fut: Future<Output = Result<CachedQueryResult<Vec<T>>>> + Send,
{
    if justknobs::eval("scm/mononoke:sql_disable_auto_cache", None, None) {
        return query_with_retry_no_cache(
            &do_query,
            shard_name,
            query_name,
            sql_query_tel,
            granularity,
            repo_ids,
        )
        .await;
    }
    let key = cache_data.key;
    if let Some(config) = cache_data.config.as_ref() {
        let fetch = || async {
            let (result, _attempt) = query_with_retry_no_cache(
                &do_query,
                shard_name,
                query_name,
                sql_query_tel,
                granularity,
                repo_ids,
            )
            .await?;
            Ok(result)
        };
        let store = QueryCacheStore {
            key: cache_data.key,
            cachelib: config.cache_handler_factory.cachelib(),
            memcache: config.cache_handler_factory.memcache(),
            cache_config: config,
            fetcher: fetch,
            cache_ttl: cache_data.cache_ttl,
        };
        let res = get_or_fill(&store, hashset! {key})
            .await?
            .into_iter()
            .exactly_one()
            .map_err(|_| anyhow!("Multiple values for a single key"))?
            .1;
        // When result came from cache or fetched through cache infrastructure,
        // we report attempt as 1 since the cache layer doesn't track retry attempts
        Ok((res, 1))
    } else {
        query_with_retry_no_cache(
            &do_query,
            shard_name,
            query_name,
            sql_query_tel,
            granularity,
            repo_ids,
        )
        .await
    }
}

/// Use the HLC from Read Your Own Writes feature (<https://fburl.com/wiki/bvaobxgp>)
/// to determine if the replica was up to date when it served the query.
pub async fn query_with_consistency_no_cache<T, Fut>(
    do_query: impl Fn(usize) -> Fut + Send + Sync,
    target_lower_bound_hlc: Option<Timestamp>,
    return_early_if: Option<Arc<Box<dyn Fn(&T) -> bool + Send + Sync>>>,
    cons_read_opts: ConsistentReadOptions,
    shard_name: &str,
    query_name: &str,
    sql_query_tel: &SqlQueryTelemetry,
    granularity: TelemetryGranularity,
    repo_ids: &[RepositoryId],
) -> (
    Result<(T, Option<QueryTelemetry>), ConsistentReadError>,
    usize,
)
where
    T: Send + 'static,
    Fut: Future<Output = Result<(T, Option<QueryTelemetry>)>>,
{
    // The minimum HLC that the replica must have for the result to be considered
    // up to date.
    let target_lower_bound_hlc = target_lower_bound_hlc.unwrap_or(Timestamp::now());

    let hlc_drift_tolerance_ns = cons_read_opts.hlc_drift_tolerance_ns;

    // Wrap in Arc so it can be cloned into the retry closure
    let do_query = Arc::new(do_query);

    let last_attempt = std::sync::atomic::AtomicUsize::new(0);

    let retry_result = retry(
        |attempt| {
            last_attempt.store(attempt, std::sync::atomic::Ordering::Relaxed);
            let do_query = Arc::clone(&do_query);
            let return_early_if = return_early_if.clone();
            async move {
                let (res, opt_tel) = do_query(attempt).await?;

                if let Some(ref early_check) = return_early_if {
                    if early_check(&res) {
                        return Ok((res, opt_tel));
                    };
                };
                let response_hlc = match opt_tel {
                    #[cfg(fbcode_build)]
                    Some(QueryTelemetry::MySQL(ref mysql_tel)) => mysql_tel
                        .hlc_ts_lower_bound
                        .ok_or(ConsistentReadError::MissingHLC),
                    Some(QueryTelemetry::Sqlite(ref sqlite_tel)) => {
                        Ok(sqlite_tel.hlc_ts_lower_bound)
                    }
                    // HLC is needed to use query_with_consistency, otherwise the
                    // result can't be trusted be up-to-date.
                    _ => Err(ConsistentReadError::MissingHLC),
                }?;

                if replica_was_up_to_date(
                    target_lower_bound_hlc,
                    response_hlc,
                    hlc_drift_tolerance_ns,
                )? {
                    Ok((res, opt_tel))
                } else {
                    Err(ConsistentReadError::ReplicaLagging)
                }
            }
        },
        cons_read_opts.interval,
    )
    .exponential_backoff(cons_read_opts.exp_backoff_base)
    .jitter(cons_read_opts.jitter)
    .max_attempts(cons_read_opts.max_attempts)
    .retry_if(|_attempt, err| {
        match err {
            ConsistentReadError::ReplicaLagging => true,
            // In this case we don't want to retry, but we'll want to log something
            ConsistentReadError::MissingHLC => false,
            // Don't retry on actual errors to avoid masking them.
            // The retries are intended for
            ConsistentReadError::QueryError(_e) => false,
        }
    })
    .inspect_err(|attempt, cons_read_err| {
        log_consistent_read_query_error(
            sql_query_tel,
            cons_read_err,
            granularity,
            repo_ids,
            query_name,
            shard_name,
            attempt,
            attempt < cons_read_opts.max_attempts,
        );
    })
    .await;

    let attempt = last_attempt.load(std::sync::atomic::Ordering::Relaxed);

    match retry_result {
        Ok((result, _)) => (Ok(result), attempt),
        Err(e) => (Err(e), attempt),
    }
}

fn replica_was_up_to_date(
    target_lower_bound_hlc: Timestamp,
    hlc: i64,
    hlc_drift_tolerance_ns: i64,
) -> Result<bool> {
    let hlc_time = Timestamp::from_timestamp_nanos(hlc + hlc_drift_tolerance_ns);

    Ok(hlc_time >= target_lower_bound_hlc)
}
