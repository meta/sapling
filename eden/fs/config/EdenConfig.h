/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/Portability.h>
#include <folly/system/HardwareConcurrency.h>
#include <chrono>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_map>
#include <unordered_set>
#include <vector>

#include <thrift/lib/cpp/concurrency/ThreadManager.h>

#include "eden/common/utils/PathFuncs.h"
#include "eden/fs/config/ConfigSetting.h"
#include "eden/fs/config/ConfigSource.h"
#include "eden/fs/config/ConfigVariables.h"
#include "eden/fs/config/HgObjectIdFormat.h"
#include "eden/fs/config/InodeCatalogType.h"
#include "eden/fs/config/MountProtocol.h"
#include "eden/fs/config/NfsAccessMode.h"
#include "eden/fs/config/RestrictedContentMode.h"
#include "eden/fs/eden-config.h"

#ifdef EDEN_HAVE_TCC_DISCLAIM_TEAM_ID
#include "eden/fs/config/facebook/TccDisclaimTeamId.h" // @manual
#else
namespace facebook::eden {
// No fleet signing team in open-source builds; set core:disclaim-tcc-team-id.
constexpr std::string_view kTccDisclaimTeamId = "";
} // namespace facebook::eden
#endif

namespace re2 {
class RE2;
} // namespace re2

namespace facebook::eden {

extern const AbsolutePath kUnspecifiedDefault;

/**
 * EdenConfig holds the Eden configuration settings. It is constructed from
 * cli settings, user configuration files, system configuration files and
 * default values. It provides methods to determine if configuration files
 * have been changed (fstat on the source files).
 *
 * To augment configuration, add a ConfigurationSetting member variable to
 * this class. ConfigurationSettings require a key to identify the setting
 * in configuration files. For example, "core:edenDirectory".
 */
class EdenConfig : private ConfigSettingManager {
 public:
  using SourceVector = std::vector<std::shared_ptr<ConfigSource>>;

  /**
   * Manually construct a EdenConfig object with some default values and
   * ConfigSources. Duplicate ConfigSources for the same ConfigSourceType are
   * disallowed. ConfigSources are immediately applied to the ConfigSettings.
   */
  explicit EdenConfig(
      ConfigVariables substitutions,
      AbsolutePathPiece userHomePath,
      AbsolutePathPiece systemConfigDir,
      SourceVector configSources);

  /**
   * EdenConfig is heap-allocated and not copyable or moveable in general. This
   * constructor clones an existing EdenConfig and is called when a config file
   * is reloaded.
   */
  explicit EdenConfig(const EdenConfig& source);
  explicit EdenConfig(EdenConfig&& source) = delete;

  EdenConfig& operator=(const EdenConfig& source) = delete;
  EdenConfig& operator=(EdenConfig&& source) = delete;

  /**
   * Create an EdenConfig for testing usage
   */
  static std::shared_ptr<EdenConfig> createTestEdenConfig();

  /**
   * Return the config data as a EdenConfigData structure that can be
   * thrift-serialized.
   */
  EdenConfigData toThriftConfigData() const;

  /** Get the path to client certificate. */
  const std::optional<AbsolutePath> getClientCertificate() const;

  /**
   * Clear all configuration for the given config source.
   */
  void clearAll(ConfigSourceType);

  /**
   *  Register the configuration setting. The fullKey is used to parse values
   *  from the toml file. It is of the form: "core:userConfigPath"
   */
  void registerConfiguration(ConfigSettingBase* configSetting) override;

  /**
   * Returns the value in optional string for the given config key.
   * Throws if the config key is ill-formed.
   */
  std::optional<std::string> getValueByFullKey(
      std::string_view configKey) const;

  /**
   * Unconditionally apply all ConfigSources to the ConfigSettings.
   */
  void reload();

  /**
   * If any ConfigSources are stale, clones this EdenConfig and applies the
   * updated sources to the ConfigSettings.
   *
   * If no sources have changed, returns nullptr.
   */
  std::shared_ptr<const EdenConfig> maybeReload() const;

  /**
   * Returns the system config dir (--etc-eden-dir) path that EdenFS was passed
   * during startup.
   */
  const AbsolutePath& getSystemConfigDir() const {
    return systemConfigDir_;
  }

 private:
  /**
   * Utility method for converting ConfigSourceType to the filename (or cli).
   * @return the string value for the ConfigSourceType.
   */
  std::string toString(ConfigSourceType cs) const;

  /**
   * Returns a Thrift-suitable path corresponding to the given source's config
   * file.
   */
  std::string toSourcePath(ConfigSourceType cs) const;

  void parseAndApplyConfigFile(
      int configFd,
      AbsolutePathPiece configPath,
      ConfigSourceType configSourceType);

  /**
   * Mapping of section name : (map of attribute : config values). The
   * ConfigSetting constructor registration populates this map.
   */
  std::map<std::string, std::map<std::string, ConfigSettingBase*>> configMap_;

  std::shared_ptr<ConfigVariables> substitutions_;

  static constexpr size_t kConfigSourceLastIndex =
      static_cast<size_t>(apache::thrift::TEnumTraits<ConfigSourceType>::max());

  /**
   * Each ConfigSourceType has exactly zero or one ConfigSource. These slots are
   * iterated in order during reload to populate EdenConfig.
   */
  std::array<std::shared_ptr<ConfigSource>, kConfigSourceLastIndex + 1>
      configSources_;

  /**
   * The daemon sometimes needs to invoke the CLI. In doing so, it needs to pass
   * information about which system config dir (--etc-eden-dir) it's using.
   * Without this information, the CLI cannot properly load configs and will
   * default to using the default system config location.
   */
  const AbsolutePath systemConfigDir_;

  /*
   * Settings follow. Their initialization registers themselves with the
   * EdenConfig object. We make use of the registration to iterate over
   * ConfigSettings generically for parsing, copy, assignment and move
   * operations. We update the property value in the constructor (since we don't
   * have the home directory here).
   *
   * The following fields must come after configMap_.
   */

 public:
  // [core]

  ConfigSetting<AbsolutePath> edenDir{
      "core:edenDirectory",
      kUnspecifiedDefault,
      this};

  ConfigSetting<AbsolutePath> systemIgnoreFile{
      "core:systemIgnoreFile",
      kUnspecifiedDefault,
      this};

  ConfigSetting<AbsolutePath> userIgnoreFile{
      "core:ignoreFile",
      kUnspecifiedDefault,
      this};

  /** Allow creating named pipes on Linux. */
  ConfigSetting<bool> enableFifo{"core:enable-fifo", true, this};

  /**
   * How often to check the on-disk lock file to ensure it is still valid.
   * EdenFS will exit if the lock file is no longer valid.
   */
  ConfigSetting<std::chrono::nanoseconds> checkValidityInterval{
      "core:check-validity-interval",
      std::chrono::minutes(5),
      this};

  /**
   * How many threads to use for the misc EdenCPUThreadPool.
   */
  ConfigSetting<uint8_t> edenCpuPoolNumThreads{
      "core:eden-cpu-pool-num-threads",
      12,
      this};

  /**
   * Whether graceful takeover client should purposefully return an exception
   * during the takeover process. Intended to be used only in integration tests.
   *
   * NOTE: if you want the takeover server to throw an exception, you can use
   * FaultInjector with the key/value pair: ("takeover", "error during send")
   */
  ConfigSetting<bool> clientThrowsDuringTakeover{
      "core:client-throws-during-takeover",
      false,
      this};

  /**
   * Takeover receive timeout in seconds. This is the time that the client will
   * wait for the server to send the takeover data. This timeout applies to each
   * chunk of data when sending data in chunks.
   */
  ConfigSetting<std::chrono::nanoseconds> takeoverReceiveTimeout{
      "core:takeover-receive-timeout",
      std::chrono::seconds(150),
      this};

  /**
   * How long a graceful restart that is not forced lets the Thrift requests
   * in flight finish. It waits up to this long for the requests that were
   * running when it was asked and refuses if they have not finished; a
   * request that has already been running longer than this is refused
   * without waiting. Once the Thrift server has stopped, whatever is still
   * running after this long again is cancelled, so that no request can hold
   * the stop to its deadline.
   */
  ConfigSetting<std::chrono::nanoseconds> takeoverRequestGrace{
      "core:takeover-request-grace",
      std::chrono::seconds(30),
      this};

  /**
   * Whether a graceful restart that is not forced waits for the Thrift
   * requests in flight, see core:takeover-request-grace. When off, it stops
   * the Thrift server at once and leaves the requests to the stop deadline,
   * as EdenFS did before the grace period existed, and it does not refuse
   * for recent activity either. The open trace streams are ended either way,
   * since a stream holds its connection until its client leaves.
   */
  ConfigSetting<bool> takeoverWaitsForRequests{
      "experimental:takeover-waits-for-requests",
      true,
      this};

  /**
   * How long after a checkout, a reset of the parent commits, a mount or an
   * unmount EdenFS counts as in use. A graceful restart that asks to proceed
   * only if EdenFS is idle, as the hourly restarter does, is refused while
   * one of those ran within this window: a checkout is several Thrift calls
   * in a row, and a takeover between two of them lands in the middle of it.
   * `eden restart --graceful` run by hand does not ask, so it is only held
   * back by the requests in flight.
   */
  ConfigSetting<std::chrono::nanoseconds> takeoverRecentActivityWindow{
      "core:takeover-recent-activity-window",
      std::chrono::minutes(10),
      this};

  /**
   * At startup, EdenFS will attempt to set its memory priority to the following
   * value. If the value is null, EdenFS will not attempt to set its priority.
   *
   * On macOS, this corresponds to a Jetsam Priority value (as of June
   * 2025, these values range between 0 (lowest, most likely to be killed), and
   * 210 (highest, least likely to be killed).
   *
   * On Linux, this corresponds to the
   * oom_score_adj value (as of June 2025, these values range between -1000
   * (highest, least likely to be killed) and 1000 (most likely to be killed).
   */
  ConfigSetting<std::optional<int32_t>> daemonTargetMemoryPriority{
      "core:daemon-target-memory-priority",
      std::nullopt,
      this};

  /**
   * Process-wide budget for retrying unhandled synchronous BUS_ADRERR faults
   * on Linux. Zero disables retries. This setting is ignored on other
   * platforms.
   */
  ConfigSetting<uint32_t> sigbusRetryBudget{
      "core:sigbus-retry-budget",
      0,
      this};
  /**
   * Similar to the above config, but sets the PrivHelper's priority instead.
   * Leave unset to avoid changing the PrivHelper's priority.
   */
  ConfigSetting<std::optional<int32_t>> privHelperTargetMemoryPriority{
      "core:priv-helper-target-memory-priority",
      std::nullopt,
      this};

  /**
   * Keep the Linux cgroup's file-backed LRU below a target by periodically
   * writing to cgroup v2 memory.reclaim. Only acts when the daemon runs in a
   * cgroup named edenfs*, which is how systemd cgroup isolation and systemd
   * lifecycle management place it. It has no effect on macOS or Windows.
   */
  ConfigSetting<bool> enableCgroupFileCacheReclaim{
      "core:enable-cgroup-file-cache-reclaim",
      false,
      this};

  /** Target size for active_file plus inactive_file in bytes. */
  ConfigSetting<uint64_t> cgroupFileCacheTargetBytes{
      "core:cgroup-file-cache-target-bytes",
      10ULL * 1024 * 1024 * 1024,
      this};

  /**
   * Maximum bytes to request from memory.reclaim in one pass. Zero disables
   * the cap.
   */
  ConfigSetting<uint64_t> cgroupFileCacheMaxReclaimBytes{
      "core:cgroup-file-cache-max-reclaim-bytes",
      10ULL * 1024 * 1024 * 1024,
      this};

  /** How often to check the cgroup's file-backed LRU. */
  ConfigSetting<std::chrono::nanoseconds> cgroupFileCacheReclaimInterval{
      "core:cgroup-file-cache-reclaim-interval",
      std::chrono::seconds{10},
      this};

