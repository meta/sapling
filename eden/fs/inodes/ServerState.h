/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/Synchronized.h>
#include <folly/concurrency/memory/ReadMostlySharedPtr.h>
#include <folly/container/F14Map.h>
#include <folly/executors/FunctionScheduler.h>
#include <folly/synchronization/CallOnce.h>
#include <memory>

#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/common/utils/UserInfo.h"
#include "eden/fs/config/CachedParsedFileMonitor.h"
#include "eden/fs/inodes/PreloadOperation.h"
#include "eden/fs/model/git/GitIgnoreFileParser.h"
#include "eden/fs/utils/GlobMatcher.h"

namespace folly {
class EventBase;
class IOThreadPoolExecutor;
} // namespace folly

namespace facebook::eden {

using PreloadProgressMap =
    folly::F14FastMap<std::string, std::shared_ptr<PreloadOperation>>;

class Clock;
class EdenConfig;
class EdenFsEventsLogger;
class EdenErrorInfoBuilder;
class EdenStats;
class ErrorLogger;
class FaultInjector;
class IXplatLogger;
class InodeAccessLogger;
class NfsServer;
class Notifier;
class PrivHelper;
class ProcessInfoCache;
class ReloadableConfig;
class StructuredLogger;
class TopLevelIgnores;
class UnboundedQueueExecutor;
struct SessionInfo;

using EdenStatsPtr = RefPtr<EdenStats>;

/**
 * ServerState is the testable, dependency injection seam for the inode
 * layer. It includes some platform abstractions like Clock, loggers,
 * and configuration, and state shared across multiple mounts.
 *
 * This is normally owned by the main EdenServer object. However unit
 * tests also create ServerState objects without an
 * EdenServer. ServerState should not contain expensive-to-create
 * objects or they should be abstracted behind an interface so
 * appropriate fakes can be used in tests.
 */
class ServerState {
 public:
  ServerState(
      UserInfo userInfo,
      EdenStatsPtr edenStats,
      SessionInfo sessionInfo, // NOLINT(performance-unnecessary-value-param)
      std::shared_ptr<PrivHelper> privHelper,
      std::shared_ptr<UnboundedQueueExecutor> threadPool,
      std::shared_ptr<UnboundedQueueExecutor> fsChannelThreadPool,
      std::shared_ptr<Clock> clock,
      std::shared_ptr<ProcessInfoCache> processInfoCache,
      std::shared_ptr<StructuredLogger> structuredLogger,
      std::shared_ptr<StructuredLogger> notificationsStructuredLogger,
      std::shared_ptr<ErrorLogger> errorLogger,
      std::shared_ptr<ReloadableConfig> reloadableConfig,
      const EdenConfig& initialConfig,
      folly::EventBase* nfsEventBase,
      std::shared_ptr<Notifier> notifier,
      bool enableFaultInjection = false,
      std::shared_ptr<InodeAccessLogger> inodeAccessLogger = nullptr,
      std::shared_ptr<IXplatLogger> xplatLogger = nullptr);
  ~ServerState();

  void shutdown();

  /**
   * Set the path to the server's thrift socket.
   *
   * This is called by EdenServer once it has initialized the thrift server.
   */
  void setSocketPath(AbsolutePathPiece path) {
    socketPath_ = path.copy();
  }

  /**
   * Get the path to the server's thrift socket.
   *
   * This is used by the EdenMount to populate the `.eden/socket` special file.
   */
  const AbsolutePath& getSocketPath() const {
    return socketPath_;
  }

  /**
   * Get the EdenStats object that tracks process-wide (rather than per-mount)
   * statistics.
   */
  const EdenStatsPtr& getStats() const {
    return edenStats_;
  }

  const std::shared_ptr<ReloadableConfig>& getReloadableConfig() const {
    return config_;
  }

  /**
   * Get the EdenConfig data.
   */
  folly::ReadMostlySharedPtr<const EdenConfig> getEdenConfig();

  /** Snapshot the glob matcher limits and ODS accounting callback. */
  GlobMatchOptions getGlobMatchOptions();

  /**
   * Get the TopLevelIgnores. It is based on the system and user git ignore
   * files.
   */
  std::unique_ptr<TopLevelIgnores> getTopLevelIgnores();

  /**
   * Get the UserInfo object describing the user running this edenfs process.
   */
  const UserInfo& getUserInfo() const {
    return userInfo_;
  }

  /**
   * Get the PrivHelper object used to perform operations that require
   * elevated privileges.
   */
  PrivHelper* getPrivHelper() {
    return privHelper_.get();
  }

  /**
   * Get the thread pool.
   *
   * Adding new tasks to this thread pool executor will never block.
   */
  const std::shared_ptr<UnboundedQueueExecutor>& getThreadPool() const {
    return threadPool_;
  }

  /**
   * Get the FS channel thread pool.
   *
   * FS channel requests are intended to run on this thread pool.
   */
  const std::shared_ptr<UnboundedQueueExecutor>& getFsChannelThreadPool()
      const {
    return fsChannelThreadPool_;
  }

  /**
   * Get the Clock.
   */
  const std::shared_ptr<Clock>& getClock() const {
    return clock_;
  }

  /**
   * The NfsServer, or null when experimental:enable-nfs-server is false and
   * no graceful restart handed NFS mounts over (see
   * createNfsServerForTakeover).
   */
  const std::shared_ptr<NfsServer>& getNfsServer() const& {
    return nfs_;
  }

