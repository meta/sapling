/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <fb303/BaseService.h>
#include <folly/CancellationToken.h>
#include <folly/Executor.h>
#include <folly/coro/Task.h>
#include <folly/coro/safe/NowTask.h>
#include <folly/futures/Future.h>
#include <atomic>
#include <optional>
#include "eden/common/os/ProcessId.h"
#include "eden/common/telemetry/TraceBus.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/fs/eden-config.h"
#include "eden/fs/inodes/EdenMountHandle.h"
#include "eden/fs/inodes/InodePtrFwd.h"
#include "eden/fs/service/gen-cpp2/StreamingEdenService.h"
#include "eden/fs/telemetry/ActivityBuffer.h"

namespace folly {
template <typename T>
class Future;
}

namespace {
class PrefetchFetchContext;
} // namespace

namespace facebook::eden {

class Hash20;
class Hash32;
class BlobAuxData;
class EdenMount;
class EdenServer;
class TreeInode;
class ObjectFetchContext;
using ObjectFetchContextPtr = RefPtr<ObjectFetchContext>;
struct EntryAttributes;
struct EntryAttributeFlags;
template <typename T>
class ImmediateFuture;
struct UnmountOptions;

extern const char* const kServiceName;

const int EXPENSIVE_GLOB_FILES_DURATION = 5;

// Maximum expected time in seconds for a cancelled request to be removed from
// the cancellation map. Used for verification metrics.
const int CANCELLATION_VERIFICATION_THRESHOLD_SECONDS = 1;

enum class RequestStatus { ACTIVE, REQUESTED, UNCANCELABLE };

struct RequestCancellationInfo {
  std::optional<folly::CancellationSource> cancellationSource;
  RequestStatus status = RequestStatus::ACTIVE;
  std::optional<std::chrono::steady_clock::time_point> cancellationRequestedAt;
  // The Thrift method, pointing at its static name.
  std::string_view endpoint;
  OptionalProcessId clientPid;
  std::chrono::steady_clock::time_point startedAt =
      std::chrono::steady_clock::now();
  // A streaming request stays open until the client disconnects or the
  // request is cancelled, so it is not counted as a long-running request.
  bool stream = false;
  // Work that continues after its Thrift response, such as a background glob
  // or prefetch. It is cancelled with everything else, but nothing waits for
  // it and it holds no connection, so it is not counted as long-running or
  // reported at the stop deadline.
  bool background = false;

  RequestCancellationInfo() = default;

  RequestCancellationInfo(
      folly::CancellationSource source,
      std::string_view endpoint,
      OptionalProcessId clientPid = std::nullopt)
      : cancellationSource(std::move(source)),
        status(RequestStatus::ACTIVE),
        endpoint(endpoint),
        clientPid(clientPid) {}

  static RequestCancellationInfo createUncancelable(
      std::string_view endpoint = {},
      OptionalProcessId clientPid = std::nullopt) {
    RequestCancellationInfo info;
    info.status = RequestStatus::UNCANCELABLE;
    info.endpoint = endpoint;
    info.clientPid = clientPid;
    return info;
  }

  bool isCancelable() const {
    return cancellationSource.has_value();
  }

  bool requestCancellation() {
    if (cancellationSource.has_value() && status == RequestStatus::ACTIVE) {
      status = RequestStatus::REQUESTED;
      cancellationRequestedAt = std::chrono::steady_clock::now();
      cancellationSource->requestCancellation();
      return true;
    }
    return false;
  }
};

struct ThriftRequestTraceEvent : TraceEventBase {
  enum Type : unsigned char {
    START,
    FINISH,
  };

  ThriftRequestTraceEvent() = delete;

  static ThriftRequestTraceEvent start(
      uint64_t requestId,
      folly::StringPiece method,
      OptionalProcessId clientPid);

  static ThriftRequestTraceEvent finish(
      uint64_t requestId,
      folly::StringPiece method,
      OptionalProcessId clientPid);

  ThriftRequestTraceEvent(
      Type type,
      uint64_t requestId,
      folly::StringPiece method,
      OptionalProcessId clientPid)
      : type(type),
        requestId(requestId),
        method(method),
        clientPid(clientPid) {}

