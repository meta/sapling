/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! A scrub handler that reports each copy a store lacks to ODS and Scuba.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use anyhow::Error;
use blobstore::BlobstoreMetadata;
use context::CoreContext;
use metaconfig_types::BlobstoreId;
use mononoke_types::RepositoryId;
use mononoke_types::repo::REPO_PREFIX_REGEX;
use multiplexedblob::LoggingScrubHandler;
use multiplexedblob::ScrubHandler;
use scuba::value::NullScubaValue;
use scuba::value::ScubaValue;
use scuba_ext::MononokeScubaSampleBuilder;
use stats::prelude::*;

define_stats! {
    prefix = "mononoke";
    scrub_repaired: dynamic_timeseries("{}.{}.blobstore.{}.{}.repaired", (tool: &'static str, subcommand: &'static str, blobstore_id: String, repo: String); Rate, Sum),
    scrub_repair_required: dynamic_timeseries("{}.{}.blobstore.{}.{}.repair_required", (tool: &'static str, subcommand: &'static str, blobstore_id: String, repo: String); Rate, Sum),
}

const BLOBSTORE_ID: &str = "blobstore_id";
const CHECK_FAIL: &str = "check_fail";
const CHECK_TYPE: &str = "check_type";
const CTIME: &str = "ctime";
const ERROR_MSG: &str = "error_msg";
const NODE_KEY: &str = "node_key";
const REPO: &str = "repo";

/// Names a tool's scrub counters,
/// `mononoke.<tool>.<subcommand>.blobstore.<store>.<repo>.repair_required`
/// and `.repaired`, which detectors alarm on.
#[derive(Clone, Copy, Debug)]
pub struct ScrubStatsKey {
    pub tool: &'static str,
    pub subcommand: &'static str,
}

impl ScrubStatsKey {
    fn args(
        &self,
        blobstore_id: BlobstoreId,
        repo_name: &str,
    ) -> (&'static str, &'static str, String, String) {
        (
            self.tool,
            self.subcommand,
            blobstore_id.to_string(),
            repo_name.to_string(),
        )
    }
}

/// Sets both of a repo's counters to zero for each store, so they report a
/// zero before any copy goes missing, rather than no data.
pub fn zero_scrub_stats(
    stats_key: ScrubStatsKey,
    repo_name: &str,
    blobstore_ids: impl IntoIterator<Item = BlobstoreId>,
) {
    for blobstore_id in blobstore_ids {
        for stat in [&STATS::scrub_repaired, &STATS::scrub_repair_required] {
            stat.add_value(0, stats_key.args(blobstore_id, repo_name));
        }
    }
}

/// Logs each copy a store lacks, counts it per repo and store, and writes a
/// Scuba row with its key and store.
pub struct StatsScrubHandler {
    scuba: MononokeScubaSampleBuilder,
    stats_key: ScrubStatsKey,
    repo_id_to_name: HashMap<RepositoryId, String>,
    inner: LoggingScrubHandler,
}

impl StatsScrubHandler {
    pub fn new(
        quiet: bool,
        scuba: MononokeScubaSampleBuilder,
        stats_key: ScrubStatsKey,
        repo_id_to_name: HashMap<RepositoryId, String>,
    ) -> Self {
        Self {
            scuba,
            stats_key,
            inner: LoggingScrubHandler::new(quiet),
            repo_id_to_name,
        }
    }
}

impl fmt::Debug for StatsScrubHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatsScrubHandler")
            .field("stats_key", &self.stats_key)
            .field("inner", &self.inner)
            .finish()
    }
}

fn get_repo_id_from_key(key: &str) -> Result<Option<RepositoryId>, Error> {
    REPO_PREFIX_REGEX
        .captures(key)
        .and_then(|m| m.get(1).map(|m| RepositoryId::from_str(m.as_str())))
        .transpose()
}

impl ScrubHandler for StatsScrubHandler {
    fn on_repair(
        &self,
        ctx: &CoreContext,
        blobstore_id: BlobstoreId,
        key: &str,
        is_repaired: bool,
        meta: &BlobstoreMetadata,
    ) {
        self.inner
            .on_repair(ctx, blobstore_id, key, is_repaired, meta);

        let ctime = match meta.ctime() {
            Some(ctime) => ScubaValue::from(ctime),
            None => ScubaValue::Null(NullScubaValue::Int),
        };

        let mut scuba = self.scuba.clone();

        let repo_id = get_repo_id_from_key(key);
        match repo_id {
            Ok(Some(repo_id)) => {
                if let Some(repo_name) = self.repo_id_to_name.get(&repo_id) {
                    scuba.add(REPO, repo_name.clone());
                    let args = self.stats_key.args(blobstore_id, repo_name);
                    if is_repaired {
                        STATS::scrub_repaired.add_value(1, args);
                    } else {
                        STATS::scrub_repair_required.add_value(1, args);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                scuba.add(ERROR_MSG, format!("{e:?}"));
            }
        }

        scuba
            .add(BLOBSTORE_ID, blobstore_id)
            // TODO parse out NodeType from string key prefix if we can. Or better, make blobstore keys typed?
            .add(NODE_KEY, key)
            .add(CHECK_TYPE, "scrub_repair")
            .add(CHECK_FAIL, if is_repaired { 0 } else { 1 })
            .add("session", ctx.session().metadata().session_id().to_string())
            .add(CTIME, ctime)
            .log();
    }
}
