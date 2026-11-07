/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/concurrency/memory/AtomicReadMostlyMainPtr.h>
#include <folly/portability/Windows.h>
#include <thrift/lib/cpp/util/EnumUtils.h>

#include "eden/common/telemetry/TraceBus.h"
#include "eden/common/utils/Guid.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/fs/inodes/FsChannel.h"
#include "eden/fs/prjfs/Enumerator.h"
#include "eden/fs/prjfs/PrjfsDispatcher.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/utils/ProcessAccessLog.h"

#ifdef _WIN32
#include <ProjectedFSLib.h> // @manual
#include <libloaderapi.h> // @manual
#endif

namespace facebook::eden {

#ifdef _WIN32

class EdenMount;
class EdenStats;
class Notifier;
class ReloadableConfig;
class PrjfsChannelInner;
class PrjfsRequestContext;
class EdenFsEventsLogger;
class FaultInjector;

using EdenStatsPtr = RefPtr<EdenStats>;

using TraceDetailedArgumentsHandle = std::shared_ptr<void>;

typedef enum __PRJ_EXT_INFO_TYPE {
  PRJ_EXT_INFO_TYPE_SYMLINK = 1
} _PRJ_EXT_INFO_TYPE;

typedef struct _PRJ_EXTENDED_INFO {
  _PRJ_EXT_INFO_TYPE InfoType;
  ULONG NextInfoOffset;
  union {
    struct {
      PCWSTR TargetName;
    } Symlink;
  } DUMMYUNIONNAME;
} PRJ_EXTENDED_INFO;

typedef HRESULT(WINAPI* PPWPI2)(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    PCWSTR,
    const PRJ_PLACEHOLDER_INFO*,
    UINT32,
    const _PRJ_EXTENDED_INFO*);

typedef HRESULT(WINAPI* PPFDEB2)(
    PRJ_DIR_ENTRY_BUFFER_HANDLE dirEntryBufferHandle,
    PCWSTR fileName,
    PRJ_FILE_BASIC_INFO* fileBasicInfo,
    PRJ_EXTENDED_INFO* extendedInfo);

struct PrjfsTraceEvent : TraceEventBase {
  enum Type : unsigned char {
    START,
    FINISH,
  };

  /**
   * Contains the useful fields of PRJ_CALLBACK_DATA to save TraceBus memory.
   */
  struct PrjfsOperationData {
    PrjfsOperationData(const PRJ_CALLBACK_DATA& data)
        : commandId{data.CommandId}, pid{data.TriggeringProcessId} {}

    PrjfsOperationData(const PrjfsOperationData& data) = default;
    int32_t commandId;
    uint32_t pid;
  };

  PrjfsTraceEvent() = delete;

  static PrjfsTraceEvent start(
      PrjfsTraceCallType callType,
      const PrjfsOperationData& data) {
    return PrjfsTraceEvent{
        callType, data, StartDetails{std::unique_ptr<std::string>{}}};
  }

  static PrjfsTraceEvent start(
      PrjfsTraceCallType callType,
      const PrjfsOperationData& data,
      std::string arguments) {
    return PrjfsTraceEvent{
        callType,
        data,
        StartDetails{std::make_unique<std::string>(std::move(arguments))}};
  }

  static PrjfsTraceEvent finish(
      PrjfsTraceCallType callType,
      const PrjfsOperationData& data,
      std::optional<HRESULT> result) {
    return PrjfsTraceEvent{
        callType, PrjfsOperationData{data}, FinishDetails{result}};
  }

  Type getType() const {
    return std::holds_alternative<StartDetails>(details_) ? Type::START
                                                          : Type::FINISH;
  }

  PrjfsTraceCallType getCallType() const {
    return type_;
  }

  const PrjfsOperationData& getData() const {
    return data_;
  }

  const std::unique_ptr<std::string>& getArguments() const {
    return std::get<StartDetails>(details_).arguments;
  }

  const std::optional<HRESULT>& getResult() const {
    return std::get<FinishDetails>(details_).result;
  }

 private:
  struct StartDetails {
    /**
     * If detailed trace arguments have been requested, this field contains a
     * human-readable representation of the Prjfs request arguments.
     *
     * It is heap-allocated to reduce memory usage in the common case that
     * detailed argument tracing is disabled.
     */
    std::unique_ptr<std::string> arguments;
  };

