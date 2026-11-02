/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::time::Duration;

/// Configuration for LoadMonitorModule (fbcode-only, ignored in OSS).
///
/// In fbcode builds, this configures system load monitoring (CPU, memory,
/// cgroup stats) for the service framework.
pub struct LoadMonitorConfig {
    pub memory_stats_ttl: Duration,
    pub cgroup_stats_ttl: Duration,
    pub counters_update_interval: Duration,
    pub cgroup_root_path: String,
}
