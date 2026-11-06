/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! EdenFS daemon counters and resource use over each fs-io phase.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use edenfs_client::checkout::find_checkout;
use edenfs_client::client::EdenFsClient;
use edenfs_client::counters::TelemetryCounters;
use edenfs_client::instance::EdenFsInstance;
use serde::Deserialize;
use serde::Serialize;
use sysinfo::Pid;
use sysinfo::ProcessesToUpdate;
use sysinfo::System;

use super::format_byte_size;
use super::format_duration_seconds;

/// Samples the EdenFS daemon that serves the benchmark's files.
pub(crate) struct DaemonSampler {
    client: Arc<EdenFsClient>,
    pid: Pid,
    system: System,
}

/// What the daemon did during one phase.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DaemonPhaseResult {
    /// CPU time the daemon's threads spent during the phase, user plus
    /// system.
    pub(super) cpu_seconds: f64,
    /// The daemon's resident set size at the end of the phase.
    pub(super) rss_bytes: u64,
    /// EdenFS's cumulative counters, as deltas over the phase.
    pub(super) counters: TelemetryCounters,
}

struct DaemonSnapshot {
    counters: TelemetryCounters,
    cpu_milliseconds: u64,
    rss_bytes: u64,
}

impl DaemonSampler {
    /// A sampler for the daemon serving `path`, or `None` when the path is
    /// not inside an EdenFS checkout.
    pub(crate) async fn for_path(instance: &EdenFsInstance, path: &Path) -> Result<Option<Self>> {
        if find_checkout(instance, path).is_err() {
            return Ok(None);
        }
        let client = instance.get_client();
        let daemon = client
            .get_health(None)
            .await
            .context("failed to reach the EdenFS daemon for counters")?;
        let pid = u32::try_from(daemon.pid)
            .map_err(|_| anyhow!("EdenFS reported an invalid pid {}", daemon.pid))?;
        let system = System::new().context("failed to open the process table")?;
        Ok(Some(Self {
            client,
            pid: Pid::from_u32(pid),
            system,
        }))
    }

    /// Runs `phase` and reports what the daemon did meanwhile.
    pub(crate) async fn measure<T>(
        &mut self,
        phase: impl FnOnce() -> Result<T>,
    ) -> Result<(T, DaemonPhaseResult)> {
        let before = self.snapshot().await?;
        let value = phase()?;
        let after = self.snapshot().await?;
        Ok((
            value,
            DaemonPhaseResult {
                cpu_seconds: after
                    .cpu_milliseconds
                    .saturating_sub(before.cpu_milliseconds) as f64
                    / 1_000.0,
                rss_bytes: after.rss_bytes,
                counters: after.counters - before.counters,
            },
        ))
    }

    async fn snapshot(&mut self) -> Result<DaemonSnapshot> {
        let counters = self
            .client
            .get_telemetry_counters()
            .await
            .context("failed to read EdenFS counters")?;
        self.system
            .refresh_processes(ProcessesToUpdate::Some(&[self.pid]), true);
        let process = self
            .system
            .process(self.pid)
            .ok_or_else(|| anyhow!("EdenFS daemon process {} not found", self.pid))?;
        Ok(DaemonSnapshot {
            counters,
            cpu_milliseconds: process.accumulated_cpu_time(),
            rss_bytes: process.memory(),
        })
    }
}

/// Runs `phase` under the sampler when there is one.
pub(super) async fn measure_phase<T>(
    sampler: Option<&mut DaemonSampler>,
    phase: impl FnOnce() -> Result<T>,
) -> Result<(T, Option<DaemonPhaseResult>)> {
    match sampler {
        Some(sampler) => {
            let (value, daemon) = sampler.measure(phase).await?;
            Ok((value, Some(daemon)))
        }
        None => Ok((phase()?, None)),
    }
}

/// The counters as `group.name` keys, so reports and comparisons need no
/// knowledge of the struct's layout. Counters EdenFS did not report are
/// absent.
pub(super) fn flatten_counters(counters: &TelemetryCounters) -> BTreeMap<String, u64> {
    let mut flattened = BTreeMap::new();
    if let Ok(value) = serde_json::to_value(counters) {
        flatten_value(&value, String::new(), &mut flattened);
    }
    flattened
}

fn flatten_value(value: &serde_json::Value, prefix: String, out: &mut BTreeMap<String, u64>) {
    match value {
        serde_json::Value::Object(fields) => {
            for (name, value) in fields {
                let key = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                flatten_value(value, key, out);
            }
        }
        serde_json::Value::Number(number) => {
            if let Some(number) = number.as_u64() {
                out.insert(prefix, number);
            }
        }
        _ => {}
    }
}

pub(super) fn write_daemon(
    formatter: &mut fmt::Formatter<'_>,
    daemon: &DaemonPhaseResult,
) -> fmt::Result {
    writeln!(
        formatter,
        "EdenFS daemon: {} CPU, {} resident",
        format_duration_seconds(daemon.cpu_seconds),
        format_byte_size(daemon.rss_bytes)
    )?;
    for (name, value) in flatten_counters(&daemon.counters) {
        if value > 0 {
            writeln!(formatter, "{name:<64} {value:>12}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_nested_counters_and_skips_missing_groups() {
        let counters: TelemetryCounters = serde_json::from_value(serde_json::json!({
            "fs_stats": {
                "syscall_open": 3, "syscall_read": 0, "syscall_readdir": 0,
                "syscall_readdirplus": 0, "syscall_write": 0, "syscall_statfs": 0,
                "syscall_getattr": 0, "syscall_setattr": 0, "syscall_lookup": 7,
                "syscall_access": 0, "syscall_mkdir": 0
            },
            "thrift_stats": {},
            "backend_stats": {
                "fetch": {
                    "edenapi_backend": {
                        "edenapi_fetches_blobs": 1, "edenapi_fetches_trees": 2,
                        "edenapi_requests": 1
                    }
                },
                "prefetch": {}
            },
            "local_cache_stats": { "fetch": {}, "prefetch": {} },
            "file_metadata_stats": null,
            "tree_metadata_stats": null,
            "monorepo_inodes_stats": null
        }))
        .expect("counters should deserialize");

        let flattened = flatten_counters(&counters);

        assert_eq!(flattened["fs_stats.syscall_open"], 3);
        assert_eq!(flattened["fs_stats.syscall_lookup"], 7);
        assert_eq!(
            flattened["backend_stats.fetch.edenapi_backend.edenapi_fetches_trees"],
            2
        );
        assert!(
            !flattened
                .keys()
                .any(|key| key.starts_with("file_metadata_stats")),
            "groups EdenFS did not report are absent"
        );
    }
}