  struct FinishDetails {
    std::optional<HRESULT> result;
  };

  using Details = std::variant<StartDetails, FinishDetails>;

  PrjfsTraceEvent(
      PrjfsTraceCallType callType,
      const PrjfsOperationData& data,
      Details&& details)
      : type_{callType}, data_{data}, details_{std::move(details)} {}

  PrjfsTraceCallType type_;
  PrjfsTraceEvent::PrjfsOperationData data_;
  Details details_;
};

class PrjfsChannelInner {
 public:
  PrjfsChannelInner(
      std::unique_ptr<PrjfsDispatcher> dispatcher,
      const folly::Logger* straceLogger,
      const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
      FaultInjector& faultInjector,
      std::shared_ptr<ProcessAccessLog> processAccessLog,
      std::shared_ptr<ReloadableConfig>& config,
      folly::Promise<folly::Unit> deletedPromise,
      std::shared_ptr<Notifier> notifier,
      size_t prjfsTraceBusCapacity,
      const std::shared_ptr<folly::Executor>& invalidationThreadPool);

  ~PrjfsChannelInner();

  explicit PrjfsChannelInner() = delete;
  PrjfsChannelInner(const PrjfsChannelInner&) = delete;
  PrjfsChannelInner& operator=(const PrjfsChannelInner&) = delete;

  ImmediateFuture<folly::Unit> waitForPendingNotifications();

  folly::coro::now_task<folly::Unit> co_waitForPendingNotifications();

  /**
   * Start a directory listing.
   *
   * May spawn futures which will extend the lifetime of self.
   */
  HRESULT startEnumeration(
      std::shared_ptr<PrjfsRequestContext> context,
      const PRJ_CALLBACK_DATA* callbackData,
      const GUID* enumerationId);

  /**
   * Terminate a directory listing.
   *
   * May spawn futures which will extend the lifetime of self.
   */
  HRESULT endEnumeration(
      std::shared_ptr<PrjfsRequestContext> context,
      const PRJ_CALLBACK_DATA* callbackData,
      const GUID* enumerationId);

  /**
   * Populate as many directory entries that dirEntryBufferHandle can take.
   *
   * May spawn futures which will extend the lifetime of self.
   */
  HRESULT getEnumerationData(
      std::shared_ptr<PrjfsRequestContext> context,
      const PRJ_CALLBACK_DATA* callbackData,
      const GUID* enumerationId,
      PCWSTR searchExpression,
      PRJ_DIR_ENTRY_BUFFER_HANDLE dirEntryBufferHandle);

  /**
   * Obtain the metadata for a given file.
   *
   * May spawn futures which will extend the lifetime of self.
   */
  HRESULT getPlaceholderInfo(
      std::shared_ptr<PrjfsRequestContext> context,
      const PRJ_CALLBACK_DATA* callbackData);

  /**
   * Test whether a given file exist in the repository.
   *
   * May spawn futures which will extend the lifetime of self.
   */
  HRESULT queryFileName(
      std::shared_ptr<PrjfsRequestContext> context,
      const PRJ_CALLBACK_DATA* callbackData);

  /**
   * Read the content of the given file.
   *
   * May spawn futures which will extend the lifetime of self.
   */
  HRESULT getFileData(
      std::shared_ptr<PrjfsRequestContext> context,
      const PRJ_CALLBACK_DATA* callbackData,
      UINT64 byteOffset,
      UINT32 length);

  /**
   * Notifies of state change for the given file.
   *
   * May spawn futures which will extend the lifetime of self.
   */
  HRESULT notification(
      std::shared_ptr<PrjfsRequestContext> context,
      const PRJ_CALLBACK_DATA* callbackData,
      BOOLEAN isDirectory,
      PRJ_NOTIFICATION notificationType,
      PCWSTR destinationFileName,
      PRJ_NOTIFICATION_PARAMETERS* notificationParameters);

  /**
   * Notification sent when a file or directory has been created.
   */
  ImmediateFuture<folly::Unit> newFileCreated(
      RelativePath relPath,
      RelativePath destPath,
      bool isDirectory,
      const ObjectFetchContextPtr& context);