  /**
   * If true, EdenFS refuses to start when running in a non-root mount
   * namespace.
   */
  ConfigSetting<bool> requireRootMountNamespace{
      "core:require-root-mount-namespace",
      false,
      this};

  /**
   * Timeout value for a clean shutdown on SIGTERM. If the timeout elapses, we
   * exit immediately. Set to zero to revert to the old behavior where we
   * unregister the signal handler unconditionally, causing the next signal to
   * instantly kill us.
   */
  ConfigSetting<std::chrono::nanoseconds> sigtermShutdownTimeout{
      "core:sigterm-shutdown-timeout",
      std::chrono::seconds(20),
      this};

  /**
   * Allow the privhelper to relaunch edenfs when this daemon dies without
   * announcing a clean shutdown.
   *
   * macOS only. Linux gets the same behavior from systemd, and a second
   * supervisor there would fight the first.
   */
  ConfigSetting<bool> restartEdenfsOnCrash{
      "privhelper:restart-edenfs-on-crash",
      false,
      this};

  /**
   * Maximum number of privhelper-driven restarts allowed within
   * restartEdenfsWindow. Beyond it edenfs is left down.
   */
  ConfigSetting<uint32_t> restartEdenfsMaxCount{
      "privhelper:restart-edenfs-max-count",
      3,
      this};

  /**
   * Window over which restartEdenfsMaxCount is counted. A window that
   * elapses without hitting the limit resets the count to zero.
   */
  ConfigSetting<std::chrono::nanoseconds> restartEdenfsWindow{
      "privhelper:restart-edenfs-window",
      std::chrono::minutes(10),
      this};

  /**
   * Time offset before shutdown to cancel active requests. This allows
   * requests to complete gracefully before the shutdown timeout is reached.
   * The cancellation happens at (shutdownTimeout -
   * cancellationOffsetBeforeShutdown).
   */
  ConfigSetting<std::chrono::nanoseconds> cancellationOffsetBeforeShutdown{
      "core:cancellation-offset-before-shutdown",
      std::chrono::seconds(5),
      this};

  /**
   * On macOS, TCC evaluates permissions such as
   * kTCCServiceSystemPolicyNetworkVolumes against a process's "responsible
   * process", which the daemon normally inherits from whatever launched it,
   * so filesystem access can fail depending on launch context. If true, the
   * daemonizing parent spawns the long-lived daemon with TCC responsibility
   * disclaimed, making the daemon its own responsible process so grants keyed
   * to its code signature apply deterministically. The daemon additionally
   * has to be signed by the team that grant is keyed to (see
   * disclaimTccTeamId); other builds never disclaim regardless of this
   * setting. Only used on macOS.
   */
  ConfigSetting<bool> disclaimTccResponsibility{
      "core:disclaim-tcc-responsibility",
      true,
      this};

  /**
   * Team identifier a macOS code signature must carry for the daemon to
   * disclaim TCC responsibility (see disclaimTccResponsibility). Defaults to
   * the fleet release team the MDM PPPC grant is keyed to in Meta builds,
   * empty otherwise. The privhelper spawn uses the compiled default, see the
   * TODO in PrivHelperImpl.cpp.
   */
  ConfigSetting<std::string> disclaimTccTeamId{
      "core:disclaim-tcc-team-id",
      std::string{kTccDisclaimTeamId},
      this};

  // [daemon]

  /**
   * Environment assignments applied by the launcher before starting EdenFS.
   * The daemon registers this launcher-owned setting to accept it in shared
   * configuration files.
   */
  ConfigSetting<std::vector<std::string>> daemonEnvironment{
      "daemon:environment",
      {},
      this};

  // [config]

  /**
   * How often the on-disk config information should be checked for changes.
   */
  ConfigSetting<std::chrono::nanoseconds> configReloadInterval{
      "config:reload-interval",
      std::chrono::minutes(5),
      this};

  // [thrift]

  /**
   * Whether Eden should implement its own unix domain socket permission checks
   * or rely on filesystem permissions.
   */
  ConfigSetting<bool> thriftUseCustomPermissionChecking{
      "thrift:use-custom-permission-checking",
      true,
      this};

  /**
   * If Eden is using custom permission checking, the list of methods that any
   * user can call.
   */
  ConfigSetting<std::vector<std::string>> thriftFunctionsAllowlist{
      "thrift:functions-allowlist",
      std::vector<std::string>{
          "BaseService.getCounter",
          "BaseService.getCounters",
          "BaseService.getRegexCounters",
          "BaseService.getSelectedCounters"},
      this};

  /**
   * The number of Thrift worker threads.
   */
  ConfigSetting<size_t> thriftNumWorkers{
      "thrift:num-workers",
      folly::available_concurrency(),
      this};

  /**
   * Maximum number of active Thrift requests.
   */
  ConfigSetting<uint32_t> thriftMaxRequests{
      "thrift:max-requests",
      apache::thrift::concurrency::ThreadManager::DEFAULT_MAX_QUEUE_SIZE,
      this};

  /**
   * Request queue timeout (rounded down to the nearest millisecond).
   */
  ConfigSetting<std::chrono::nanoseconds> thriftQueueTimeout{
      "thrift:queue-timeout",
      std::chrono::seconds(30),
      this};

  /**
   * How long `streamJournalChanged` waits after a change before notifying a
   * subscriber, so a burst of changes produces one notification per window
   * rather than one per change. Zero notifies on every change.
   */
  ConfigSetting<std::chrono::nanoseconds> thriftJournalChangedDelay{
      "thrift:journal-changed-delay",
      std::chrono::milliseconds(5),
      this};

  /**
   * Whether Eden should use resource pools
   */
  ConfigSetting<bool> thriftUseResourcePools{
      "thrift:use-resource-pools",
      false,
      this};

  /**
   * Whether Eden should use serial execution for each request. Resource pools
   * must be enabled for this to take effect
   */
  ConfigSetting<bool> thriftUseSerialExecution{
      "thrift:use-serial-execution",
      false,
      this};

  /**
   * Number of threads in the diff work executor, which runs the inode loads
   * and file hashing that a diff of the working copy entails.
   */
  ConfigSetting<uint64_t> numDiffWorkThreads{
      "thrift:checkout-revision-num-servicing-threads",
      folly::available_concurrency(),
      this};

  /**
   * Whether status runs the overlay reads and content hashing it needs for
   * materialized files on the diff work executor, in parallel, instead of
   * inline on the Thrift worker handling the request. Checkout uses the same
   * executor; see EdenServer::getDiffWorkExecutor().
   */
  ConfigSetting<bool> thriftUseStatusExecutor{
      "thrift:use-status-executor",
      true,
      this};

  /**
   * Whether Eden should use a dedicated executor for prefetchFilesV2 requests.
   * This is meant to help with prefetch performance while using serial
   * execution for other Thrift requests.
   */
  ConfigSetting<bool> thriftUsePrefetchExecutor{
      "thrift:use-prefetch-executor",
      true,
      this};

  /**
   * Number of threads that will service the prefetchFilesV2 Thrift endpoint
   * when using its own executor.
   */
  ConfigSetting<uint64_t> numPrefetchThreads{
      "thrift:prefetch-num-servicing-threads",
      // 8 is plenty - any more just leads to excess sl batch blob fetches that
      // use extra memory with no throughput increase.
      std::max(std::min(folly::available_concurrency() / 2, 8u), 1u),
      this};

  /**
   * Number of blob IDs passed to each prefetchBlobs() call.
   */
  ConfigSetting<uint32_t> prefetchBlobBatchSize{
      "thrift:prefetch-blob-batch-size",
      4096,
      this};

  /**
   * How often to collect Thrift server metrics. The default value mirrors the
   * value from facebook::fb303::TServerCounters::kDefaultSampleRate
   */
  ConfigSetting<uint32_t> thriftServerObserverSamplingRate{
      "thrift:server-observer-sampling-rate",
      32,
      this};

  /**
   * How often to publish Thrift server metrics in milliseconds.
   */
  ConfigSetting<std::chrono::nanoseconds> thriftServerObserverPublishInterval{
      "thrift:server-observer-publish-interval",
      std::chrono::milliseconds(1000),
      this};

  /**
   * Whether streamJournalChanged should complete subscriptions with a typed
   * SHUTTING_DOWN EdenError during intentional EdenFS shutdown.
   */
  ConfigSetting<bool> thriftStreamJournalChangedShuttingDownError{
      "thrift:stream-journal-changed-shutting-down-error",
      true,
      this};

  /**
   * Enable use of MiniTracer to collect checkout timing info.
   */
  ConfigSetting<bool> thriftCheckoutTimeTracing{
      "thrift:checkout-time-tracing",
      true,
      this};
  /**
   * How long the Thrift server waits for in-flight requests to finish when it
   * stops. A request that outlives the wait makes the daemon abort.
   */
  ConfigSetting<std::chrono::nanoseconds> thriftWorkersJoinTimeout{
      "thrift:workers-join-timeout",
      std::chrono::seconds(120),
      this};

  // [ssl]

  ConfigSetting<AbsolutePath> clientCertificate{
      "ssl:client-certificate",
      kUnspecifiedDefault,
      this};

  ConfigSetting<std::vector<std::string>> clientCertificateLocations{
      "ssl:client-certificate-locations",
      std::vector<std::string>{},
      this};

  // [mount]

  /**
   * After checkout completes with conflicts, how many conflicts should be
   * printed in the edenfs.log?
   */
  ConfigSetting<uint64_t> numConflictsToLog{
      "mount:num-conflicts-to-log",
      10,
      this};

  /**
   * Number of directories whose paths are remembered so that an inode's path
   * can be built from its parent's instead of walked up to the root. 0
   * disables the cache. Read when a mount starts.
   */
  ConfigSetting<size_t> inodePathCacheSize{
      "mount:inode-path-cache-size",
      64,
      this};

  /**
   * How often inode garbage collection will run.
   *
   * Default to every hour.
   */
  ConfigSetting<std::chrono::nanoseconds> gcPeriod{
      "mount:garbage-collection-period",
      std::chrono::hours(1),
      this};

  /**
   * Minimum interval between pressure-based GC runs for a mount, used when
   * inode count is at/above gc-pressure-max-inodes.
   */
  ConfigSetting<std::chrono::nanoseconds> pressureBasedGcPeriodMin{
      "mount:pressure-gc-period-min",
      std::chrono::seconds{30},
      this};

  /**
   * Maximum interval between pressure-based GC runs for a mount, used when
   * inode count is below gc-pressure-min-inodes.
   */
  ConfigSetting<std::chrono::nanoseconds> pressureBasedGcPeriodMax{
      "mount:pressure-gc-period-max",
      std::chrono::minutes{5},
      this};

  /**
   * How often the pressure-based GC scheduler wakes up to check whether each
   * mount is due for a pressure-derived GC run.
   */
  ConfigSetting<std::chrono::nanoseconds> pressureBasedGcTickPeriod{
      "mount:pressure-gc-tick-period",
      std::chrono::seconds{5},
      this};

  /**
   * Pressure-based GC waits the regular garbage-collection-period before
   * running again when a run reclaims no more than this percentage of the
   * inodes it invalidated (see EdenMount::recordPressureGcOutcome). This
   * avoids re-invalidating a large set of stuck inodes at the pressure-derived
   * rate when doing so has no effect. Zero disables the check.
   */
  ConfigSetting<uint64_t> pressureBasedGcMinReclaimPercent{
      "mount:pressure-gc-min-reclaim-percent",
      10,
      this};

  /**
   * Whether the sweep that follows an inode GC run's invalidation phase
   * applies the run's cutoff to inodes without a filesystem reference, so
   * one that a status or Thrift request referenced recently is kept. When
   * off, every inode without a filesystem reference is unloaded on each run.
   */
  ConfigSetting<bool> gcSweepHonorsCutoff{
      "mount:gc-sweep-honors-cutoff",
      true,
      this};

