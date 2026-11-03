/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <string_view>

namespace facebook::eden {

/**
 * Config keys ("section:key") that EdenFS no longer reads, but that config
 * sources (in particular remotely-deployed dynamic configs) may still set.
 * Keys listed here are silently ignored instead of triggering the
 * "Ignoring unknown key in eden config" warning.
 *
 * Add a key here when deleting its ConfigSetting from EdenConfig. Remove it
 * once no config source sets the key anymore.
 */
inline constexpr std::string_view kDeadConfigKeys[] = {
    "core:migrate_existing_to_nfs",
    "core:migrate_existing_to_nfs_all_macos",
    "coroutines:enable-phase1",
    "coroutines:enable-phase2",
    "coroutines:enable-phase3",
    "coroutines:enable-phase4",
    "coroutines:enable-phase5",
    "coroutines:enable-phase6",
    "coroutines:enable-phase11",
    "experimental:batch-checkout-dir-mutations",
    "experimental:enable-nfs-crawl-detection",
    "experimental:filteredfs-optimize-unfiltered",
    "experimental:glob-skip-redundant-origin-hashes",
    "experimental:ignore-prefetch-result",
    "experimental:nfs-crawl-excluded-process-names",
    "experimental:prefetch-optimizations-v2",
    "experimental:propagate-checkout-errors",
    "experimental:skip-checkout-child-overlay-writes",
    "glob:allowed-suffix-queries",
    "glob:use-edenapi-suffix-query",
    "mount:accidental-unmount-recovery-interval",
    "mount:readdir-prefetch",
    "nfs:access-rate-limit-count",
    "nfs:access-rate-limit-window-seconds",
    "nfs:gid-access-modes",
    "nfs:root-access-mode",
    "nfs:root-access-rate-limit-count",
    "nfs:root-access-rate-limit-window-seconds",
    "nfs:uid-access-modes",
    "nfs:wheel-access-mode",
    "nfs:wheel-access-rate-limit-count",
    "nfs:wheel-access-rate-limit-window-seconds",
    "overlay:buffer-size",
    "overlay:buffered",
    "overlay:direct-file-writes",
    "overlay:direct-serialization",
    "overlay:maintenance-interval",
    "overlay:synchronous-mode",
    "overlay:unsafe-in-memory-overlay",
    "predictive-prefetch-profiles:size",
    "prefetch-profiles:predictive-prefetching-enabled",
    "store:max-tree-prefetches",
    "telemetry:enable-obc-on-eden",
    "telemetry:enable-xplatlogger-cli-usage",
    "telemetry:enable-xplatlogger-errors",
    "telemetry:enable-xplatlogger-events",
    "telemetry:enable-xplatlogger-fileaccess",
    "telemetry:error-scribe-category",
    "telemetry:file-access-scribe-category",
    "telemetry:enable-xplatlogger-rollouts",
    "thrift:leak-outstanding-requests-when-server-stops",
    "thrift:use-checkout-executor",
};

inline bool isDeadConfigKey(std::string_view section, std::string_view key) {
  for (auto dead : kDeadConfigKeys) {
    if (dead.size() == section.size() + 1 + key.size() &&
        dead.starts_with(section) && dead[section.size()] == ':' &&
        dead.ends_with(key)) {
      return true;
    }
  }
  return false;
}

} // namespace facebook::eden