  /**
   * Notification sent when a file or directory has been replaced.
   */
  ImmediateFuture<folly::Unit> fileOverwritten(
      RelativePath relPath,
      RelativePath destPath,
      bool isDirectory,
      const ObjectFetchContextPtr& context);

  /**
   * Notification sent when a file has been modified.
   */
  ImmediateFuture<folly::Unit> fileHandleClosedFileModified(
      RelativePath relPath,
      RelativePath destPath,
      bool isDirectory,
      const ObjectFetchContextPtr& context);

  /**
   * Notification sent when a file or directory has been renamed.
   */
  ImmediateFuture<folly::Unit> fileRenamed(
      RelativePath oldPath,
      RelativePath newPath,
      bool isDirectory,
      const ObjectFetchContextPtr& context);

  /**
   * Notification sent prior to a file or directory being renamed.
   *
   * Called prior to ProjectedFS doing any on-disk checks, and thus if newPath
   * exist on disk, the file rename may later fail. The rename is known to have
   * happened only when fileRenamed is called.
   */
  ImmediateFuture<folly::Unit> preRename(
      RelativePath oldPath,
      RelativePath newPath,
      bool isDirectory,
      const ObjectFetchContextPtr& context);

  /**
   * Notification sent prior to a file or directory being deleted.
   *
   * Called prior to ProjectedFS doing any on-disk checks, and thus if relPath
   * may or may not exist on disk and the deletion may later fail. The deletion
   * is known to have happened only when fileHandleClosedFileDeleted is called.
   */
  ImmediateFuture<folly::Unit> preDelete(
      RelativePath relPath,
      RelativePath destPath,
      bool isDirectory,
      const ObjectFetchContextPtr& context);

  /**
   * Notification sent when a file or directory has been removed.
   */
  ImmediateFuture<folly::Unit> fileHandleClosedFileDeleted(
      RelativePath relPath,
      RelativePath destPath,
      bool isDirectory,
      const ObjectFetchContextPtr& context);

  /**
   * Notification sent prior to a hardlink being created.
   */
  ImmediateFuture<folly::Unit> preSetHardlink(
      RelativePath oldPath,
      RelativePath newPath,
      bool isDirectory,
      const ObjectFetchContextPtr& context);

  /**
   * Notification sent prior to a file being converted to a full file.
   */
  ImmediateFuture<folly::Unit> preConvertToFull(
      RelativePath relpath,
      RelativePath destPath,
      bool isDirectory,
      const ObjectFetchContextPtr& context);

  /**
   * This is not a handler for PrjFS notifications, this method can be called by
   * internal callers who notice that Eden's view of the filesystem is out of
   * sync.
   *
   * Unlike the prjfs notification handlers, the returned future does wait
   * for the notification to be processed. That means that Eden should be
   * in sync with the filesystem after the returned future completes.
   */
  ImmediateFuture<folly::Unit> matchEdenViewOfFileToFS(
      RelativePath relPath,
      const ObjectFetchContextPtr& context);

  const std::shared_ptr<ProcessAccessLog>& getProcessAccessLog() {
    return processAccessLog_;
  }

  std::shared_ptr<EdenFsEventsLogger> getEdenFsEventsLogger() const {
    return edenFsEventsLogger_;
  }

  std::chrono::nanoseconds getLongRunningFSRequestThreshold() const {
    return longRunningFSRequestThreshold_;
  }

  void setMountChannel(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT channel) {
    mountChannel_ = channel;
  }

  PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT getMountChannel() const {
    return mountChannel_;
  }

  void sendSuccess(
      int32_t commandId,
      PRJ_COMPLETE_COMMAND_EXTENDED_PARAMETERS* FOLLY_NULLABLE extra);

  void sendError(int32_t commandId, HRESULT error);

  struct OutstandingRequest {
    PrjfsTraceCallType type;
    PrjfsTraceEvent::PrjfsOperationData data;
  };

  /**
   * Returns the approximate set of outstanding PrjFS requests. Since
   * telemetry is tracked on a background thread, the result may very slightly
   * lag reality.
   */
  std::vector<PrjfsChannelInner::OutstandingRequest> getOutstandingRequests();

  /**
   * While the returned handle is alive, PrjfsTraceEvents published on the
   * TraceBus will have detailed argument strings.
   */
  TraceDetailedArgumentsHandle traceDetailedArguments();