  /**
   * Cancel an inode GC run after this many tree-load failures, and have
   * pressure-based GC wait the regular garbage-collection-period before
   * running again. Zero disables the limit.
   */
  ConfigSetting<uint64_t> gcMaxTreeLoadFailures{
      "mount:gc-max-tree-load-failures",
      10,
      this};

  /**
   * Whether pressure-based GC discovers inodes pinned by processes (via the
   * privhelper `scan-pins` mode) so it can reclaim everything else while
   * skipping the pinned chains. On Linux (FUSE) pins are the directories
   * used as working directories or roots: invalidating one's entry breaks
   * getcwd() and path resolution for the pinning process without reclaiming
   * anything, since the kernel cannot FORGET a pinned inode. On macOS (NFS)
   * pins also include files held open or mapped, since EdenFS forgets the
   * inode itself and the process would get ESTALE.
   *
   * When disabled, or whenever the scan fails, pressure-based GC leaves all
   * directories alone (file reclamation is unaffected).
   */
  ConfigSetting<bool> pressureBasedGcScanPins{
      "mount:pressure-gc-scan-pins",
      true,
      this};

  /**
   * How long pressure-based GC waits for the privhelper's pin scan before
   * killing it and running with pins unknown. A scan takes well under a
   * second on an idle machine, but pressure GC runs on machines short of
   * memory, where everything is slow; a run without pins forgets open files
   * on NFS, so waiting is the better outcome. The wait is cancelled with GC
   * itself, so a checkout never sits behind it.
   */
  ConfigSetting<std::chrono::nanoseconds> pressureBasedGcPinScanTimeout{
      "mount:pressure-gc-pin-scan-timeout",
      std::chrono::seconds{60},
      this};

  /**
   * If the number of inodes is greater than this threshold, the garbage
   * collection cutoff will be more aggressive.
   *
   * Set to zero to disable aggressive GC.
   *
   * See aggressiveGcCutoff for more details.
   */
  ConfigSetting<uint64_t> aggressiveGcThreshold{
      "mount:agrrssive-gc-threshold",
      0,
      this};

  /**
   * Inodes with a last used time (atime on Windows) older than cutoff will be
   * invalidated during GC.
   *
   * When total number of inodes is greater than a threshold, cutoff will be
   * more aggressive.
   *
   * See aggressiveGcThreshold for more details.
   */
  ConfigSetting<OneHourMinDuration> aggressiveGcCutoff{
      "mount:aggressive-gc-cutoff",
      OneHourMinDuration(std::chrono::hours(1)),
      this};

  /**
   * Inodes with a last used time (atime on Windows) older than cutoff will be
   * invalidated during GC.
   *
   * Default to a day. On Windows, the atime is  updated only once an hour, so
   * values below 1h are disallowed.
   */
  ConfigSetting<OneHourMinDuration> gcCutoff{
      "mount:garbage-collection-cutoff",
      OneHourMinDuration(std::chrono::hours(24)),
      this};

  /**
   * Inode count below which no GC pressure is applied.
   * Below this count, max TTL and max GC cutoff are used.
   */
  ConfigSetting<uint64_t> gcPressureMinInodes{
      "mount:gc-pressure-min-inodes",
      10000,
      this};

  /**
   * Inode count at/above which maximum GC pressure is applied.
   * At or above this count, min TTL and min GC cutoff are used.
   */
  ConfigSetting<uint64_t> gcPressureMaxInodes{
      "mount:gc-pressure-max-inodes",
      2000000,
      this};

  /**
   * Maximum FUSE entry/attribute cache TTL (seconds), used when inode
   * count is below gc-pressure-min-inodes.
   */
  ConfigSetting<uint64_t> fuseTtlMaxSeconds{
      "mount:fuse-ttl-max-seconds",
      3600,
      this};

  /**
   * Minimum FUSE entry/attribute cache TTL (seconds), used when inode
   * count is at/above gc-pressure-max-inodes.
   */
  ConfigSetting<uint64_t> fuseTtlMinSeconds{
      "mount:fuse-ttl-min-seconds",
      1,
      this};

  /**
   * FUSE negative dentry cache TTL (seconds) for ENOENT lookups.
   *
   * EdenFS answers a lookup of a non-existent path with nodeid 0 and this
   * TTL, so the kernel caches the negative (ENOENT) result for that long.
   * When the path later appears (e.g. after checkout), EdenFS clears the
   * cached entry with FUSE_NOTIFY_INVAL_ENTRY. Should that invalidation ever
   * be missed, the stale negative entry would otherwise persist for the full
   * TTL and keep hiding the now-existing file until a write (e.g. touch)
   * forces a fresh lookup. A bounded TTL caps how long such a stale entry can
   * persist.
   */
  ConfigSetting<uint64_t> fuseNegativeDcacheTtlSeconds{
      "fuse:negative-dcache-ttl-seconds",
      60,
      this};

  /**
   * Maximum GC cutoff duration (seconds), used at lowest pressure.
   * Inodes not accessed within this duration are candidates for GC.
   */
  ConfigSetting<uint64_t> gcCutoffMaxSeconds{
      "mount:gc-cutoff-max-seconds",
      21600,
      this};

  /**
   * Minimum GC cutoff duration (seconds), used at highest pressure.
   */
  ConfigSetting<uint64_t> gcCutoffMinSeconds{
      "mount:gc-cutoff-min-seconds",
      60,
      this};

  /**
   * Keep the inode-number-ordered index that readdir builds for a directory
   * across the requests of one listing, instead of rebuilding it for every
   * request. The index is dropped once the listing ends or the directory
   * changes.
   */
  ConfigSetting<bool> experimentalReaddirIndexCache{
      "experimental:readdir-index-cache",
      true,
      this};

  /**
   * Specify the interval of periodic mount health checks.
   *
   * Health checks only observe and report unless mount-health-check-recovery
   * is set; they never unmount anything.
   */
  ConfigSetting<std::chrono::nanoseconds> mountHealthCheckInterval{
      "mount:mount-health-check-interval",
      std::chrono::minutes(0),
      this};

  /**
   * When set, the mount health check remounts the configured checkouts it
   * finds not served, instead of only reporting them.
   */
  ConfigSetting<bool> mountHealthCheckRecovery{
      "mount:mount-health-check-recovery",
      false,
      this};

  /**
   * When set, a checkout whose FUSE or NFS channel stops without anyone asking
   * (ENODEV, EOF, read errors) is remounted right away, without waiting for
   * the next mount health check.
   */
  ConfigSetting<bool> recoverOnChannelStop{
      "mount:recover-on-channel-stop",
      false,
      this};

  /**
   * Have the mount health check confirm that a checkout is still mounted with
   * statx(2) on its root, which takes no mount-namespace lock, instead of
   * reading the kernel mount table. It falls back to the mount table when the
   * mount's device is not known yet or does not match. Linux only.
   */
  ConfigSetting<bool> mountHealthCheckStatx{
      "mount:mount-health-check-statx",
      false,
      this};

  /**
   * How long a mount health check probe may run before the checkout is
   * reported as hung (`daemon_running_mount_hung`). At or below the 5 s probe
   * timeout, including the default of 0, a probe that runs past 5 s is
   * reported as timed out instead.
   */
  ConfigSetting<std::chrono::nanoseconds> mountHealthCheckHangThreshold{
      "mount:mount-health-check-hang-threshold",
      std::chrono::seconds(0),
      this};

  /**
   * The soong build system used in AOSP loves to crawls the entirety of the
   * repository, including the .eden directory. In doing so, it infinitely
   * recurse into the this-dir.
   *
   * Per kemurphy@, sending a pull request to soong will be refused, thus we
   * need to workaround it in EdenFS.
   *
   * DO NOT SET UNLESS YOU ARE RUNNING AOSP ON EDENFS.
   *
   * TODO(T147468271): Remove this once soong has been taught to recognize
   * EdenFS/Mercurial correctly.
   */
  ConfigSetting<bool> findIgnoreInDotEden{
      "mount:find-ignore-in-dot-eden",
      false,
      this};

  // [store]

  /**
   * The minimum duration between logging occurrences of failed HgProxyHash
   * loads.
   */
  ConfigSetting<std::chrono::nanoseconds> missingHgProxyHashLogInterval{
      "store:missing-hgproxyhash-log-interval",
      std::chrono::minutes{10},
      this};

  /**
   * If the number of fetching requests of a process reaches this number,
   * a FetchHeavy event will be sent to Scuba.
   */
  ConfigSetting<uint32_t> fetchHeavyThreshold{
      "store:fetch-heavy-threshold",
      100000,
      this};

  /**
   * The maximum number blob SHA-1s and sizes to keep in memory per mount. See
   * the comment on `ObjectStore::metadataCache_` for more details.
   */
  ConfigSetting<uint64_t> metadataCacheSize{
      "store:metadata-cache-size",
      1'000'000,
      this};

  /**
   * Number of shards to use for the metadata cache.
   *
   * This is used to reduce lock contention on the metadata cache. Higher number
   * means lower contention, but more imperfect LRU property (each shard has its
   * own LRU).
   */
  ConfigSetting<uint64_t> metadataCacheShards{
      "store:metadata-cache-shards",
      32,
      this};

  ConfigSetting<bool> warmTreeAuxMemCacheIfTreeFromBackingStore{
      "store:warm-aux-mem-cache-tree-backing-store",
      false,
      this};
  // [fuse]

  /**
   * The maximum number of concurrent background FUSE requests we allow the
   * kernel to send us. background should mean things like readahead prefetches
   * and direct I/O, but may include things that seem like more traditionally
   * foreground I/O. What counts as "background" seems to be up to the
   * discretion of the kernel.
   *
   * Linux FUSE defaults to 12, but EdenFS can handle a great deal of
   * concurrency.
   */
  ConfigSetting<int32_t> fuseMaximumBackgroundRequests{
      "fuse:max-concurrent-requests",
      1000,
      this};

  /**
   * The number of FUSE dispatcher threads to spawn.
   */
  ConfigSetting<int32_t> fuseNumDispatcherThreads{
      "fuse:num-dispatcher-threads",
      16,
      this};

  /**
   * The number of FUSE invalidation threads to spawn per mount.
   */
  ConfigSetting<uint32_t> fuseNumInvalidationThreads{
      "fuse:num-invalidation-threads",
      4,
      this};

  /**
   * The maximum time duration allowed for a fuse request. If a request exceeds
   * this amount of time, an ETIMEDOUT error will be returned to the kernel to
   * avoid blocking forever.
   */
  ConfigSetting<std::chrono::nanoseconds> fuseRequestTimeout{
      "fuse:request-timeout",
      std::chrono::minutes(1),
      this};

  /**
   * The string we use in the vfs type when mounting the fuse mount. Others will
   * see this in the mount table on the system.
   */
  ConfigSetting<std::string> fuseVfsType{"fuse:vfs-type", "fuse", this};

  /**
   * The read-ahead size in KB for FUSE mounts. This is written to
   * /sys/class/bdi/{major}:{minor}/read_ahead_kb after mounting.
   * If nullopt, the kernel default is used.
   * This setting only applies to Linux FUSE mounts.
   */
  ConfigSetting<std::optional<uint32_t>> fuseBdiReadAheadKb{
      "fuse:bdi-read-ahead-kb",
      std::nullopt,
      this};

  /**
   * The maximum number of pages per FUSE read request. Default kernel value
   * is 32 pages (128KB with 4KB pages). Setting FUSE_MAX_PAGES flag in
   * FUSE_INIT allows configuring this up to 256 pages (1MB with 4KB pages).
   * Set to 0 to use the kernel default (32).
   */
  ConfigSetting<uint32_t> fuseMaxPages{"fuse:max-pages", 0, this};

  /**
   * Pass FUSE write data straight through to FileInode::write when the
   * inode is already loaded, instead of copying it into a std::string for
   * an asynchronous continuation.
   */
  ConfigSetting<bool> experimentalFuseAvoidWriteCopy{
      "experimental:fuse-avoid-write-copy",
      true,
      this};

  /**
   * Handle FUSE_RENAME2 requests using RENAME_NOREPLACE semantics.
   */
  ConfigSetting<bool> experimentalFuseRenameNoReplace{
      "experimental:fuse-rename-noreplace",
      false,
      this};