  /**
   * Build the NfsServer although experimental:enable-nfs-server is false.
   *
   * A graceful restart hands NFS mounts over whatever the new daemon's config
   * says, and only an NfsServer in this process can serve them. EdenServer
   * calls this while it prepares the takeover, before any mount exists and
   * before anything reads getNfsServer(). New mounts still follow the config;
   * a full restart mounts everything the way the config says.
   */
  void createNfsServerForTakeover();

  const std::shared_ptr<ProcessInfoCache>& getProcessInfoCache() const {
    return processInfoCache_;
  }

  const std::shared_ptr<StructuredLogger>& getStructuredLogger() const {
    return structuredLogger_;
  }

  const std::shared_ptr<EdenFsEventsLogger>& getEdenFsEventsLogger() const {
    return edenFsEventsLogger_;
  }

  const std::shared_ptr<StructuredLogger>& getNotificationsStructuredLogger()
      const {
    return notificationsStructuredLogger_;
  }

  ErrorLogger& getErrorLogger() const {
    return *errorLogger_;
  }

  /**
   * Returns a InodeAccessLogger that can be used to send log events to external
   * long term storage for offline consumption. Prefer this method if the
   * caller needs to own a reference due to lifetime mismatch with the
   * ServerState
   */
  const std::shared_ptr<InodeAccessLogger>& getInodeAccessLogger() const {
    return inodeAccessLogger_;
  }

  FaultInjector& getFaultInjector() {
    return *faultInjector_;
  }

  const std::shared_ptr<Notifier>& getNotifier() {
    return notifier_;
  }

  /**
   * Get the map of active page cache preload operations.
   * Used by EdenServiceHandler to query progress of ongoing operations.
   */
  folly::Synchronized<PreloadProgressMap>& getPreloadProgressMap() {
    return preloadProgressMap_;
  }

  /**
   * Get the dedicated thread pool used for page-cache preload work, creating
   * it on first use.
   *
   * Preload tasks perform blocking filesystem I/O against EdenFS's own mount
   * (open/read/mmap), so they must NOT share a pool with Thrift handlers or
   * other EdenMount work — see ACR_thread_pool_batching.md for the starvation
   * pattern (S412223 / S399431).
   */
  const std::shared_ptr<folly::IOThreadPoolExecutor>& getPreloadThreadPool()
      const;

  /**
   * Remove a completed preload operation from the progress map.
   */
  void removePreloadProgress(const std::string& operationId) {
    preloadProgressMap_.wlock()->erase(operationId);
  }

  /**
   * Remove completed preload operations that finished more than maxAge ago,
   * plus old not-yet-done operations that have stopped making progress
   * (presumed stranded by a bug; reaping bounds the map size).
   *
   * Runs periodically from preloadCleanupScheduler_ so entries are evicted
   * even when no client polls. Progress queries for an evicted id report it
   * as not-found.
   */
  void cleanupStalePreloadProgress(std::chrono::seconds maxAge);

 private:
  void shutdownPreloadCleanup();
  std::shared_ptr<NfsServer> makeNfsServer(const EdenConfig& config) const;

  AbsolutePath socketPath_;
  UserInfo userInfo_;
  EdenStatsPtr edenStats_;
  std::shared_ptr<PrivHelper> privHelper_;
  std::shared_ptr<UnboundedQueueExecutor> threadPool_;
  std::shared_ptr<UnboundedQueueExecutor> fsChannelThreadPool_;
  std::shared_ptr<Clock> clock_;
  std::shared_ptr<ProcessInfoCache> processInfoCache_;
  std::shared_ptr<StructuredLogger> structuredLogger_;
  std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger_;
  std::shared_ptr<StructuredLogger> notificationsStructuredLogger_;
  std::shared_ptr<ErrorLogger> errorLogger_;
  std::unique_ptr<FaultInjector> const faultInjector_;
  folly::EventBase* const nfsEventBase_;
  std::shared_ptr<NfsServer> nfs_;

  std::shared_ptr<ReloadableConfig> config_;
  folly::Synchronized<CachedParsedFileMonitor<GitIgnoreFileParser>>
      userIgnoreFileMonitor_;
  folly::Synchronized<CachedParsedFileMonitor<GitIgnoreFileParser>>
      systemIgnoreFileMonitor_;
  std::shared_ptr<Notifier> notifier_;
  std::shared_ptr<InodeAccessLogger> inodeAccessLogger_;

  // Map of active page cache preload operations, keyed by a unique operation
  // ID.
  folly::Synchronized<PreloadProgressMap> preloadProgressMap_;

  // Dedicated executor for blocking preload I/O. Sized to match the previous
  // implicit per-call worker count (32). Lives separately from threadPool_ so
  // a slow preload cannot starve Thrift handlers. Lazily constructed by
  // getPreloadThreadPool() so processes that never preload don't pay for it.
  mutable folly::once_flag preloadThreadPoolOnceFlag_;
  mutable std::shared_ptr<folly::IOThreadPoolExecutor> preloadThreadPool_;

  // Periodic background sweep of preloadProgressMap_ to evict completed
  // operations whose clients never polled. Started in the constructor,
  // shut down in the destructor.
  folly::FunctionScheduler preloadCleanupScheduler_;
  folly::once_flag preloadCleanupShutdownOnceFlag_;
  folly::once_flag executorShutdownOnceFlag_;
};
} // namespace facebook::eden