  std::shared_ptr<TraceBus<PrjfsTraceEvent>> getTraceBusPtr() {
    return traceBus_;
  }

  const std::atomic<size_t>& getTraceDetailedArguments() const {
    return *traceDetailedArguments_;
  }

  const EdenStatsPtr& getStats() const {
    return dispatcher_->getStats();
  }

  void initializeSymlinkSupport();

  bool symlinksSupported() {
    return symlinksSupported_;
  }

 private:
  const folly::Logger& getStraceLogger() const {
    return *straceLogger_;
  }

  void addDirectoryEnumeration(Guid guid, std::vector<PrjfsDirEntry> dirents) {
    auto [iterator, inserted] = enumSessions_.wlock()->emplace(
        std::move(guid), std::make_shared<Enumerator>(std::move(dirents)));
    XDCHECK(inserted);
  }

  std::optional<std::shared_ptr<Enumerator>> findDirectoryEnumeration(
      Guid& guid) {
    auto enumerators = enumSessions_.rlock();
    auto it = enumerators->find(guid);

    if (it == enumerators->end()) {
      return std::nullopt;
    }

    return it->second;
  }

  void removeDirectoryEnumeration(Guid& guid) {
    enumSessions_.wlock()->erase(guid);
    // In theory, we should check that we removed an entry, but ProjectedFS
    // sometimes likes to close directories that were never opened, making
    // checking for the return value of erase unreliable. Since it doesn't
    // really matter if we remove an entry in this case, we are free to ignore
    // the return value.
  }

  // Internal ProjectedFS channel used to communicate with ProjectedFS.
  PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT mountChannel_{nullptr};

  std::unique_ptr<PrjfsDispatcher> dispatcher_;
  const folly::Logger* const straceLogger_{nullptr};

  const std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger_;

  FaultInjector& faultInjector_;

  // thread pool used for invalidation operations. These can block and it's
  // likely better not to run them on the same thread as we process requests
  // PrjFS in case there is any re-entrancy.
  const std::shared_ptr<folly::Executor> invalidationThreadPool_;

  // The last time we logged a about a torn read the interval at which
  // we log is set by EdenConfig::tornReadLogInterval.
  std::shared_ptr<folly::Synchronized<std::chrono::steady_clock::time_point>>
      lastTornReadLog_;

  std::shared_ptr<Notifier> notifier_;

  std::shared_ptr<ProcessAccessLog> processAccessLog_;

  std::shared_ptr<ReloadableConfig> config_;

  // Set of currently active directory enumerations.
  folly::Synchronized<folly::F14FastMap<Guid, std::shared_ptr<Enumerator>>>
      enumSessions_;

  // Set when the destructor is called.
  folly::Promise<folly::Unit> deletedPromise_;

  struct TelemetryState {
    std::unordered_map<uint64_t, OutstandingRequest> requests;
  };
  folly::Synchronized<TelemetryState> telemetryState_;
  std::vector<TraceSubscriptionHandle<PrjfsTraceEvent>>
      traceSubscriptionHandles_;
  std::shared_ptr<std::atomic<size_t>> traceDetailedArguments_;
  // The TraceBus must be the last member because its subscribed functions may
  // close over `this` and can run until the TraceBus itself is deallocated.
  std::shared_ptr<TraceBus<PrjfsTraceEvent>> traceBus_;

  /**
   * The duration that must elapse before we consider a PrjFS request to be
   * "long running" and therefore log it with StructuredLogger. This value
   * is configured with EdenConfig::longRunningFSRequestThreshold.
   */
  std::chrono::nanoseconds longRunningFSRequestThreshold_;

  bool symlinksSupported_ = false;
};

class PrjfsChannel : public FsChannel {
 public:
  PrjfsChannel(const PrjfsChannel&) = delete;
  PrjfsChannel& operator=(const PrjfsChannel&) = delete;

  explicit PrjfsChannel() = delete;

  PrjfsChannel(
      AbsolutePathPiece mountPath,
      std::unique_ptr<PrjfsDispatcher> dispatcher,
      std::shared_ptr<ReloadableConfig> config,
      const folly::Logger* straceLogger,
      const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
      FaultInjector& faultInjector,
      std::shared_ptr<ProcessInfoCache> processInfoCache,
      Guid guid,
      bool enableWindowsSymlinks,
      std::shared_ptr<Notifier> notifier,
      const std::shared_ptr<folly::Executor>& invalidationThreadPool);