  /**
   * Negotiate FUSE_HANDLE_KILLPRIV_V2, which lets the kernel stop asking for
   * the security.capability xattr before every write. Only safe while files
   * in the mount never carry setuid, setgid or sticky bits.
   */
  ConfigSetting<bool> experimentalFuseHandleKillPrivV2{
      "experimental:fuse-handle-killpriv-v2",
      true,
      this};

  /**
   * Whether to use io_uring for FUSE request/reply transport instead of
   * traditional /dev/fuse read/write. Requires Linux 6.11+ with
   * CONFIG_FUSE_IO_URING=y. Falls back to /dev/fuse automatically if
   * the kernel doesn't support it.
   */
  ConfigSetting<bool> fuseUseIoUring{"fuse:use-io-uring", false, this};

  /**
   * How long a /dev/fuse worker thread polls for the next request after
   * answering one before blocking in read(). Zero disables polling. Read
   * when a mount starts; a change applies to mounts started afterwards.
   */
  ConfigSetting<std::chrono::nanoseconds> fuseBusyPoll{
      "fuse:busy-poll",
      std::chrono::microseconds(100),
      this};

  /**
   * Skip the eventfd wakeup when a FUSE io_uring reply is queued from the
   * ring worker that owns the queue: the worker drains pending commits
   * before its next submit_and_wait, so it does not need to be woken.
   */
  ConfigSetting<bool> experimentalFuseIoUringSkipSelfWakeup{
      "experimental:fuse-io-uring-skip-self-wakeup",
      true,
      this};

  /**
   * RE2 regex pattern matched against the Linux kernel release string
   * (`uname -r`) where Eden may negotiate FUSE io_uring when
   * fuse:use-io-uring is enabled. Defaults to fbk 6.13 kernels, where Eden's
   * graceful restart flow has been validated.
   */
  ConfigSetting<std::string> fuseIoUringKernelReleaseRegex{
      "fuse:io-uring-kernel-release-regex",
      "^6\\.13\\.",
      this};

  /**
   * Whether `eden restart --graceful` should fall back to a full restart when
   * the current FUSE transport config differs from the transport negotiated by
   * the running mounts.
   */
  ConfigSetting<bool> fuseRestartOnTransportMismatch{
      "fuse:restart-on-transport-mismatch",
      false,
      this};

  /**
   * The io_uring queue depth to use when the FUSE io_uring transport is
   * enabled. Match libfuse's current default.
   */
  ConfigSetting<uint32_t> fuseIoUringQueueDepth{
      "fuse:io-uring-queue-depth",
      8,
      this};

  /**
   * When the FUSE io_uring transport is enabled, pass IORING_ENTER_NO_IOWAIT so
   * that io_uring worker threads parked waiting for the next FUSE request are
   * not charged as iowait. Without this, idle-parked workers inflate
   * `/proc/stat` iowait and cgroup io.pressure (PSI) even when no real I/O is
   * outstanding. Requires a kernel with IORING_FEAT_NO_IOWAIT; otherwise it is
   * a no-op. Disabling iowait is all-or-nothing per ring, so genuine FUSE I/O
   * waits stop being accounted too.
   */
  ConfigSetting<bool> fuseIoUringDisableIoWait{
      "fuse:io-uring-disable-iowait",
      true,
      this};

  /**
   * When the FUSE io_uring transport is enabled, create every io_uring queue
   * and allocate its buffers before replying to FUSE_INIT, and mount over
   * /dev/fuse instead if they do not all fit.
   *
   * Eden creates one queue per logical CPU, and each ring charges
   * RLIMIT_MEMLOCK against a limit shared by every process running as this
   * uid, so a co-tenant can leave room for the first queues but not the rest.
   * Before the INIT reply is the only point where declining io_uring is still
   * possible: once the reply advertises FUSE_OVER_IO_URING the kernel blocks
   * request allocation until the ring is ready, so a later failure hangs the
   * mount rather than falling back.
   *
   * When disabled, each worker thread creates its own queue as it starts,
   * which keeps that queue's memory NUMA-local to the worker but leaves no way
   * to recover from a shortfall.
   */
  ConfigSetting<bool> fuseIoUringPreCreateQueues{
      "fuse:io-uring-pre-create-queues",
      false,
      this};

  // [nfs]

  /**
   * Whether EdenFS should unload NFS inodes. NFSv3 does not notify us when
   * file handles are closed. We have no definitive info from NFS on how many
   * open handles there are for already removed inodes.
   *
   * This enables background inode unloads to keep our inode memory and
   * disk usage bounded.
   */
  ConfigSetting<bool> unloadUnlinkedInodes{
      "nfs:unload-unlinked-inodes",
      false,
      this};

  ConfigSetting<std::chrono::nanoseconds> postCheckoutDelayToUnloadInodes{
      "nfs:post-checkout-inode-unloading-delay",
      std::chrono::seconds{10},
      this};

  /**
   * On macOS, ._ (AppleDouble) are sprinkled all over the place. Enabling this
   * allows these file to be created. When disabled, the AppleDouble files
   * won't be created.
   */
  ConfigSetting<bool> allowAppleDouble{"nfs:allow-apple-double", true, this};

  /**
   * ============== NFS MOUNT OPTIONS ==============
   *
   * See `man mount_nfs` for more information on these options.
   *
   * https://www.unix.com/man-page/osx/8/mount_nfs/
   */

  /**
   * The maximum time duration allowed for a NFS request. If a request exceeds
   * this amount of time, an NFS3ERR_JUKEBOX error will be returned to the
   * client to avoid blocking forever. NOTE: This is currently unimplemented.
   */
  ConfigSetting<std::chrono::nanoseconds> nfsRequestTimeout{
      "nfs:request-timeout",
      std::chrono::minutes(1),
      this};

  /**
   * Buffer size for read requests. Default to 16 KiB.
   *
   * This is both the client's rsize mount option and the read transfer size
   * EdenFS advertises in its FSINFO reply (rtmax/rtpref), which the client
   * clamps rsize to. It also drives the advertised dtpref, since READDIR
   * replies are reads.
   *
   * 16KiB was determined to offer the best tradeoff of random write speed to
   * streaming writes on macOS, use the benchmarks/random_writes.cpp before
   * changing this default value.
   */
  ConfigSetting<uint32_t> nfsReadIoSize{"nfs:read-iosize", 16 * 1024, this};

  /**
   * Buffer size for write requests. Default to 16 KiB.
   *
   * This is both the client's wsize mount option and the write transfer size
   * EdenFS advertises in its FSINFO reply (wtmax/wtpref), which the client
   * clamps wsize to.
   *
   * 16KiB was determined to offer the best tradeoff of random write speed to
   * streaming writes on macOS, use the benchmarks/random_writes.cpp before
   * changing this default value.
   */
  ConfigSetting<uint32_t> nfsWriteIoSize{"nfs:write-iosize", 16 * 1024, this};

  /**
   * Whether EdenFS NFS sockets should bind themself to unix sockets instead of
   * TCP ones.
   *
   * Unix sockets bypass the overhead of TCP and are thus significantly faster.
   * This is only supported on macOS.
   *
   * Note: Using UDS for binding is currently believed to be buggy. Reads would
   * randomly fail with some error due to a bug in the kernel not retrying
   * some internal error (like buffer being too small).
   */
  ConfigSetting<bool> useUnixSocket{"nfs:use-uds", false, this};

  /**
   * Whether the privhelper overrides the filesystem type that statfs reports
   * for an NFS mount served over a Unix domain socket to "edenfs:". The
   * override is a private fsctl that macOS 27 answers with a crash report on
   * every call. Mount table consumers now recognize these mounts by the name
   * of the nfsd socket, so this can be turned off once the Sapling and
   * watchman builds that know that name are deployed. Read at daemon start.
   */
  ConfigSetting<bool> nfsOverrideFstype{"nfs:override-fstype", true, this};

  /**
   * Send and receive buffer size of the nfsd socket when it is a Unix domain
   * socket (nfs:use-uds), 0 to keep the kernel default. Such a socket on
   * macOS defaults to 8 KiB each way, less than one READ reply, so every
   * large reply took a partial write, a wait for the client to drain, and a
   * second write. TCP sockets keep the kernel's buffers and autotuning.
   * Applied when the socket is accepted, so a change applies to mounts
   * started afterwards.
   */
  ConfigSetting<uint64_t> nfsSocketBufferSize{
      "nfs:socket-buffer-size",
      1024 * 1024,
      this};

  /**
   * ========== MACOS ONLY ==========
   *
   * Set the directory read size to the specified value. The value should
   * normally be a multiple of DIRBLKSIZ that is <= the read size for the mount.
   * The default is 8192 for UDP mounts and 32768 for TCP mounts.
   */
  ConfigSetting<std::optional<uint32_t>> nfsDirectoryReadSize{
      "nfs:dir-read-size",
      std::nullopt,
      this};

  /**
   * ========== MACOS ONLY ==========
   *
   * Set the maximum read-ahead count to the specified value. This may be in the
   * range of 0 - 128, and determines how many Read RPCs will be read ahead when
   * a large file is being read sequentially. Trying larger values for this is
   * suggested for mounts with a large bandwidth * delay product.
   */
  ConfigSetting<uint8_t> nfsReadAhead{"nfs:read-ahead", 16, this};

  /**
   * Set the initial retransmit timeout to the specified value. (Normally, the
   * dumbtimer option should be specified when using this option to manually
   * tune the timeout interval). The value is in tenths of a second.
   *
   * On macOS with dumbtimer enabled this is the fixed per-request timeout of
   * the kernel NFS client. For soft mounts the kernel gives up on a request
   * (ETIMEDOUT to the caller) after at most min(timeo, 30s) once EdenFS is
   * slow to answer, so 30s is the largest useful value there.
   */
  ConfigSetting<int32_t> nfsRetransmitTimeoutTenthSeconds{
      "nfs:retransmit-timeout-tenths",
      folly::kIsApple ? 300 : 10,
      this};

  /**
   * Set the retransmit timeout count for soft mounts to the specified value.
   */
  ConfigSetting<uint32_t> nfsRetransmitAttempts{
      "nfs:retransmit-attempts",
      10,
      this};

  /**
   * ========== MACOS ONLY ==========
   *
   * If the mount is still unresponsive X seconds after it is initially
   * reported unresponsive, then mark the mount as dead so that it will be
   * forcibly unmounted.  Note: mounts which are both soft and read-only will
   * also have the deadtimeout mount option set to 60 seconds.  This can be
   * explicitly overridden by setting deadtimeout=0.
   */
  ConfigSetting<int32_t> nfsDeadTimeoutSeconds{
      "nfs:dead-timeout-seconds",
      0,
      this};

  /**
   * ========== MACOS ONLY ==========
   *
   * Turn off the dynamic retransmit timeout estimator and use
   * nfs:retransmit-timeout-tenths as a fixed request timeout instead.
   *
   * The estimator tracks the smoothed RTT of EdenFS replies, which is
   * sub-millisecond for a local server, so the estimated timeout sits at its
   * ~80ms floor. Any request that EdenFS answers slowly then trips the timer
   * repeatedly, and on soft mounts that shrinks the kernel's give-up budget to
   * a few seconds (or ~1.4s on a busy mount, where 15 retries are burned in
   * 80ms steps). Enabled by default on macOS for that reason; nullopt leaves
   * the kernel default (estimator on).
   */
  ConfigSetting<std::optional<bool>> nfsDumbtimer{
      "nfs:dumbtimer",
      folly::kIsApple ? std::optional<bool>{true} : std::optional<bool>{},
      this};

  /**
   * When set to true, we will use readdirplus instead of readdir. Readdirplus
   * will be enabled for all nfs mounts. If set to false, regular readdir is
   * used instead.
   */
  ConfigSetting<bool> useReaddirplus{"nfs:use-readdirplus", false, this};