  Type type;
  uint64_t requestId;
  // Safe to use StringPiece because method names are string literals.
  folly::StringPiece method;
  OptionalProcessId clientPid;
};

/*
 * Handler for the EdenService thrift interface
 */
class EdenServiceHandler
    : virtual public StreamingEdenServiceSvIf,
      public fb303::BaseService,
      public std::enable_shared_from_this<EdenServiceHandler> {
 public:
  explicit EdenServiceHandler(
      std::vector<std::string> originalCommandLine,
      EdenServer* server);
  ~EdenServiceHandler() override;

  EdenServiceHandler(EdenServiceHandler const&) = delete;
  EdenServiceHandler& operator=(EdenServiceHandler const&) = delete;

  /**
   * Return a newly initialized ActivityBuffer<ThriftRequestTraceEvent> if
   * using ActivityBuffers is enabled and return std::nullopt otherwise.
   */
  std::optional<ActivityBuffer<ThriftRequestTraceEvent>>
  initThriftRequestActivityBuffer();

  std::unique_ptr<apache::thrift::AsyncProcessor> getProcessor() override;

  folly::SemiFuture<folly::Unit> semifuture_mount(
      std::unique_ptr<MountArgument> mount) override;

  folly::SemiFuture<folly::Unit> semifuture_unmount(
      std::unique_ptr<std::string> mountPoint) override;

  folly::SemiFuture<folly::Unit> semifuture_unmountV2(
      std::unique_ptr<UnmountArgument> unmountArg) override;

  /**
   * Waits for the unmount, or answers the request with an error once it is
   * cancelled, as EdenServer::stop() cancels every active request. The mount's
   * shutdown is not affected.
   */
  ImmediateFuture<folly::Unit> unmountUnlessCancelled(
      uint64_t requestId,
      AbsolutePath mountPath,
      UnmountOptions options);

  void listMounts(std::vector<MountInfo>& results) override;

  folly::SemiFuture<std::unique_ptr<std::vector<CheckoutConflict>>>
  semifuture_checkOutRevision(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::string> hash,
      CheckoutMode checkoutMode,
      std::unique_ptr<CheckOutRevisionParams> params) override;

  folly::SemiFuture<std::unique_ptr<std::vector<CheckoutConflict>>>
  semifuture_checkOutRevisionImpl(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::string> hash,
      CheckoutMode checkoutMode,
      std::unique_ptr<CheckOutRevisionParams> params);

  /**
   * Coroutine implementation of `checkOutRevision`. Selected by
   * `EdenConfig::enableCoroutinesPhase7`.
   */
  folly::coro::now_task<std::unique_ptr<std::vector<CheckoutConflict>>>
  co_checkOutRevisionImpl(
      apache::thrift::Cpp2RequestContext* requestContext,
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::string> hash,
      CheckoutMode checkoutMode,
      std::unique_ptr<CheckOutRevisionParams> params);

  folly::SemiFuture<folly::Unit> semifuture_resetParentCommits(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<WorkingDirectoryParents> parents,
      std::unique_ptr<ResetParentCommitsParams> params) override;

  void getCurrentSnapshotInfo(
      GetCurrentSnapshotInfoResponse& out,
      std::unique_ptr<GetCurrentSnapshotInfoRequest> params) override;

  folly::SemiFuture<folly::Unit> semifuture_synchronizeWorkingCopy(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<SynchronizeWorkingCopyParams> params) override;

  folly::SemiFuture<folly::Unit> semifuture_addBindMount(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::string> repoPath,
      std::unique_ptr<std::string> targetPath) override;
  folly::SemiFuture<folly::Unit> semifuture_removeBindMount(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::string> repoPath) override;

  folly::coro::Task<std::unique_ptr<std::vector<SHA1Result>>> co_getSHA1(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::vector<std::string>> paths,
      std::unique_ptr<SyncBehavior> sync) override;

  folly::coro::now_task<std::unique_ptr<std::vector<SHA1Result>>> getSHA1Impl(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::vector<std::string>> paths,
      std::unique_ptr<SyncBehavior> sync);

  folly::coro::Task<std::unique_ptr<std::vector<Blake3Result>>> co_getBlake3(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::vector<std::string>> paths,
      std::unique_ptr<SyncBehavior> sync) override;

  folly::coro::now_task<std::unique_ptr<std::vector<Blake3Result>>>
  getBlake3Impl(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::vector<std::string>> paths,
      std::unique_ptr<SyncBehavior> sync);

  folly::coro::Task<std::unique_ptr<std::vector<DigestHashResult>>>
  co_getDigestHash(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::vector<std::string>> paths,
      std::unique_ptr<SyncBehavior> sync) override;

  folly::coro::now_task<std::unique_ptr<std::vector<DigestHashResult>>>
  getDigestHashImpl(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::vector<std::string>> paths,
      std::unique_ptr<SyncBehavior> sync);

  void getCurrentJournalPosition(
      JournalPosition& out,
      std::unique_ptr<std::string> mountPoint) override;

  void peekCurrentJournalPosition(
      PeekCurrentJournalPositionResponse& out,
      std::unique_ptr<PeekCurrentJournalPositionRequest> params) override;

  void getFilesChangedSince(
      FileDelta& out,
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<JournalPosition> fromPosition) override;

  void setJournalMemoryLimit(
      std::unique_ptr<PathString> mountPoint,
      int64_t limit) override;

  int64_t getJournalMemoryLimit(
      std::unique_ptr<PathString> mountPoint) override;

  void flushJournal(std::unique_ptr<PathString> mountPoint) override;

  void debugGetRawJournal(
      DebugGetRawJournalResponse& out,
      std::unique_ptr<DebugGetRawJournalParams> params) override;

  folly::SemiFuture<std::unique_ptr<std::vector<EntryInformationOrError>>>
  semifuture_getEntryInformation(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::vector<std::string>> paths,
      std::unique_ptr<SyncBehavior> sync) override;

  folly::SemiFuture<std::unique_ptr<std::vector<EntryInformationOrError>>>
  semifuture_getEntryInformationImpl(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::vector<std::string>> paths,
      std::unique_ptr<SyncBehavior> sync);

  folly::coro::now_task<std::unique_ptr<std::vector<EntryInformationOrError>>>
  co_getEntryInformationImpl(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::vector<std::string>> paths,
      std::unique_ptr<SyncBehavior> sync);

  folly::SemiFuture<std::unique_ptr<std::vector<FileInformationOrError>>>
  semifuture_getFileInformation(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::vector<std::string>> paths,
      std::unique_ptr<SyncBehavior> sync) override;

  folly::SemiFuture<std::unique_ptr<std::vector<FileInformationOrError>>>
  semifuture_getFileInformationImpl(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::vector<std::string>> paths,
      std::unique_ptr<SyncBehavior> sync);

  folly::coro::now_task<std::unique_ptr<std::vector<FileInformationOrError>>>
  co_getFileInformationImpl(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::vector<std::string>> paths,
      std::unique_ptr<SyncBehavior> sync);

  folly::SemiFuture<std::unique_ptr<GetAttributesFromFilesResultV2>>
  semifuture_getAttributesFromFilesV2(
      std::unique_ptr<GetAttributesFromFilesParams> params) override;

  folly::SemiFuture<std::unique_ptr<GetAttributesFromFilesResultV2>>
  semifuture_getAttributesFromFilesV2Impl(
      std::unique_ptr<GetAttributesFromFilesParams> params);

  folly::coro::now_task<std::unique_ptr<GetAttributesFromFilesResultV2>>
  co_getAttributesFromFilesV2Impl(
      std::unique_ptr<GetAttributesFromFilesParams> params);

  folly::coro::Task<std::unique_ptr<ReaddirResult>> co_readdir(
      std::unique_ptr<ReaddirParams> params) override;

  folly::SemiFuture<std::unique_ptr<Glob>> semifuture_globFiles(
      std::unique_ptr<GlobParams> params) override;

  folly::coro::now_task<std::unique_ptr<Glob>> co_globFilesImpl(
      std::unique_ptr<GlobParams> params);

  folly::coro::Task<std::unique_ptr<PrefetchResult>> co_prefetchFilesV2(
      std::unique_ptr<PrefetchParams> params) override;

  folly::coro::now_task<std::unique_ptr<PrefetchResult>> prefetchFilesV2Impl(
      std::unique_ptr<PrefetchParams> params);

  folly::SemiFuture<std::unique_ptr<ChangeOwnershipResponse>>
  semifuture_changeOwnership(
      std::unique_ptr<ChangeOwnershipRequest> request) override;

  apache::thrift::ServerStream<JournalPosition> subscribeStreamTemporary(
      std::unique_ptr<std::string> mountPoint) override;

  apache::thrift::ServerStream<JournalPosition> streamJournalChanged(
      std::unique_ptr<std::string> mountPoint) override;

  apache::thrift::ServerStream<FsEvent> traceFsEvents(
      std::unique_ptr<std::string> mountPoint,
      int64_t eventCategoryMask) override;

  apache::thrift::ServerStream<ThriftRequestEvent> traceThriftRequestEvents()
      override;

  apache::thrift::ServerStream<HgEvent> traceHgEvents(
      std::unique_ptr<std::string> mountPoint) override;

  apache::thrift::ServerStream<InodeEvent> traceInodeEvents(
      std::unique_ptr<std::string> mountPoint) override;

  apache::thrift::ServerStream<TaskEvent> traceTaskEvents(
      std::unique_ptr<::facebook::eden::TraceTaskEventsRequest> request)
      override;

  folly::coro::Task<std::unique_ptr<GetScmStatusResult>> co_getScmStatusV2(
      apache::thrift::RequestParams params,
      std::unique_ptr<GetScmStatusParams> scmParams) override;

  apache::thrift::ResponseAndServerStream<ChangesSinceResult, ChangedFileResult>
  streamChangesSince(std::unique_ptr<StreamChangesSinceParams> params) override;

  void sync_changesSinceV2(
      ChangesSinceV2Result& result,
      std::unique_ptr<ChangesSinceV2Params> params) override;

  apache::thrift::ResponseAndServerStream<ChangesSinceResult, ChangedFileResult>
  streamSelectedChangesSince(
      std::unique_ptr<StreamSelectedChangesSinceParams> params) override;

  folly::SemiFuture<std::unique_ptr<ScmStatus>>
  semifuture_getScmStatusBetweenRevisions(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::string> oldHash,
      std::unique_ptr<std::string> newHash) override;

  folly::SemiFuture<std::unique_ptr<MatchFileSystemResponse>>
  semifuture_matchFilesystem(
      std::unique_ptr<MatchFileSystemRequest> params) override;

  folly::SemiFuture<std::unique_ptr<DebugGetScmBlobResponse>>
  semifuture_debugGetBlob(
      std::unique_ptr<DebugGetScmBlobRequest> request) override;

  folly::SemiFuture<std::unique_ptr<DebugGetScmBlobResponse>> debugGetBlobImpl(
      std::unique_ptr<DebugGetScmBlobRequest> request);

  folly::coro::Task<std::unique_ptr<DebugGetScmBlobResponse>>
  co_debugGetBlobImpl(
      apache::thrift::Cpp2RequestContext* requestContext,
      std::unique_ptr<DebugGetScmBlobRequest> request);

  folly::SemiFuture<std::unique_ptr<DebugGetBlobMetadataResponse>>
  semifuture_debugGetBlobMetadata(
      std::unique_ptr<DebugGetBlobMetadataRequest> request) override;

  folly::SemiFuture<std::unique_ptr<DebugGetScmTreeResponse>>
  semifuture_debugGetTree(
      std::unique_ptr<DebugGetScmTreeRequest> request) override;

  void debugInodeStatus(
      std::vector<TreeInodeDebugInfo>& inodeInfo,
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::string> path,
      int64_t flags,
      std::unique_ptr<SyncBehavior> sync) override;

  void debugOutstandingFuseCalls(
      std::vector<FuseCall>& outstandingCalls,
      std::unique_ptr<std::string> mountPoint) override;

  void debugOutstandingNfsCalls(
      std::vector<NfsCall>& outstandingCalls,
      std::unique_ptr<std::string> mountPoint) override;

  void debugOutstandingPrjfsCalls(
      std::vector<PrjfsCall>& outstandingCalls,
      std::unique_ptr<std::string> mountPoint) override;

  void debugOutstandingThriftRequests(
      std::vector<ThriftRequestMetadata>& outstandingCalls) override;

  void debugOutstandingHgEvents(
      std::vector<HgEvent>& outstandingEvents,
      std::unique_ptr<std::string> mountPoint) override;

  void debugStartRecordingActivity(
      ActivityRecorderResult& result,
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::string> outputPath) override;

  void debugStopRecordingActivity(
      ActivityRecorderResult& result,
      std::unique_ptr<std::string> mountPoint,
      int64_t unique) override;

  void debugListActivityRecordings(
      ListActivityRecordingsResult& result,
      std::unique_ptr<std::string> mountPoint) override;

  void debugGetInodePath(
      InodePathDebugInfo& inodePath,
      std::unique_ptr<std::string> mountPoint,
      int64_t inodeNumber) override;

  void clearFetchCounts() override;

  void clearFetchCountsByMount(std::unique_ptr<std::string> mountPath) override;

  void getAccessCounts(GetAccessCountsResult& result, int64_t duration)
      override;

  int64_t debugDropAllPendingRequests() override;

  int64_t unloadInodeForPath(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::string> path,
      std::unique_ptr<TimeSpec> age) override;

  folly::SemiFuture<std::unique_ptr<DebugInvalidateResponse>>
  semifuture_debugInvalidateNonMaterialized(
      std::unique_ptr<DebugInvalidateRequest> params) override;

  void flushStatsNow() override;

  folly::SemiFuture<folly::Unit> semifuture_invalidateKernelInodeCache(
      std::unique_ptr<std::string> mountPoint,
      std::unique_ptr<std::string> path) override;

  void getStatInfo(
      InternalStats& result,
      std::unique_ptr<GetStatInfoParams> params) override;

  /**
   * The fb303 counter getters, preceded by a publish of the thread-cached
   * counters so that a read sees increments made just before it.
   */
  void getCounters(std::map<std::string, int64_t>& result) override;
  void getRegexCounters(
      std::map<std::string, int64_t>& result,
      std::unique_ptr<std::string> regex) override;
  void getSelectedCounters(
      std::map<std::string, int64_t>& result,
      std::unique_ptr<std::vector<std::string>> keys) override;
  int64_t getCounter(std::unique_ptr<std::string> key) override;

  void enableTracing() override;
  void disableTracing() override;
  void getTracePoints(std::vector<TracePoint>& result) override;

  void getRetroactiveThriftRequestEvents(
      GetRetroactiveThriftRequestEventsResult& result) override;

  void getRetroactiveHgEvents(
      GetRetroactiveHgEventsResult& result,
      std::unique_ptr<GetRetroactiveHgEventsParams> params) override;

  void getRetroactiveInodeEvents(
      GetRetroactiveInodeEventsResult& result,
      std::unique_ptr<GetRetroactiveInodeEventsParams> params) override;

  void injectFault(std::unique_ptr<FaultDefinition> fault) override;
  bool removeFault(std::unique_ptr<RemoveFaultArg> fault) override;
  int64_t unblockFault(std::unique_ptr<UnblockFaultArg> info) override;
  void getBlockedFaults(
      GetBlockedFaultsResponse& out,
      std::unique_ptr<GetBlockedFaultsRequest> request) override;

  folly::SemiFuture<std::unique_ptr<SetPathObjectIdResult>>
  semifuture_setPathObjectId(
      std::unique_ptr<SetPathObjectIdParams> params) override;

  folly::SemiFuture<folly::Unit> semifuture_removeRecursively(
      std::unique_ptr<RemoveRecursivelyParams> params) override;

  folly::SemiFuture<folly::Unit> semifuture_ensureMaterialized(
      std::unique_ptr<EnsureMaterializedParams> params) override;

  void reloadConfig() override;

  void getDaemonInfo(DaemonInfo& result) override;

  apache::thrift::ResponseAndServerStream<DaemonInfo, std::string>
  streamStartStatus() override;
  /**
   * Checks the PrivHelper connection.
   * For Windows, result.connected will always be set to true.
   */
  void checkPrivHelper(PrivHelperInfo& result) override;

  int64_t getPid() override;

  void getCheckoutProgressInfo(
      CheckoutProgressInfoResponse& ret,
      std::unique_ptr<CheckoutProgressInfoRequest> params) override;

  /**
   * A thrift client has requested that we shutdown.
   */
  void initiateShutdown(std::unique_ptr<std::string> reason) override;

  void getConfig(
      EdenConfigData& result,
      std::unique_ptr<GetConfigParams> params) override;

  /**
   * Enable all backing stores to record fetched files
   */
  void startRecordingBackingStoreFetch() override;

  /**
   * Make all backing stores stop recording
   * fetched files. Previous records for different kinds of backing
   * stores will be returned by backing store types.
   */
  void stopRecordingBackingStoreFetch(GetFetchedFilesResult& results) override;

  /**
   * Returns the pid that caused the Thrift request running on the calling
   * Thrift worker thread and registers it with the ProcessInfoCache.
   *
   * This must be run from a Thrift worker thread, because the calling pid is
   * stored in a thread local variable. For coroutines, pass the request
   * context explicitly to avoid thread-local storage issues.
   */
  OptionalProcessId getAndRegisterClientPid(
      apache::thrift::Cpp2RequestContext* requestContext = nullptr);

  folly::SemiFuture<std::unique_ptr<StartFileAccessMonitorResult>>
  semifuture_startFileAccessMonitor(
      std::unique_ptr<StartFileAccessMonitorParams> params) override;

  folly::SemiFuture<std::unique_ptr<StopFileAccessMonitorResult>>
  semifuture_stopFileAccessMonitor() override;

  void sendNotification(
      SendNotificationResponse& response,
      std::unique_ptr<SendNotificationRequest> request) override;

  void listRedirections(
      ListRedirectionsResponse& response,
      std::unique_ptr<ListRedirectionsRequest> request) override;

  folly::coro::Task<std::unique_ptr<GetFileContentResponse>> co_getFileContent(
      apache::thrift::RequestParams params,
      std::unique_ptr<GetFileContentRequest> request) override;

  folly::coro::Task<std::unique_ptr<::facebook::eden::CancelRequestsResponse>>
  co_cancelRequests(
      apache::thrift::RequestParams params,
      std::unique_ptr<::facebook::eden::CancelRequestsParams> p_params)
      override;

  folly::coro::Task<
      std::unique_ptr<::facebook::eden::GetActiveRequestsResponse>>
  co_getActiveRequests(apache::thrift::RequestParams params) override;

  void insertCancellationSource(
      uint64_t requestId,
      folly::CancellationSource cancellationSource,
      std::string_view endpoint,
      OptionalProcessId clientPid = std::nullopt);

  std::optional<folly::CancellationSource> getCancellationSource(
      uint64_t requestId);

  bool removeCancellationSource(uint64_t requestId);

  bool requestCancellation(uint64_t requestId);

  size_t getActiveCancellationSourceCount() const;

  void insertUncancelableRequest(
      uint64_t requestId,
      std::string_view endpoint = {},
      OptionalProcessId clientPid = std::nullopt);

  /**
   * Record that a request is a stream that stays open until its client
   * disconnects. Cancelling the request ends the stream.
   */
  void markRequestAsStream(uint64_t requestId);

  /**
   * Record that a request's work continues after its response was sent, see
   * RequestCancellationInfo::background.
   */
  void markRequestAsBackground(uint64_t requestId);

  /**
   * End every open stream that was marked with markRequestAsStream(). EdenFS
   * does this when it stops, because the Thrift server cannot shut down while
   * a stream keeps its connection busy.
   */
  void closeTraceStreams();

  struct LongRunningRequest {
    uint64_t requestId;
    std::string method;
    OptionalProcessId clientPid;
    std::chrono::steady_clock::duration age;
    bool cancelable;
  };

  /**
   * The requests, streams excluded, that started at least minAge ago.
   */
  std::vector<LongRunningRequest> getRequestsRunningLongerThan(
      std::chrono::steady_clock::duration minAge) const;

  std::optional<RequestStatus> getRequestStatus(uint64_t requestId);

  std::optional<folly::CancellationToken> getCancellationToken(
      uint64_t requestId);

  /**
   * Cancel all active Thrift requests.
   *
   * Iterates through all active requests in requestCancellationStore_ and
   * requests cancellation for each one. This is used during server shutdown
   * to allow operations to finish gracefully before terminating.
   *
   * @param reason Description of why cancellation was requested (for logging)
   */
  void cancelAllActiveRequests(std::string_view reason);

  /**
   * Log every Thrift request that is still in flight, with its method, age,
   * client and whether it can be cancelled. Called when the daemon stops, so
   * a drain that later times out can be read back to the request it waited
   * on.
   */
  void logOutstandingThriftRequests(std::string_view reason);

  /**
   * Cancel every request that is still active once the delay has passed,
   * unless disarmDelayedCancellation() has run by then. The timer does not
   * depend on the main event base, which is blocked while the Thrift server
   * waits for its requests to finish. Arming again replaces a pending timer.
   */
  void cancelActiveRequestsAfter(
      std::chrono::nanoseconds delay,
      std::string reason);

  /**
   * Cancel the timer armed by cancelActiveRequestsAfter(), if it has not
   * fired. A daemon that resumes serving after a failed takeover must not
   * have its new requests cancelled by the timer the takeover armed.
   */
  void disarmDelayedCancellation();

  void beginStreamJournalChangedShutdown();

 private:
  EdenMountHandle lookupMount(const MountId& mountId);

  /**
   * Where a trace stream's request scope is destroyed once the stream ends,
   * since that may otherwise happen on a TraceBus thread.
   */
  std::shared_ptr<folly::Executor> getTraceStreamExecutor();
  EdenMountHandle lookupMount(const std::unique_ptr<std::string>& mountId);
  EdenMountHandle lookupMount(apache::thrift::field_ref<std::string&> mountId);
  EdenMountHandle lookupMount(
      apache::thrift::field_ref<const std::string&> mountId);
  EdenMountHandle lookupMount(const std::string& mountId);

  void fillDaemonInfo(DaemonInfo& info);

  /**
   * Returns attributes requested by `reqBitmask` for each path in `paths`.
   *
   * The caller must ensure `edenMount` and `paths` stay alive for the duration
   * of the operation.
   */
  ImmediateFuture<std::vector<folly::Try<EntryAttributes>>> getEntryAttributes(
      const EdenMount& edenMount,
      const std::vector<std::string>& paths,
      EntryAttributeFlags reqBitmask,
      AttributesRequestScope reqScope,
      SyncBehavior sync,
      const ObjectFetchContextPtr& fetchContext);
  ImmediateFuture<std::vector<folly::Try<EntryAttributes>>>
  getEntryAttributesImpl(
      const EdenMount& edenMount,
      const std::vector<std::string>& paths,
      EntryAttributeFlags reqBitmask,
      AttributesRequestScope reqScope,
      SyncBehavior sync,
      const ObjectFetchContextPtr& fetchContext);
  ImmediateFuture<EntryAttributes> getEntryAttributesForPath(
      const EdenMount& edenMount,
      EntryAttributeFlags reqBitmask,
      AttributesRequestScope reqScope,
      std::string_view path,
      const ObjectFetchContextPtr& fetchContext);
  ImmediateFuture<EntryAttributes> getEntryAttributesForPathImpl(
      const EdenMount& edenMount,
      EntryAttributeFlags reqBitmask,
      AttributesRequestScope reqScope,
      std::string_view path,
      const ObjectFetchContextPtr& fetchContext);
  folly::coro::now_task<EntryAttributes> co_getEntryAttributesForPath(
      const EdenMount& edenMount,
      EntryAttributeFlags reqBitmask,
      AttributesRequestScope reqScope,
      std::string_view path,
      const ObjectFetchContextPtr& fetchContext);
  folly::coro::now_task<std::vector<folly::Try<EntryAttributes>>>
  co_getEntryAttributes(
      const EdenMount& edenMount,
      const std::vector<std::string>& paths,
      EntryAttributeFlags reqBitmask,
      AttributesRequestScope reqScope,
      SyncBehavior sync,
      const ObjectFetchContextPtr& fetchContext);

  folly::Synchronized<std::unordered_map<uint64_t, ThriftRequestTraceEvent>>
      outstandingThriftRequests_;

  // The timer armed by cancelActiveRequestsAfter(). The generation tells a
  // timer that fires while it is being disarmed that it has nothing to do.
  struct DelayedCancellation {
    uint64_t generation = 0;
    std::optional<folly::Future<folly::Unit>> timer;
  };
  folly::Synchronized<DelayedCancellation> delayedCancellation_;

  folly::Synchronized<std::unordered_map<uint64_t, RequestCancellationInfo>>
      requestCancellationStore_;

  const std::vector<std::string> originalCommandLine_;
  EdenServer* const FOLLY_NONNULL server_;

  // Recovery installs a new handler. Publishers from the old handler retain
  // this state so they still report the shutdown that terminated their stream.
  std::shared_ptr<std::atomic<bool>> streamJournalChangedShuttingDown_;

  std::optional<ActivityBuffer<ThriftRequestTraceEvent>>
      thriftRequestActivityBuffer_;

  TraceSubscriptionHandle<ThriftRequestTraceEvent> thriftRequestTraceHandle_;

  std::shared_ptr<TraceBus<ThriftRequestTraceEvent>> thriftRequestTraceBus_;
};
} // namespace facebook::eden