  virtual ~PrjfsChannel();

  void destroy() override;

  folly::Future<StopFuture> initialize() override;

  /**
   * Wait for all the received notifications to be fully handled.
   *
   * The PrjfsChannel will receive notifications and immediately dispatch the
   * work to a background executor and return S_OK to ProjectedFS to unblock
   * applications writing to the EdenFS repository.
   *
   * Thus an application writing to the repository may have their file creation
   * be successful prior to EdenFS having updated its inode hierarchy. This
   * discrepancy can cause issues in EdenFS for operations that exclusively
   * look at the inode hierarchy such as status/checkout/glob.
   *
   * The returned ImmediateFuture will complete when all the previously
   * received notifications have completed.
   */
  ImmediateFuture<folly::Unit> waitForPendingWrites() override;

  folly::coro::now_task<folly::Unit> co_waitForPendingWrites() override;

  /**
   * Background correctness checkers might notice that EdenFS has an incorrect
   * view of a file. This can be used to make Eden pickup any changes it missed
   * to a file.
   *
   * Unlike the prjfs notification handlers, the returned future does wait
   * for the notification to be processed. That means that Eden should be
   * in sync with the filesystem after the returned future completes.
   */
  ImmediateFuture<folly::Unit> matchEdenViewOfFileToFS(
      RelativePath relPath,
      const ObjectFetchContextPtr& context);

  const char* getName() const override {
    return "prjfs";
  }

  /**
   * Stop the PrjfsChannel.
   *
   * The returned future will complete once all the pending callbacks and
   * notifications are completed.
   *
   * PrjfsChannel must not be destructed until the returned future is
   * fulfilled.
   */
  folly::SemiFuture<folly::Unit> unmount(UnmountOptions options) override;

  bool takeoverStop() override {
    // ProjFS does not support takeover.
    return false;
  }

  struct StopData : FsStopData {
    bool isUnmounted() override;
    FsChannelInfo extractTakeoverInfo() override;
  };

  StopFuture getStopFuture();

  /**
   * Remove a file that has been cached on disk by ProjectedFS. This should be
   * called when the content of a materialized file has changed, typically
   * called during on an `update` operation.
   *
   * This can fail when the underlying file cannot be evicted from ProjectedFS,
   * one example is when the user has locked the file.
   */
  [[nodiscard]] virtual folly::Try<folly::Unit> removeCachedFile(
      RelativePathPiece path);

  /**
   * Ensure that the directory is a placeholder so that ProjectedFS will always
   * invoke the opendir/readdir callbacks when the user is listing files in it.
   * This particularly matters for directories that were created by the user to
   * later be committed.
   */
  [[nodiscard]] virtual folly::Try<folly::Unit> addDirectoryPlaceholder(
      RelativePathPiece path);

  ImmediateFuture<folly::Unit> completeInvalidations() override;

  folly::coro::now_task<folly::Unit> co_completeInvalidations() override;

  void flushNegativePathCache();

  const std::shared_ptr<ProcessAccessLog>& getProcessAccessLog() override {
    return processAccessLog_;
  }

  /**
   * Copy the inner channel.
   *
   * As long as the returned value is alive, the mount cannot be unmounted.
   * When an unmount is pending, the shared_ptr will be NULL.
   */
  folly::ReadMostlySharedPtr<PrjfsChannelInner> getInner() {
    return inner_.load(std::memory_order_consume);
  }

 private:
  const AbsolutePath mountPath_;
  Guid mountId_;
  bool enableSymlinks_;
  bool useNegativePathCaching_{true};
  folly::Promise<FsStopDataPtr> stopPromise_{
      folly::Promise<FsStopDataPtr>::makeEmpty()};

  std::shared_ptr<ProcessAccessLog> processAccessLog_;

  std::shared_ptr<ReloadableConfig> config_;

  folly::AtomicReadMostlyMainPtr<PrjfsChannelInner> inner_;
  folly::SemiFuture<folly::Unit> innerDeleted_;
};

#endif

} // namespace facebook::eden