  /**
   * The number of threads per NFS mount that send directory invalidations
   * (chmods) to the kernel. Each chmod is a round trip through the kernel
   * back to EdenFS, so this bounds how fast inode GC can invalidate. GC
   * orders its own work through per-directory completion, not through the
   * queue, so more threads only let the chmods of unrelated directories
   * overlap; checkout's invalidations do not depend on the order either.
   * The default of one matches the serial executor this replaced; raising it
   * is believed safe for the reasons above but has not been verified under
   * load.
   */
  ConfigSetting<uint32_t> nfsNumInvalidationThreads{
      "nfs:num-invalidation-threads",
      1,
      this};

  /**
   * Upper bound on the number of directory invalidations inode GC may have
   * queued at once on an NFS mount. The queue is shared with checkout's
   * invalidations, so an unbounded GC queue would delay a checkout by however
   * many GC invalidations were ahead of it. When the bound is reached the GC
   * walk waits for the queue to drain before queuing more.
   */
  ConfigSetting<uint32_t> nfsMaxQueuedGcInvalidations{
      "nfs:max-queued-gc-invalidations",
      1024,
      this};

  /**
   * When set to true, NFS mounts are mounted with the "soft" mount option. This
   * setting applies to all NFS mounts. Behavior when set to false differs
   * between platforms:
   *
   * - macOS: Hard mount with INTR mount option is used.
   * - Linux: Hard mount is used (no INTR). Intr is unsupported after Linux
   *          kernel version 2.6.25.
   *
   * Note: setting to "true" does not turn off the "INTR" option on macOS.
   */
  ConfigSetting<bool> useSoftMounts{
      "nfs:use-soft-mounts",
      folly::kIsLinux ? true : false,
      this};

  /**
   * Whether to fast-path null and unimplemented NFS RPCs directly on the
   * EventBase thread, bypassing the thread pool. Prevents liveness probes
   * from blocking behind slow operations.
   */
  ConfigSetting<bool> nfsFastPathRPCs{"nfs:fast-path-rpcs", true, this};

  /**
   * Per-uid access policy, "uid:mode", e.g. ["0:log", "89:block"]. A match
   * bumps nfs.access.uid.<uid> and, per procedure,
   * nfs.access.uid.<uid>.<procedure>; "block" also rejects requests for
   * procedures in nfs:access-policy-procedures, bumping nfs.blocked.uid.<uid>
   * and nfs.blocked_access; "rate_limit" does so for those procedures only
   * past nfs:access-policy-rate-limit-*. Re-read on every request; AUTH_SYS
   * ids are client-asserted, so this sheds noisy processes rather than
   * enforcing a security boundary.
   */
  ConfigSetting<std::unordered_map<uint32_t, NfsAccessMode>> nfsUidAccessPolicy{
      "nfs:uid-access-policy",
      {{0, NfsAccessMode::Log}},
      this};

  /**
   * Same as nfs:uid-access-policy, keyed by gid: an entry matches a request
   * whose AUTH_SYS credential has that gid as its primary gid or among its
   * auxiliary gids. Evaluated independently of the uid entries, and every
   * matching entry of either map is counted.
   */
  ConfigSetting<std::unordered_map<uint32_t, NfsAccessMode>> nfsGidAccessPolicy{
      "nfs:gid-access-policy",
      {{0, NfsAccessMode::Log}},
      this};

  /**
   * The procedures that nfs:uid-access-policy / nfs:gid-access-policy act on,
   * as lowercase NFSv3 procedure names, e.g. ["readdir", "readdirplus"];
   * anything else, including other casing, is ignored. Only procedures in
   * this set are blocked, rate-limited, or consume rate-limit budget; every
   * other non-exempt request that matches an entry is still counted in
   * nfs.access.{uid,gid}.<id>, and the policed ones additionally in
   * nfs.policed.{uid,gid}.<id>. The control-plane procedures (NULL, FSSTAT,
   * FSINFO, PATHCONF) stay exempt whatever this set holds.
   */
  ConfigSetting<std::unordered_set<std::string>> nfsAccessPolicyProcedures{
      "nfs:access-policy-procedures",
      {"readdir", "readdirplus"},
      this};

  /**
   * For "rate_limit" entries in nfs:uid-access-policy / nfs:gid-access-policy:
   * the requests an id may make per
   * nfs:access-policy-rate-limit-window-seconds before further ones in that
   * window are rejected the way "block" rejects them. Budgets are per id and
   * per mount.
   */
  ConfigSetting<uint32_t> nfsAccessPolicyRateLimitCount{
      "nfs:access-policy-rate-limit-count",
      1000,
      this};

  /**
   * The window length, in seconds, for nfs:access-policy-rate-limit-count.
   */
  ConfigSetting<uint32_t> nfsAccessPolicyRateLimitWindowSeconds{
      "nfs:access-policy-rate-limit-window-seconds",
      60,
      this};

  /**
   * When true, an nfsd3 server refuses client connections beyond the single
   * kernel client it serves. EOF on any accepted connection is treated as
   * the mount being unmounted, so accepting extra connections lets an
   * unrelated local process tear the mount down by connecting to the
   * mount's loopback port and disconnecting again.
   */
  ConfigSetting<bool> nfsRefuseExtraClientConnections{
      "nfs:refuse-extra-client-connections",
      false,
      this};

  // [prjfs]

  /**
   * Enable ProjectedFS's negative path caching to reduce the number of
   * requests non existent files.
   * Only applicable on Windows
   */
  ConfigSetting<bool> prjfsUseNegativePathCaching{
      "prjfs:use-negative-path-caching",
      true,
      this};

  /**
   * Controls the number of threads per mount dedicated to running directory
   * invalidation.
   */
  ConfigSetting<uint8_t> prjfsNumInvalidationThreads{
      "prjfs:num-invalidation-threads",
      1,
      this};

  /**
   * Not sure if a Windows behavior, or a ProjectedFS one, but symlinks
   * aren't created atomically, they start their life as a directory, and
   * then a reparse tag is added to them to change them to a symlink. This
   * is an issue for EdenFS as the call to symlink_status above will race
   * with this 2 step process and thus may detect a symlink as a
   * directory...
   *
   * This is bad for EdenFS for a number of reason. The main one being that
   * EdenFS will attempt to recursively add all the children of that
   * directory to the inode hierarchy. If the symlinks points to a very
   * large directory, this can be extremely slow, leading to a very poor
   * user experience.
   *
   * How to solve this? Since notifications are handled completely
   * asynchronously, we can simply wait a bit and retry if the notification
   * has been received semi-recently. We still run the
   * risk of winning the race if the system is overloaded, but the
   * probability should be much lower.
   *
   * This config controls how long to wait after a directory creation
   * notification has been received.
   */
  ConfigSetting<std::chrono::nanoseconds> prjfsDirectoryCreationDelay{
      "prjfs:directory-creation-delay",
      std::chrono::milliseconds(100),
      this};

  /**
   * Listen to pre convert to full notifications from ProjFS. By the spec these
   * should not give us any information that the other notifications all ready
   * cover. However, ProjFS currently (Feb 2023) has a bug: we do not receive
   * the file closed and modified notification. We can listen to this instead
   * to ensure our in memory state reflects file truncations.
   */
  ConfigSetting<bool> prjfsListenToPreConvertToFull{
      "prjfs:listen-to-pre-convert-to-full",
      false,
      this};

  /**
   * With out this FSCK will not attempt to "fix" renamed files. This can
   * leave EdenFS out of sync with the filesystem. However this make FSCK
   * slower.
   */
  ConfigSetting<bool> prjfsFsckDetectRenames{
      "prjfs:fsck-detect-renames",
      true,
      this};

  /**
   * Controls how frequently we log to the EdenFS log file and scuba tables
   * about torn reads - i.e. when Prjfs attempts to read a file that was
   * modified in the middle of an operation.
   */
  ConfigSetting<std::chrono::nanoseconds> prjfsTornReadLogInterval{
      "prjfs:torn-read-log-interval",
      std::chrono::seconds{10},
      this};

  ConfigSetting<std::chrono::nanoseconds> tornReadCleanupDelay{
      "prjfs:torn-read-cleanup-delay",
      std::chrono::seconds{1},
      this};

  // [fschannel]

  /**
   * Number of threads that will service the background FS channel requests.
   */
  ConfigSetting<uint64_t> numFsChannelThreads{
      "fschannel:num-servicing-threads",
      folly::available_concurrency(),
      this};

  /**
   * Maximum number of pending FSChannel requests. When enforced, requests
   * beyond this limit receive backpressure: FUSE blocks until capacity is
   * available, NFS returns NFS3ERR_JUKEBOX causing the client to retry.
   * This value is also used as the threshold for logging high request counts.
   * When set to 0, no limit is enforced and no logging will occur.
   * Defaults to 10× CPU count on macOS (~100 on typical Macs), 0 (disabled)
   * on other platforms.
   */
  ConfigSetting<uint64_t> maxFsChannelInflightRequests{
      "fschannel:max-inflight-requests",
      folly::kIsApple ? folly::available_concurrency() * 10 : 0,
      this};

  ConfigSetting<std::chrono::nanoseconds> highFsRequestsLogInterval{
      "fschannel:high-fs-requests-log-interval",
      std::chrono::minutes{30},
      this};

  // [hg]

  /**
   * Controls whether Eden enforces parent commits in a hg status
   * (getScmStatusV2) call
   */
  ConfigSetting<bool> enforceParents{"hg:enforce-parents", true, this};

  /**
   * Controls whether EdenFS will attempt to fetch aux metadata or always
   * fallback to fetching blobs when the metadata is not present in local
   * caches.
   */
  ConfigSetting<bool> fetchHgAuxMetadata{"hg:fetch-aux-metadata", true, this};

  /**
   * Which object ID format should the SaplingBackingStore use?
   */
  ConfigSetting<HgObjectIdFormat> hgObjectIdFormat{
      "hg:object-id-format",
      HgObjectIdFormat::WithPath,
      this};

  /**
   * Controls the number of blob or prefetch import requests we batch in
   * SaplingBackingStore
   */
  ConfigSetting<uint32_t> importBatchSize{"hg:import-batch-size", 1, this};

  /**
   * Controls the number of tree import requests we batch in SaplingBackingStore
   */
  ConfigSetting<uint32_t> importBatchSizeTree{
      "hg:import-batch-size-tree",
      1,
      this};

  /**
   * Controls the max number of blob aux data import requests we batch in
   * SaplingBackingStore
   */
  ConfigSetting<uint32_t> importBatchSizeBlobMeta{
      "hg:import-batch-size-blobmeta",
      1024,
      this};

  /**
   * Controls the max number of tree aux data import requests we batch in
   * SaplingBackingStore
   */
  ConfigSetting<uint32_t> importBatchSizeTreeMeta{
      "hg:import-batch-size-treemeta",
      1024,
      this};

  ConfigSetting<uint32_t> hgActivityBufferSize{
      "hg:activity-buffer-size",
      100,
      this};

  /**
   * Should we use the cached `sl status` results or not
   */
  ConfigSetting<bool> hgEnableCachedResultForStatusRequest{
      "hg:enable-scm-status-cache",
      folly::kIsWindows ? false : true,
      this};

  /**
   *  The maximum size of SCM status cache in bytes:
   *  1. Generally, a file path is about 50 chars long.
   *  2. We only cache status when the number of diff files is less than 10k.
   *  3. And we allow at most 5 such "huge" status
   */
  ConfigSetting<size_t> scmStatusCacheMaxSize{
      "hg:scm-status-cache-max-size",
      50 * 10 * 1024 * 5, // 2.5 MB
      this};

  /**
   *  The minimum number of items to keep in SCM status cache
   */
  ConfigSetting<size_t> scmStatusCacheMinimumItems{
      "hg:scm-status-cache-min-items",
      3,
      this};

  /**
   *  The maximum number of entries we want to cache within a single status
   */
  ConfigSetting<size_t> scmStatusCacheMaxEntriesPerItem{
      // @lint-ignore SPELL
      "hg:scm-status-cache-max-entires-per-item",
      10000,
      this};

  // [backingstore]

  /**
   * Number of threads that will pull backingstore requests off the queue.
   */
  ConfigSetting<uint8_t> numBackingstoreThreads{
      "backingstore:num-servicing-threads",
      32,
      this};

  /**
   * Control walk detection behavior:
   * - "" means defer to sapling config (backingstore.walk-mode)
   * - "off" means do nothing
   * - "monitor" means track walk behavior but take no action
   * - "prefetch" means track walk behavior and prefetch automatically
   */
  ConfigSetting<std::string> backingstoreWalkMode{
      "backingstore:walk-mode",
      "",
      this};

  /**
   * Whether a mount gets a walk detector at all. Off means the mount creates
   * none, so its reads are never reported to the backing store's walk
   * detection, whatever `backingstore:walk-mode` says. Read when the mount is
   * created.
   */
  ConfigSetting<bool> enableWalkDetector{
      "experimental:enable-walk-detector",
      true,
      this};

  /**
   * Whether a file load tells the walk detector how many entries the file's
   * directory had when its inode was loaded, so small and large directories
   * are recognized even when their trees were already cached.
   */
  ConfigSetting<bool> walkDirSizeHints{
      "experimental:walk-dir-size-hints",
      true,
      this};

  // [telemetry]

  /**
   * Whether the daemon sends samples to Scribe at all via the XplatLogger.
   * Checked on every sample, so it can be flipped in emergency situations. Off
   * by default in debug builds to avoid pollution but that means it needs to be
   * enabled for testing telemetry related code changes.
   */
  ConfigSetting<bool> enableScribeLogging{
      "telemetry:enable-scribe-logging",
      !folly::kIsDebug,
      this};

  /**
   * Location of scribe_cat binary on the system. If not specified, scribe
   * logging will be disabled.
   */
  ConfigSetting<std::string> scribeLogger{"telemetry:scribe-cat", "", this};

  /**
   * Scribe category is the first argument passed to the scribe_cat binary.
   */
  ConfigSetting<std::string> scribeCategory{
      "telemetry:scribe-category",
      "",
      this};

  /**
   * Scribe category is the first argument passed to the scribe_cat binary. This
   * is used by the NotificationsStructuredLogger
   */
  ConfigSetting<std::string> notificationsScribeCategory{
      "telemetry:notifications-scribe-category",
      "",
      this};

  /**
   * Kill switch for the entire structured error logging feature.
   */
  ConfigSetting<bool> enableErrorLogging{
      "telemetry:enable-error-logging",
      false,
      this};

  /**
   * Maximum number of error events per minute, per `error_type`, that
   * `ErrorLogger` admits on this host. Errors above this rate are dropped and
   * counted; the count is attached to the next admitted event of the same type
   * as `suppressed_count` in the `extras` column. 0 disables rate limiting.
   *
   * The limit is applied before `enableErrorLogging`, so callers that gate
   * their local log line on the returned outcome are protected even when
   * Scuba logging is off.
   */
  ConfigSetting<uint32_t> errorLogMaxPerMinute{
      "telemetry:error-log-max-per-minute",
      6,
      this};

  /**
   * Number of error events of a single `error_type` admitted immediately
   * before `errorLogMaxPerMinute` starts to apply.
   */
  ConfigSetting<uint32_t> errorLogBurst{"telemetry:error-log-burst", 10, this};

  /**
   * Whether to upload stack traces to Manifold when logging errors.
   */
  ConfigSetting<bool> enableStackTraceUpload{
      "telemetry:enable-stack-trace-upload",
      false,
      this};

  /**
   * Whether to attribute the processes accessing a mount to whatever launched
   * them (ProcessInfoCache::ReadFuncConfig::attribution). The attribution is
   * attached to per-process events (currently FetchHeavy) as opaque string
   * fields so the activity is credited to the client rather than to the
   * environment EdenFS itself was started under. Read at startup.
   */
  ConfigSetting<bool> attributeClientProcesses{
      "telemetry:attribute-client-processes",
      true,
      this};

  /**
   * Controls which paths eden will log data fetches for when this is set.
   * Fetches for any paths that match the regex will be logged.
   */
  ConfigSetting<std::optional<std::shared_ptr<RE2>>> logObjectFetchPathRegex{
      "telemetry:log-object-fetch-path-regex",
      std::nullopt,
      this};

  /**
   * Minimum interval between NFS stats updates in seconds. NFS stat collection
   * only happens on macOS. Change this to 0 to disable NFS stat collection.
   */
  ConfigSetting<std::chrono::nanoseconds> updateNFSStatsInterval{
      "telemetry:update-nfs-stats-interval",
      std::chrono::seconds{0},
      this};

  /**
   * Controls the capacity of the internal buffer for NFS Tracebus.
   */
  ConfigSetting<size_t> nfsTraceBusCapacity{
      "telemetry:nfs-tracebus-capacity",
      25000,
      this};

  /**
   * Controls whether, after a silent daemon exit, system logs are checked
   * to determine if the exit was caused by memory pressure. On macOS, this
   * runs "log show"; on Linux, this runs "dmesg". The check is
   * time-consuming, so it is executed asynchronously and does not block
   * EdenFS startup.
   */
  ConfigSetting<bool> silentDaemonExitLogShow{
      "telemetry:silent-daemon-exit-log-show",
      false,
      this};

  ConfigSetting<size_t> HgTraceBusCapacity{
      "telemetry:hg-tracebus-capacity",
      100000,
      this};

  ConfigSetting<size_t> InodeTraceBusCapacity{
      "telemetry:inode-tracebus-capacity",
      25000,
      this};

  ConfigSetting<size_t> ThriftTraceBusCapacity{
      "telemetry:thrift-tracebus-capacity",
      25000,
      this};

  ConfigSetting<size_t> FuseTraceBusCapacity{
      "telemetry:fuse-tracebus-capacity",
      25000,
      this};

  ConfigSetting<size_t> PrjfsTraceBusCapacity{
      "telemetry:prjfs-tracebus-capacity",
      25000,
      this};

  /**
   * Controls whether EdenFS logs inode state changes to Tracebus or not.
   */
  ConfigSetting<bool> enableInodeTraceBus{
      "telemetry:enable-inodetracebus",
      true,
      this};

  /**
   * Controls whether EdenFS makes use of ActivityBuffers to store past
   * events in memory.
   */
  ConfigSetting<bool> enableActivityBuffer{
      "telemetry:enable-activitybuffer",
      true,
      this};

  /**
   * Sets the maximum number of events an ActivityBuffer can store before
   * evicting old events
   */
  ConfigSetting<uint32_t> activityBufferMaxEvents{
      "telemetry:activitybuffer-max-events",
      100,
      this};

  // TODO: Understand why long running requests are hit so frequently on Windows
  // hosts. For now, disable the config on Windows because too many Scuba
  // samples are generated.
  ConfigSetting<std::chrono::nanoseconds> longRunningFSRequestThreshold{
      "telemetry:long-running-fs-request-threshold",
      folly::kIsWindows ? std::chrono::nanoseconds(0)
                        : std::chrono::seconds{45},
      this};

  // [experimental]

  /**
   * Controls whether EdenFS exports itself as an NFS server.
   *
   * A graceful restart hands NFS mounts over whatever this setting says in
   * the new daemon, and the new daemon then runs an NFS server for them until
   * the next full restart. New mounts follow this setting.
   */
  ConfigSetting<bool> enableNfsServer{
      "experimental:enable-nfs-server",
      folly::kIsApple,
      this};

  /**
   * Specify the interval of updating eden heartbeat file. The heartbeat file
   * should have the latest timestamp. If eden crash with a SIGKILL, the
   * timestamp of the heartbeat file can be used to determine approximate time
   * of the crash.
   * Zero means disable the updating feature.
   */
  ConfigSetting<std::chrono::nanoseconds> updateEdenHeartbeatFileInterval{
      "experimental:update-eden-heartbeat-file-interval",
      std::chrono::minutes(15),
      this};

  /**
   * Controls whether EdenFS will periodically garbage collect the working
   * directory.
   *
   * For now, periodic GC only makes sense on Windows. On macOS, it is
   * currently in a limited dogfooding phase and its behavior is not
   * yet fully defined. Behavior on Linux is unknown.
   */
  ConfigSetting<bool> enableGc{
      "experimental:enable-garbage-collection",
      folly::kIsWindows,
      this};

  /**
   * The interval for background periodic unloading of inodes from inodeMap.
   * 0 means periodic unloading is disabled.
   *
   * Note 1: Periodic inode unloading and Garbage Collection (GC) are mutually
   * exclusive. EdenFS skips periodic unloading when pressure-based GC is
   * enabled.
   * Note 2: Periodic inode unloading is no-op on Windows.
   */
  ConfigSetting<uint32_t> periodicUnloadIntervalMinutes{
      "experimental:periodic-inode-map-unload-interval",
      0,
      this};

  /**
   * Controls whether EdenFS eagerly invalidates directories during checkout or
   * whether it only does when children were modified.
   *
   * On Windows, this must be set to true for correctness reasons.
   */
  ConfigSetting<bool> alwaysInvalidateDirectory{
      "experimental:always-invalidate-directories",
      folly::kIsWindows,
      this};

  /**
   * Enables early exit cancellation by throwing, for cancellation supported
   * endpoints.
   */
  ConfigSetting<bool> throwOnCancel{
      "experimental:throw-on-cancel",
      false,
      this};

  /**
   * Controls whether EdenFS uses coroutines for debugGetBlob thrift endpoint.
   * This allows for gradual rollout of coroutine-based implementation.
   * Default is false for conservative rollout strategy.
   */
  ConfigSetting<bool> enableCoroutinesInDebugGetBlob{
      "experimental:enable-coroutines-debug-get-blob",
      false,
      this};

  /**
   * When true, checkout removes stale overlay directory data in the background
   * GC thread instead of synchronously on the checkout thread.
   */
  ConfigSetting<bool> backgroundOverlayCleanupDuringCheckout{
      "experimental:background-overlay-cleanup-during-checkout",
      true,
      this};

  /**
   * When true, a forced checkout that replaces a directory with a file removes
   * the local-only files inside it so the replacement can succeed. When false,
   * the directory is left in place and reported as DIRECTORY_NOT_EMPTY.
   */
  ConfigSetting<bool> forceCheckoutRemovesLocalOnly{
      "experimental:force-checkout-removes-local-only",
      true,
      this};

  /**
   * Master gate for pressure-based inode GC on FUSE.
   * When enabled, FUSE TTLs and GC cutoffs are dynamically computed based
   * on total inode count.
   */
  ConfigSetting<bool> enablePressureBasedGc{
      "experimental:enable-pressure-based-gc",
      false,
      this};

  /**
   * Whether NFS inode GC answers its own invalidation chmod with a stale
   * handle error, which makes the macOS client drop the directory's cached
   * names at once, and forgets the directory's children at that moment. When
   * disabled, the chmod is answered normally and the children are forgotten
   * once it has succeeded, as before the stale reply; the client then keeps
   * the names it has cached until a stale file handle sends it back to
   * EdenFS. A fallback for the stale reply misbehaving, not a mode to run in.
   */
  ConfigSetting<bool> nfsGcStaleReply{
      "experimental:nfs-gc-stale-reply",
      true,
      this};

  /**
   * Whether to use systemd for EdenFS lifecycle management
   * (start/stop/restart).
   */
  ConfigSetting<bool> systemdManagedLifecycle{
      "experimental:systemd-managed-lifecycle",
      false,
      this};

  /**
   * How long systemd should extend EdenFS startup timeout on each startup
   * progress event. Set to 0 to disable startup timeout extension.
   */
  ConfigSetting<std::chrono::nanoseconds> systemdStartupTimeoutExtension{
      "systemd:startup-timeout-extension",
      std::chrono::minutes(5),
      this};

  /**
   * Whether to place EdenFS in a dedicated systemd cgroup via systemd-run.
   */
  ConfigSetting<bool> systemdCgroupIsolation{
      "experimental:systemd-cgroup-isolation",
      false,
      this};

  // [coroutines]

  /**
   * Master switch to enable/disable all coroutine-based implementations.
   * When false, all coroutine implementations are disabled regardless of
   * individual feature flags. When true, individual feature flags control
   * each endpoint.
   * Default is true to allow individual feature flags to work.
   */
  ConfigSetting<bool> enableCoroutines{"coroutines:enabled", true, this};

  /**
   * Controls whether EdenFS uses phase 7 coroutine implementations
   * (the checkOutRevision thrift endpoint).
   */
  ConfigSetting<bool> enableCoroutinesPhase7{
      "coroutines:enable-phase7",
      false,
      this};

  // [blobcache]

  /**
   * Controls whether if EdenFS caches blobs in memory.
   */
  ConfigSetting<bool> enableInMemoryBlobCaching{
      "blobcache:enable-in-memory-blob-caching",
      true,
      this};

  /**
   * How many bytes worth of blobs to keep in memory, at most.
   */
  ConfigSetting<size_t> inMemoryBlobCacheSize{
      "blobcache:cache-size",
      40 * 1024 * 1024,
      this};

  /**
   * The minimum number of recent blobs to keep cached. Trumps
   * maximumBlobCacheSize.
   */
  ConfigSetting<size_t> inMemoryBlobCacheMinimumItems{
      "blobcache:minimum-items",
      16,
      this};

  // [treecache]

  /**
   * Controls whether if EdenFS caches trees in memory.
   */
  ConfigSetting<bool> enableInMemoryTreeCaching{
      "treecache:enable-in-memory-tree-caching",
      true,
      this};

  /**
   * Number of bytes worth of data to keep in memory. The budget must exceed
   * the largest tree by a comfortable margin, or a repeated walk of a working
   * set just larger than the budget misses on every lookup.
   */
  ConfigSetting<size_t> inMemoryTreeCacheSize{
      "treecache:cache-size",
      128 * 1024 * 1024,
      this};

  /**
   * The minimum number of recent tree to keep cached. Trumps
   * inMemoryTreeCacheSize.
   */
  ConfigSetting<size_t> inMemoryTreeCacheMinimumItems{
      "treecache:minimum-items",
      16,
      this};

  /**
   * Number of shards for the tree cache. Higher number means lower lock
   * contention, but the byte budget and minimum-items floor are split per
   * shard, so a tree larger than its shard's slice cannot stay cached.
   */
  ConfigSetting<uint64_t> treeCacheShards{"treecache:shards", 1, this};

  /**
   * Whether glob and prefetch requests bypass the in-memory tree cache and
   * the in-memory tree aux data cache. Those requests walk far more trees than
   * the caches can hold, so caching their trees only evicts what other callers
   * have warm.
   */
  ConfigSetting<bool> treeCacheBypassGlobAndPrefetch{
      "treecache:bypass-glob-and-prefetch",
      true,
      this};

  // [notifications]

  /**
   * A command to run to warn the user of a generic problem encountered
   * while trying to process a request.
   * The command is executed by the shell.
   * If blank, no command will be run.
   */
  ConfigSetting<std::string> genericErrorNotificationCommand{
      "notifications:generic-connectivity-notification-cmd",
      "",
      this};

  /**
   * Don't show a notification more often than once in the specified interval
   */
  ConfigSetting<std::chrono::nanoseconds> notificationInterval{
      "notifications:interval",
      std::chrono::minutes(1),
      this};

  /**
   * Whether the E-Menu should be created when the EdenFS daemon is started
   */
  ConfigSetting<bool> enableEdenMenu{
      "notifications:enable-eden-menu",
      true,
      this};

  /**
   * Whether notifications are completely disabled
   */
  ConfigSetting<bool> enableNotifications{
      "notifications:enable-notifications",
      true,
      this};

  /**
   * Whether the debug menu is shown in the Eden Menu
   */
  ConfigSetting<bool> enableEdenDebugMenu{
      "notifications:enable-debug",
      true,
      this};

  /**
   * Whether health report notifications should be shown via Windows
   * notifications. This is only used in the CLI, it is included here to
   * get rid of warnings.
   */
  ConfigSetting<bool> notifyHealthReportIssues{
      "notifications:notify-health-report-issues",
      false,
      this};

  /**
   * The age threshold that the health-report command should utilize to check if
   * the running EdenFS version is stale.
   */
  ConfigSetting<size_t> healthReportStaleVersionThresholdDays{
      "notifications:health-report-stale-version-threshold-days",
      45,
      this};

  /**
   * Whether EdenFS ready status should be shown via Windows
   * notifications
   */
  ConfigSetting<bool> notifyEdenReady{
      "notifications:notify-eden-ready",
      false,
      this};

  // [prefetch-profiles]

  /**
   * Kill switch for the prefetch profiles feature.
   */
  ConfigSetting<bool> enablePrefetchProfiles{
      "prefetch-profiles:prefetching-enabled",
      true,
      this};

  /**
   * Used to control file access logging.
   */
  ConfigSetting<bool> logFileAccesses{
      "prefetch-profiles:file-access-logging-enabled",
      false,
      this};

  /**
   * A number from 0 - x to determine how often we should log file access
   * events. This is currently agnostic to the type of file access. If this
   * is not at 100%, we will not log filenames and we will only log directory
   * paths. In the following equation, 1/x = percentage, x is this variable.
   * For 50% rollout, 1/x = .5, so x = 2, so this would be set to 2. 0
   * indicates that the feature is off.
   */
  ConfigSetting<uint32_t> logFileAccessesSamplingDenominator{
      "prefetch-profiles:file-access-logging-sampling-denominator",
      0,
      this};

  // [redirections]

  /**
   * Whether to use symlinks, APFS volumes, or disk images for bind
   * redirections on macOS.
   */
  ConfigSetting<std::string> darwinRedirectionType{
      "redirections:darwin-redirection-type",
      "apfs",
      this};

  // [overlay]

  /**
   * The `InodeCatalogType` to use when creating new `Overlay`s, unless
   * specified by parameter during creation via CLI or otherwise.
   */
  ConfigSetting<InodeCatalogType> inodeCatalogType{
      "overlay:inode-catalog-type",
      kInodeCatalogTypeDefault,
      this};

  /**
   * Number of OverlayFile and metadata cached in memory.
   */
  ConfigSetting<size_t> overlayFileAccessCacheSize{
      "overlay:file-access-cache-size",
      100,
      this};

  /**
   * Remove the overlay data of unlinked inodes on the overlay GC thread once
   * the kernel forgets them, instead of on the FsChannel thread handling the
   * forget.
   */
  ConfigSetting<bool> overlayBackgroundInodeRemoval{
      "overlay:background-inode-removal",
      true,
      this};

  /**
   * When true, write a newly created overlay file directly instead of
   * creating a temporary file and renaming it into place. A torn write
   * leaves a partial file, which is safe here because an inode is only
   * recorded as materialized after its contents are written: on crash the
   * partial file is an fsck orphan and the contents still come from source
   * control. Covers file inodes only; non-materialized directory records are
   * always written directly.
   */
  ConfigSetting<bool> experimentalOverlayDirectFileCreate{
      "experimental:overlay-direct-file-create",
      true,
      this};

  /**
   * Keep overlay WAL files open in a small cache instead of opening and
   * closing one per append, and track their size in memory rather than
   * with an lseek per append.
   */
  ConfigSetting<bool> experimentalOverlayCacheWalFiles{
      "experimental:overlay-cache-wal-files",
      true,
      this};

  /**
   * How many directories' WAL files to keep open at once when
   * experimental:overlay-cache-wal-files is on. A directory not in the
   * cache pays an open and a close for its next append.
   */
  ConfigSetting<size_t> overlayWalFileCacheSize{
      "overlay:wal-file-cache-size",
      64,
      this};

  /**
   * Floor on the WAL compaction probability denominator: a directory's WAL
   * is compacted with probability 1/max(this, multiplier * entries), so
   * small directories are compacted far less often. 0 restores the
   * previous 1/(multiplier * max(entries, 10)) behavior.
   *
   * A compaction rewrites the directory's record through a temporary file
   * and a rename on the request path, under the parent's contents lock, so
   * it should stay rare for directories that see steady churn; replaying a
   * hundred WAL entries when the directory is next loaded costs well under a
   * millisecond.
   */
  ConfigSetting<uint64_t> experimentalOverlayWalMinCompactionThreshold{
      "experimental:overlay-wal-min-compaction-threshold",
      100,
      this};

  /**
   * Require the renamed entry in the destination snapshot before writing.
   * Disabling this guard restores the legacy full rewrite, which can lose the
   * moved child when the snapshots omit it.
   */
  ConfigSetting<bool> experimentalOverlayRenameRequireDestination{
      "experimental:overlay-rename-require-destination",
      true,
      this};

  /**
   * Keep the file descriptor from creating a new overlay file in the open
   * file cache, instead of closing it and reopening the file on the first
   * write.
   */
  ConfigSetting<bool> experimentalOverlayReuseCreatedFds{
      "experimental:overlay-reuse-created-fds",
      true,
      this};

  /**
   * Determines if EdenFS should use Write-Ahead Logging (WAL) for overlay
   * directory writes. Only applies to the Legacy catalog type.
   */
  ConfigSetting<bool> overlayUseWal{"overlay:use-wal", true, this};

  /**
   * Write new overlay WAL files in the versioned format. Existing WAL files
   * are always read according to their on-disk format. Snapshot at mount
   * creation; disabling this setting does not disable versioned WAL reads.
   *
   * Rollout invariant: only enable once every EdenFS binary that could run
   * against the overlay can read versioned WALs. A binary that predates the
   * reader sees the `OVWL` header as a torn v1 frame, reports a parse error,
   * and heals the directory by rewriting its base without the WAL, which
   * silently drops the pending mutations. Turning this flag back off does
   * not rewrite existing versioned WALs.
   */
  ConfigSetting<bool> overlayWalWriteV2{"overlay:wal-write-v2", false, this};

  /**
   * Persist an upper bound on allocated inode numbers so an unclean startup
   * can skip startup fsck when online-fsck is enabled and the reservation is
   * available. Reservation applies to the Legacy catalog type.
   */
  ConfigSetting<bool> overlayInodeReservation{
      "overlay:inode-reservation",
      true,
      this};

  /**
   * Whether mmap accesses to the inode metadata table should recover from
   * synchronous SIGBUS faults. Snapshot at Overlay initialization.
   */
  ConfigSetting<bool> overlayUseSigbusProtection{
      "overlay:use-sigbus-protection",
      false,
      this};

  /**
   * Validate and recover overlay data on access instead of running startup
   * fsck. Requires inode reservation and the Legacy catalog. Unvisited
   * orphan subtrees are left for offline fsck. Disabled by default.
   * See eden/fs/docs/OnlineFsck.md for recovery policy and trade-offs.
   */
  ConfigSetting<bool> overlayOnlineFsck{"overlay:online-fsck", false, this};

  /**
   * Multiplier applied to a directory's base size when computing the
   * inline-compaction threshold. A compaction is triggered when the WAL
   * entry count for a parent exceeds `multiplier * max(baseSize, 10)`,
   * capped by `overlay:wal-compaction-cap`. Larger values let the WAL
   * absorb more appends before paying the rewrite cost; smaller values
   * keep load-time replay shorter at the cost of more frequent
   * compactions. A multiplier of 0 compacts on every append. Snapshot at
   * Overlay construction.
   */
  ConfigSetting<size_t> overlayWalCompactionMultiplier{
      "overlay:wal-compaction-multiplier",
      3,
      this};

  /**
   * Hard upper bound on the inline-compaction WAL byte size, regardless
   * of directory size. Caps the worst-case work the inline compaction
   * has to do under the parent `TreeInode::contents_` lock. Snapshot at
   * Overlay construction.
   *
   * Default 5 MB ≈ 100k typical WAL entries (each entry is ~50 bytes),
   * which benchmarks at ~225 ms total replay + compact time inline.
   */
  ConfigSetting<uint64_t> overlayWalCompactionByteCap{
      "overlay:wal-compaction-byte-cap",
      5'000'000,
      this};

  /**
   * Number of overlay files to pre-create for future file inodes so that
   * creating a new file requires no filesystem syscalls on the request
   * path. 0 disables preallocation. Only supported by the legacy
   * (file-based) overlay. Read when a checkout's overlay is opened, so a
   * change applies to mounts started afterwards.
   */
  ConfigSetting<uint64_t> overlayFilePreallocPoolSize{
      "overlay:file-prealloc-pool-size",
      64,
      this};

  /**
   * Number of empty overlay directory records to pre-create for future
   * directory inodes so that mkdir does not write the new child's overlay
   * record on the request path. 0 disables preallocation. Only supported by
   * the legacy (file-based) overlay. Read when a checkout's overlay is
   * opened, so a change applies to mounts started afterwards.
   */
  ConfigSetting<uint64_t> overlayDirPreallocPoolSize{
      "overlay:dir-prealloc-pool-size",
      64,
      this};

  // [clone]

  /**
   * Controls the mount protocol that `eden clone` will default to.
   */
  ConfigSetting<MountProtocol> defaultCloneMountProtocol{
      "clone:default-mount-protocol",
      folly::kIsWindows ? MountProtocol::PRJFS : MountProtocol::FUSE,
      this};

  /**
   * Controls whether clone sets up the working copy's ".hg" repo dir as a
   * symlink into the state dir ("off mount").
   */
  ConfigSetting<bool> offMountRepoDir{"clone:off-mount-repo-dir", false, this};

  /**
   * Controls the timeout (in seconds) that is set when calling the Thrift mount
   * endpoint from the CLI during clone. A value of 0 means no timeout.
   */
  ConfigSetting<size_t> cloneMountTimeout{"clone:mount-timeout", 30, this};

  /**
   * Maximum number of Eden checkouts allowed on this host. When the count of
   * existing checkouts reaches this limit, `eden clone` will fail with a clear
   * error message. A value of 0 (the default) means unlimited.
   */
  ConfigSetting<uint32_t> maxClones{"clone:max-clones", 0, this};

  // [fsck]

  /**
   * When FSCK is running, how often should we log about the number of scanned
   * directories. This is the number of directories that are scanned in between
   * logs.
   */
  ConfigSetting<uint64_t> fsckLogFrequency{"fsck:log-frequency", 10000, this};

  /**
   * Should FSCK be run on multiple threads, or serialized. This option is
   * specific to Windows.
   */
  ConfigSetting<bool> multiThreadedFsck{"fsck:multi-threaded", true, this};

  /**
   * The number of threads that the OverlayChecker will use per parallel fsck
   * stage. Set to 0 to pick an I/O-oriented default.
   */
  ConfigSetting<uint64_t> fsckNumErrorDiscoveryThreads{
      "fsck:num-error-discovery-threads",
      0,
      this};

  /**
   * Use fsck's lower-memory scan path. This avoids retaining every parsed
   * directory map for the full fsck run.
   */
  ConfigSetting<bool> fsckUseMemoryEfficientScan{
      "fsck:use-memory-efficient-scan",
      true,
      this};

  /**
   * Maximum number of mounts that can run fsck concurrently during startup.
   * Limits peak memory usage after ungraceful shutdown to N times a single
   * mount's fsck. Set to 0 for unlimited concurrency (no semaphore).
   */
  ConfigSetting<uint32_t> fsckMaxConcurrentMounts{
      "fsck:max-concurrent-mounts",
      1,
      this};

  // [glob]

  /**
   * Whether glob requests should use the mount's case sensitivity setting (i.e.
   * interpret patterns as case-insensitive on case-insensitive systems). If
   * false, globs are always case-sensitive.
   * TODO: Remove this killswitch after the feature proves to be safe.
   */
  ConfigSetting<bool> globUseMountCaseSensitivity{
      "glob:use-mount-case-sensitivity",
      true,
      this};

  /**
   * When true, maintain previous behavior where we sort/dedupe list of file ids
   * when prefetching.
   */
  ConfigSetting<bool> globDedupePrefetchFiles{
      "glob:dedupe-prefetch-files",
      false,
      this};

  /**
   * The number of recursive glob levels that should always use async execution
   * through the folly executor. This prevents excessive synchronous recursion
   * in glob operations and improves concurrency.
   */
  ConfigSetting<uint32_t> globRecursiveAsyncDepth{
      "glob:recursive-async-depth",
      3,
      this};

  /**
   * Whether GlobMatcher memoizes failed backtracking states. Disabling this is
   * an emergency rollback mechanism; the backtracking step limit remains
   * active to keep pathological matches bounded.
   */
  ConfigSetting<bool> globEnableFailureMemoization{
      "glob:enable-failure-memoization",
      true,
      this};

  /**
   * Maximum number of failed backtracking states retained by one match.
   * Matching continues without retaining new states after reaching this cap.
   */
  ConfigSetting<size_t> globMaxMemoizedFailureStates{
      "glob:max-memoized-failure-states",
      65'536,
      this};

  /** Maximum number of recursive backtracking attempts made by one match. */
  ConfigSetting<size_t> globMaxBacktrackingSteps{
      "glob:max-backtracking-steps",
      100'000,
      this};

  // [doctor]

  /**
   * Class names of doctor problems that should not be reported to the user or
   * automatically fixed.
   */
  ConfigSetting<std::vector<std::string>> doctorIgnoredProblemClassNames{
      "doctor:ignored-problem-class-names",
      {},
      this};

  /**
   * The minimum kernel version required for EdenFS to work correctly.
   */
  ConfigSetting<std::string> doctorMinimumKernelVersion{
      "doctor:minimum-kernel-version",
      "5.12",
      this};

  /**
   * Known bad kernel versions for which we should print a warning in `edenfsctl
   * doctor`.
   */
  ConfigSetting<std::string> doctorKnownBadKernelVersions{
      "doctor:known-bad-kernel-versions",
      "TODO,TEST",
      this};

  /**
   * Known bad edenfs versions for which we should print a warning in `edenfsctl
   * doctor`. Currently not used in Daemon.
   * Format:
   * [<bad_version_1>|<sev_1(optional):reason_1>,<bad_version_2>|<sev_2(optional):reason_2>]
   */
  ConfigSetting<std::vector<std::string>> doctorKnownBadEdenFsVersions{
      "doctor:known-bad-edenfs-versions",
      {},
      this};

  /**
   * Extensions that may do bad things that we want to warn about in
   * doctor.
   */
  ConfigSetting<std::vector<std::string>> doctorExtensionWarnList{
      "doctor:vscode-extensions-warn-list",
      {},
      this};

  /**
   * Extensions that will do bad things that we definitely want to advise
   * against using.
   */
  ConfigSetting<std::vector<std::string>> doctorExtensionBlockList{
      "doctor:vscode-extensions-block-list",
      {},
      this};

  /**
   * Extensions that we know are fine and should not be warned against.
   */
  ConfigSetting<std::vector<std::string>> doctorExtensionAllowList{
      "doctor:vscode-extensions-allow-list",
      {},
      this};

  /**
   * Extensions authors that are known and we should not warn about.
   */
  ConfigSetting<std::vector<std::string>> doctorExtensionAuthorAllowList{
      "doctor:vscode-extensions-author-allow-list",
      {},
      this};

  // [rage]
  /**
   * The tool that will be used to upload eden rages. Only currently used in the
   * CLI.
   */
  ConfigSetting<std::string> rageReporter{"rage:reporter", "", this};

  // [hash]
  /**
   * The key to use for blake3 hash computation.
   * The key must be exactly 32 bytes.
   *
   * !!!IMPORTANT!!!
   * The value of this config must be kept in sync with the source control
   * server as well as all the tools that will compute a Blake3 hash from file
   * content. To this effect, this config is overwritten early at startup in
   * Meta's environment to prevent any mismatch.
   */
  ConfigSetting<std::optional<std::string>> blake3Key{
      "hash:blake3-key",
      std::nullopt,
      this};

  // [notify]
  /**
   * This is the maximum number of changes that changesSinceV2 will return.
   */
  ConfigSetting<uint64_t> notifyMaxNumberOfChanges{
      "notify:max-num-changes",
      10000,
      this};

  /**
   * Vector of known VCS directories - used to filter changesSinceV2 results.
   */
  ConfigSetting<std::vector<RelativePath>> vcsDirectories{
      "notify:vcs-directories",
      {RelativePath{".hg"}, RelativePath{".git"}, RelativePath{".sl"}},
      this};

  /**
   * In-repo location for notifications states
   */
  ConfigSetting<RelativePath> notificationsStateDirectory{
      "notify:state-directory",
      RelativePath{".edenfs-notifications-state"},
      this};

  // [xplat-logger]

  ConfigSetting<bool> xplatLoggerPeriodicUsernameRefresh{
      "telemetry:periodic-username-refresh",
      true,
      this};

  ConfigSetting<size_t> xplatLoggerQueueLimitBytes{
      "xplat-logger:queue-limit-bytes",
      128 * 1024,
      this};

  ConfigSetting<size_t> xplatLoggerMaxBatchSize{
      "xplat-logger:max-batch-size",
      100,
      this};

  ConfigSetting<size_t> xplatLoggerMaxConsecutiveFailures{
      "xplat-logger:max-consecutive-failures",
      10,
      this};

  ConfigSetting<std::chrono::nanoseconds> xplatLoggerInitialBackoff{
      "xplat-logger:initial-backoff",
      std::chrono::milliseconds(100),
      this};

  ConfigSetting<std::chrono::nanoseconds> xplatLoggerMaxBackoff{
      "xplat-logger:max-backoff",
      std::chrono::seconds(5),
      this};

  ConfigSetting<std::chrono::nanoseconds> xplatLoggerFlushTimeout{
      "xplat-logger:flush-timeout",
      std::chrono::seconds(1),
      this};

  ConfigSetting<std::chrono::nanoseconds> xplatLoggerConnectTimeout{
      "xplat-logger:connect-timeout",
      std::chrono::seconds(1),
      this};

  ConfigSetting<std::chrono::nanoseconds> xplatLoggerRpcTimeout{
      "xplat-logger:rpc-timeout",
      std::chrono::seconds(1),
      this};

  /**
   * Whether to resend a batch whose ScribeD write timed out on the client.
   * ScribeD may already have accepted the batch, so resending can duplicate
   * rows, while dropping it can lose them.
   */
  ConfigSetting<bool> xplatLoggerRetryTimedOutWrites{
      "xplat-logger:retry-timed-out-writes",
      true,
      this};

  // [acl]
  // Path-based ACL settings

  /**
   * TTL in seconds for restricted tree permission rechecks. When a restricted
   * tree's TTL expires, EdenFS calls check_permission to see if the user has
   * been granted access. Set to 0 to recheck on every access.
   */
  ConfigSetting<uint64_t> restrictedTreeTtlSeconds{
      "acl:restricted-tree-ttl-seconds",
      65,
      this};

  /**
   * Interval at which EdenFS rechecks restricted tree roots that are hidden
   * from their parent's listing in omitted mode. Set to 0 to disable.
   */
  ConfigSetting<std::chrono::nanoseconds> restrictedRootRefreshInterval{
      "acl:restricted-root-refresh-interval",
      std::chrono::seconds(0),
      this};

  /**
   * Whether EdenFS should compute UNDER_ACL and local empty ACL info from
   * source-control tree ACL metadata.
   */
  ConfigSetting<bool> enableLocalUnderAclComputation{
      "acl:enable-local-under-acl-computation",
      true,
      this};

  /**
   * Controls how restricted ACL roots are presented. "restricted" (the
   * default) shows them in directory listings with mode 000; "omitted" hides
   * them from enumeration entirely. Read once at daemon startup; runtime
   * config reloads do not change the running mode.
   */
  ConfigSetting<RestrictedContentMode> restrictedContentMode{
      "acl:restricted-content-mode",
      RestrictedContentMode::Restricted,
      this};
};

} // namespace facebook::eden
