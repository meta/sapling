/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifdef _WIN32

#include "eden/fs/prjfs/PrjfsChannel.h"
#include <fmt/format.h>
#include <folly/coro/Invoke.h>
#include <folly/coro/safe/NowTask.h>
#include <folly/logging/xlog.h>

#include "eden/common/utils/Bug.h"
#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/Guid.h"
#include "eden/common/utils/ImmediateFuture.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/StringConv.h"
#include "eden/common/utils/windows/WinError.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/notifications/Notifier.h"
#include "eden/fs/prjfs/PrjfsDispatcher.h"
#include "eden/fs/prjfs/PrjfsRequestContext.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/LogEvent.h"
#include "eden/fs/utils/NotImplemented.h"
#include "eden/fs/utils/StaticAssert.h"

namespace facebook::eden {

using namespace std::literals::chrono_literals;

namespace {
// These static asserts exist to make explicit the memory usage of the per-mount
// PrjfsTraceBus. TraceBus uses 2 * capacity * sizeof(TraceEvent) memory usage,
// so limit total memory usage to around 1 MB per mount.
static_assert(CheckSize<PrjfsTraceEvent, 48>());

PPWPI2 placeholderExtendedInfo2_{nullptr};
PPFDEB2 prjFillDirEntryBuffer2_{nullptr};

// TODO: Remove once the build has switched to a more recent SDK
HRESULT PrjWritePlaceholderInfo2(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT namespaceVirtualizationContext,
    PCWSTR destinationFileName,
    const PRJ_PLACEHOLDER_INFO* placeholderInfo,
    UINT32 placeholderInfoSize,
    const PRJ_EXTENDED_INFO* ExtendedInfo) {
  return placeholderExtendedInfo2_(
      namespaceVirtualizationContext,
      destinationFileName,
      placeholderInfo,
      placeholderInfoSize,
      ExtendedInfo);
}

HRESULT PrjFillDirEntryBuffer2(
    PRJ_DIR_ENTRY_BUFFER_HANDLE dirEntryBufferHandle,
    PCWSTR fileName,
    PRJ_FILE_BASIC_INFO* fileBasicInfo,
    PRJ_EXTENDED_INFO* extendedInfo) {
  return prjFillDirEntryBuffer2_(
      dirEntryBufferHandle, fileName, fileBasicInfo, extendedInfo);
}

folly::ReadMostlySharedPtr<PrjfsChannelInner> getChannel(
    const PRJ_CALLBACK_DATA* callbackData) noexcept {
  XDCHECK(callbackData);
  auto* channel = static_cast<PrjfsChannel*>(callbackData->InstanceContext);
  XDCHECK(channel);
  return channel->getInner();
}

/**
 * ProjectedFS gives us a full device path for the application that triggered
 * the IO, this trims it an returns a view onto the last component.
 *
 * The lifetime of the returned view is the same as the lifetime of the
 * argument.
 */
std::wstring_view basenameFromAppName(PCWSTR fullAppName) noexcept {
  auto fullAppNameView = std::wstring_view{fullAppName};
  auto lastSlash = fullAppNameView.find_last_of(L'\\');
  return fullAppNameView.substr(lastSlash + 1);
}

/**
 * Disallow some know applications that force EdenFS to overfetch files.
 *
 * Some backup applications or indexing are ignoring the
 * FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS attribute attached to all EdenFS
 * files/directories and are therefore forcing the entire repository to be
 * fetched. Since this isn't the intention of these applications, simply
 * disallow them from accessing anything on EdenFS.
 */
bool disallowMisbehavingApplications(PCWSTR fullAppName) noexcept {
  if (fullAppName == nullptr) {
    return false;
  }

  constexpr std::wstring_view misbehavingApps[] = {
      L"Code42Service.exe",
      L"CrashPlanService.exe",
      L"windirstat.exe",
      L"AgentRansack.exe",
  };

  auto appName = basenameFromAppName(fullAppName);
  for (auto misbehavingApp : misbehavingApps) {
    if (appName == misbehavingApp) {
      XLOGF(
          DBG6,
          "Stopping \"{}\" from accessing the repository.",
          wideToMultibyteString<std::string>(appName));
      return true;
    }
  }

  return false;
}
} // namespace

namespace {

template <class Method, class... Args>
HRESULT runCallback(
    Method method,
    PrjfsTraceCallType callType,
    const PRJ_CALLBACK_DATA* callbackData,
    LPCWSTR destinationFileName,
    Args&&... args) noexcept {
  try {
    if (disallowMisbehavingApplications(
            callbackData->TriggeringProcessImageFileName)) {
      return HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED);
    }

    auto channel = getChannel(callbackData);
    if (!channel) {
      return HRESULT_FROM_WIN32(ERROR_INVALID_PARAMETER);
    }

    auto channelPtr = channel.get();
    auto context = std::make_shared<PrjfsRequestContext>(
        std::move(channel), *callbackData, callType, destinationFileName);
    return (channelPtr->*method)(
        std::move(context), callbackData, std::forward<Args>(args)...);
  } catch (const std::exception& ex) {
    return exceptionToHResult(ex);
  }
}

/**
 * Log on callbacks triggered by EdenFS.
 *
 * All callbacks besides the "notification" one are allowed to be called from
 * EdenFS itself, this is due to these only accessing data from the
 * ObjectStore which will never perform any disk IO to the working copy. To
 * handle out of order notifications about file/directory changes, the
 * "notification" callback may need to read the working copy, which may
 * trigger some callbacks to be triggered. These are OK due to the property
 * described above.
 */
void allowRecursiveCallbacks(const PRJ_CALLBACK_DATA* callbackData) {
  if (callbackData->TriggeringProcessId == GetCurrentProcessId()) {
    XLOGF(
        DBG6,
        "Recursive EdenFS call for: {}",
        RelativePath(callbackData->FilePathName));
  }
}

HRESULT startEnumeration(
    const PRJ_CALLBACK_DATA* callbackData,
    const GUID* enumerationId) noexcept {
  allowRecursiveCallbacks(callbackData);
  return runCallback(
      &PrjfsChannelInner::startEnumeration,
      PrjfsTraceCallType::START_ENUMERATION,
      callbackData,
      nullptr,
      enumerationId);
}

HRESULT endEnumeration(
    const PRJ_CALLBACK_DATA* callbackData,
    const GUID* enumerationId) noexcept {
  allowRecursiveCallbacks(callbackData);
  return runCallback(
      &PrjfsChannelInner::endEnumeration,
      PrjfsTraceCallType::END_ENUMERATION,
      callbackData,
      nullptr,
      enumerationId);
}

HRESULT getEnumerationData(
    const PRJ_CALLBACK_DATA* callbackData,
    const GUID* enumerationId,
    PCWSTR searchExpression,
    PRJ_DIR_ENTRY_BUFFER_HANDLE dirEntryBufferHandle) noexcept {
  allowRecursiveCallbacks(callbackData);
  return runCallback(
      &PrjfsChannelInner::getEnumerationData,
      PrjfsTraceCallType::GET_ENUMERATION_DATA,
      callbackData,
      nullptr,
      enumerationId,
      searchExpression,
      dirEntryBufferHandle);
}

HRESULT getPlaceholderInfo(const PRJ_CALLBACK_DATA* callbackData) noexcept {
  allowRecursiveCallbacks(callbackData);
  return runCallback(
      &PrjfsChannelInner::getPlaceholderInfo,
      PrjfsTraceCallType::GET_PLACEHOLDER_INFO,
      callbackData,
      nullptr);
}

HRESULT queryFileName(const PRJ_CALLBACK_DATA* callbackData) noexcept {
  allowRecursiveCallbacks(callbackData);
  return runCallback(
      &PrjfsChannelInner::queryFileName,
      PrjfsTraceCallType::QUERY_FILE_NAME,
      callbackData,
      nullptr);
}

HRESULT getFileData(
    const PRJ_CALLBACK_DATA* callbackData,
    UINT64 byteOffset,
    UINT32 length) noexcept {
  allowRecursiveCallbacks(callbackData);
  return runCallback(
      &PrjfsChannelInner::getFileData,
      PrjfsTraceCallType::GET_FILE_DATA,
      callbackData,
      nullptr,
      byteOffset,
      length);
}

void cancelCommand(const PRJ_CALLBACK_DATA* callbackData) noexcept {
  allowRecursiveCallbacks(callbackData);
  // TODO(T67329233): Interrupt the future.
  XLOGF(
      DBG6, "Cancellation requested for command: {}", callbackData->CommandId);
}

namespace {
const std::unordered_map<PRJ_NOTIFICATION, PrjfsTraceCallType>
    notificationTypeMap = {
        {PRJ_NOTIFICATION_NEW_FILE_CREATED,
         PrjfsTraceCallType::NEW_FILE_CREATED},
        {PRJ_NOTIFICATION_PRE_DELETE, PrjfsTraceCallType::PRE_DELETE},
        {PRJ_NOTIFICATION_FILE_OVERWRITTEN,
         PrjfsTraceCallType::FILE_OVERWRITTEN},
        {PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED,
         PrjfsTraceCallType::FILE_HANDLE_CLOSED_FILE_MODIFIED},
        {PRJ_NOTIFICATION_FILE_RENAMED, PrjfsTraceCallType::FILE_RENAMED},
        {PRJ_NOTIFICATION_PRE_RENAME, PrjfsTraceCallType::PRE_RENAME},
        {PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED,
         PrjfsTraceCallType::FILE_HANDLE_CLOSED_FILE_DELETED},
        {PRJ_NOTIFICATION_PRE_SET_HARDLINK,
         PrjfsTraceCallType::PRE_SET_HARDLINK},
        {PRJ_NOTIFICATION_FILE_PRE_CONVERT_TO_FULL,
         PrjfsTraceCallType::FILE_PRE_CONVERT_TO_FULL}};
} // namespace

HRESULT notification(
    const PRJ_CALLBACK_DATA* callbackData,
    BOOLEAN isDirectory,
    PRJ_NOTIFICATION notificationType,
    PCWSTR destinationFileName,
    PRJ_NOTIFICATION_PARAMETERS* notificationParameters) noexcept {
  try {
    auto channel = getChannel(callbackData);
    if (!channel) {
      // TODO(zeyi): Something modified the working copy while it is being
      // unmounted. At this point, we have no way to deal with this properly
      // and the next time this repository is mounted, there will be a
      // discrepancy between what EdenFS thinks the state of the working copy
      // should be and what it actually is. To solve this, we will need to
      // scan the working copy at mount time to find these files and fixup
      // EdenFS inodes. Once the above is done, refactor this code to use
      // runCallback.
      EDEN_BUG() << "A notification was received while unmounting";
    }

    auto channelPtr = channel.get();
    auto typeIt = notificationTypeMap.find(notificationType);
    auto nType = PrjfsTraceCallType::INVALID;
    if (typeIt != notificationTypeMap.end()) {
      nType = typeIt->second;
    }
    auto context = std::make_shared<PrjfsRequestContext>(
        std::move(channel), *callbackData, nType, destinationFileName);
    return channelPtr->notification(
        std::move(context),
        callbackData,
        isDirectory,
        notificationType,
        destinationFileName,
        notificationParameters);
  } catch (const std::exception& ex) {
    return exceptionToHResult(ex);
  }
}

/**
 * Detach the passed in future onto the global CPU executor.
 */
void detachAndCompleteCallback(
    ImmediateFuture<folly::Unit> future,
    std::shared_ptr<PrjfsRequestContext> context,
    EdenStatsPtr stats,
    StatsGroupBase::Counter PrjfsStats::* countSuccessful,
    StatsGroupBase::Counter PrjfsStats::* countFailure) {
  auto completionFuture = context
                              ->catchErrors(
                                  std::move(future),
                                  std::move(stats),
                                  countSuccessful,
                                  countFailure)
                              .ensure([context = std::move(context)]() {});
  if (!completionFuture.isReady()) {
    folly::futures::detachOnGlobalCPUExecutor(
        std::move(completionFuture).semi());
  }
}

} // namespace

PrjfsChannelInner::PrjfsChannelInner(
    std::unique_ptr<PrjfsDispatcher> dispatcher,
    const folly::Logger* straceLogger,
    const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
    FaultInjector& faultInjector,
    std::shared_ptr<ProcessAccessLog> processAccessLog,
    std::shared_ptr<ReloadableConfig>& config,
    folly::Promise<folly::Unit> deletedPromise,
    std::shared_ptr<Notifier> notifier,
    size_t prjfsTraceBusCapacity,
    const std::shared_ptr<folly::Executor>& invalidationThreadPool)
    : dispatcher_(std::move(dispatcher)),
      straceLogger_(straceLogger),
      edenFsEventsLogger_(edenFsEventsLogger),
      faultInjector_(faultInjector),
      invalidationThreadPool_(invalidationThreadPool),
      lastTornReadLog_(
          std::make_shared<
              folly::Synchronized<std::chrono::steady_clock::time_point>>()),
      notifier_(std::move(notifier)),
      processAccessLog_(std::move(processAccessLog)),
      config_(config),
      deletedPromise_(std::move(deletedPromise)),
      traceDetailedArguments_(std::make_shared<std::atomic<size_t>>(0)),
      traceBus_(
          TraceBus<PrjfsTraceEvent>::create(
              "PrjfsTrace",
              prjfsTraceBusCapacity)),
      longRunningFSRequestThreshold_(
          config_->getEdenConfig()->longRunningFSRequestThreshold.getValue()) {
  traceSubscriptionHandles_.push_back(traceBus_->subscribeFunction(
      "PrjFS request tracking", [this](const PrjfsTraceEvent& event) {
        switch (event.getType()) {
          case PrjfsTraceEvent::START: {
            auto state = telemetryState_.wlock();
            state->requests.emplace(
                event.getData().commandId,
                OutstandingRequest{event.getCallType(), event.getData()});
            break;
          }
          case PrjfsTraceEvent::FINISH: {
            auto state = telemetryState_.wlock();
            auto erased = state->requests.erase(event.getData().commandId);
            XCHECK(erased) << "duplicate prjfs finish event";
            break;
          }
        }
      }));
}

PrjfsChannelInner::~PrjfsChannelInner() {
  if (mountChannel_) {
    PrjStopVirtualizing(mountChannel_);
    deletedPromise_.setValue(folly::unit);
  }
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::waitForPendingNotifications() {
  return dispatcher_->waitForPendingNotifications();
}

folly::coro::now_task<folly::Unit>
PrjfsChannelInner::co_waitForPendingNotifications() {
  co_return co_await dispatcher_->co_waitForPendingNotifications();
}

HRESULT PrjfsChannelInner::startEnumeration(
    std::shared_ptr<PrjfsRequestContext> context,
    const PRJ_CALLBACK_DATA* callbackData,
    const GUID* enumerationId) {
  auto guid = Guid(*enumerationId);
  auto path = RelativePath(callbackData->FilePathName);
  auto fut = makeImmediateFutureWith([this,
                                      context,
                                      guid = std::move(guid),
                                      path = std::move(path)]() mutable {
    auto requestWatch =
        std::shared_ptr<RequestMetricsScope::LockedRequestWatchList>(nullptr);
    context->startRequest(
        getStats().copy(), &PrjfsStats::openDir, requestWatch);

    FB_LOGF(
        getStraceLogger(), DBG7, "opendir({}, guid={})", path, guid.toString());
    return dispatcher_
        ->opendir(std::move(path), context->getObjectFetchContext())
        .thenValue([this, context = std::move(context), guid = std::move(guid)](
                       auto&& dirents) {
          addDirectoryEnumeration(std::move(guid), std::move(dirents));
          context->sendSuccess();
        });
  });

  detachAndCompleteCallback(
      std::move(fut),
      std::move(context),
      getStats().copy(),
      &PrjfsStats::openDirSuccessful,
      &PrjfsStats::openDirFailure);

  return HRESULT_FROM_WIN32(ERROR_IO_PENDING);
}

HRESULT PrjfsChannelInner::endEnumeration(
    std::shared_ptr<PrjfsRequestContext> /* context */,
    const PRJ_CALLBACK_DATA* /* callbackData */,
    const GUID* enumerationId) {
  auto guid = Guid(*enumerationId);
  FB_LOGF(getStraceLogger(), DBG7, "closedir({})", guid.toString());

  removeDirectoryEnumeration(guid);

  return S_OK;
}

namespace {
LARGE_INTEGER timespecToPrjTime(EdenTimestamp time) {
  auto filetime = time.toFileTime();
  LARGE_INTEGER res;
  res.QuadPart = (filetime.tv_sec * 1000000000ull + filetime.tv_nsec) / 100ull;
  return res;
}
} // namespace

HRESULT PrjfsChannelInner::getEnumerationData(
    std::shared_ptr<PrjfsRequestContext> context,
    const PRJ_CALLBACK_DATA* callbackData,
    const GUID* enumerationId,
    PCWSTR searchExpression,
    PRJ_DIR_ENTRY_BUFFER_HANDLE dirEntryBufferHandle) {
  auto guid = Guid(*enumerationId);

  FB_LOGF(
      getStraceLogger(),
      DBG7,
      "readdir({}, searchExpression={})",
      guid.toString(),
      searchExpression == nullptr
          ? "<nullptr>"
          : wideToMultibyteString<std::string>(searchExpression));

  auto optEnumerator = findDirectoryEnumeration(guid);
  if (!optEnumerator.has_value()) {
    XLOGF(DBG5, "Directory enumeration not found: {}", guid);
    return HRESULT_FROM_WIN32(ERROR_INVALID_PARAMETER);
  }
  auto enumerator = std::move(optEnumerator).value();

  auto shouldRestart =
      bool(callbackData->Flags & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN);
  if (enumerator->isSearchExpressionEmpty() || shouldRestart) {
    if (searchExpression != nullptr) {
      enumerator->saveExpression(searchExpression);
    } else {
      enumerator->saveExpression(L"*");
    }
  }

  if (shouldRestart) {
    enumerator->restartEnumeration();
  }

  auto fut = makeImmediateFutureWith([this,
                                      context,
                                      enumerator = std::move(enumerator),
                                      buffer = dirEntryBufferHandle]() mutable {
    auto requestWatch =
        std::shared_ptr<RequestMetricsScope::LockedRequestWatchList>(nullptr);
    context->startRequest(
        getStats().copy(), &PrjfsStats::readDir, requestWatch);

    return enumerator->prepareEnumeration().thenValue(
        [this,
         buffer,
         context = std::move(context),
         enumerator =
             std::move(enumerator)](std::shared_ptr<Enumeration> enumeration) {
          bool added = false;
          auto timestamp = dispatcher_->getLastCheckoutTime();
          auto prjTime = timespecToPrjTime(timestamp);

          for (auto optEntry = enumeration->getCurrent(); optEntry.has_value();
               optEntry = enumeration->getNext()) {
            auto& entry = optEntry.value();

            auto fileInfo = PRJ_FILE_BASIC_INFO();
            fileInfo.IsDirectory = entry.isDir;
            fileInfo.FileSize = entry.size;
            fileInfo.CreationTime = prjTime;
            fileInfo.LastWriteTime = prjTime;
            fileInfo.ChangeTime = prjTime;

            XLOGF(
                DBG6,
                "Directory entry: {}, {}, size={}",
                fileInfo.IsDirectory ? "Dir" : "File",
                PathComponent(entry.name),
                fileInfo.FileSize);

            HRESULT result = 0;
            if (entry.symlinkTarget.has_value()) {
              auto content = entry.symlinkTarget.value();
              fileInfo.FileSize = 0;
              _PRJ_EXTENDED_INFO extInfo;
              extInfo.Symlink.TargetName =
                  std::wstring(content.begin(), content.end()).c_str();
              extInfo.InfoType = _PRJ_EXT_INFO_TYPE::PRJ_EXT_INFO_TYPE_SYMLINK;
              extInfo.NextInfoOffset = 0;
              result = PrjFillDirEntryBuffer2(
                  buffer, entry.name.c_str(), &fileInfo, &extInfo);
            } else {
              result =
                  PrjFillDirEntryBuffer(entry.name.c_str(), &fileInfo, buffer);
            }
            if (FAILED(result)) {
              if (result == HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER) &&
                  added) {
                // We are out of buffer space. This entry didn't make it. Return
                // without increment.
                break;
              } else {
                return folly::Try<folly::Unit>{makeHResultErrorExplicit(
                    result,
                    fmt::format(
                        FMT_STRING("Adding directory entry {}"),
                        PathComponent(entry.name)))};
              }
            }
            added = true;
          }

          context->sendEnumerationSuccess(buffer);
          return folly::Try{folly::unit};
        });
  });

  detachAndCompleteCallback(
      std::move(fut),
      std::move(context),
      getStats().copy(),
      &PrjfsStats::readDirSuccessful,
      &PrjfsStats::readDirFailure);

  return HRESULT_FROM_WIN32(ERROR_IO_PENDING);
}

HRESULT PrjfsChannelInner::getPlaceholderInfo(
    std::shared_ptr<PrjfsRequestContext> context,
    const PRJ_CALLBACK_DATA* callbackData) {
  auto path = RelativePath(callbackData->FilePathName);
  auto virtualizationContext = callbackData->NamespaceVirtualizationContext;

  auto fut = makeImmediateFutureWith([this,
                                      context,
                                      path = std::move(path),
                                      virtualizationContext]() mutable {
    auto requestWatch =
        std::shared_ptr<RequestMetricsScope::LockedRequestWatchList>(nullptr);
    context->startRequest(getStats().copy(), &PrjfsStats::lookup, requestWatch);

    FB_LOGF(getStraceLogger(), DBG7, "lookup({})", path);
    return dispatcher_
        ->lookup(std::move(path), context->getObjectFetchContext())
        .thenValue(
            [this, context, virtualizationContext = virtualizationContext](
                std::optional<LookupResult>&& optLookupResult)
                -> ImmediateFuture<folly::Unit> {
              if (!optLookupResult) {
                context->sendError(HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND));
                return folly::unit;
              }
              const auto& lookupResult = optLookupResult.value();

              auto timestamp = dispatcher_->getLastCheckoutTime();
              auto prjTime = timespecToPrjTime(timestamp);

              PRJ_PLACEHOLDER_INFO placeholderInfo{};
              placeholderInfo.FileBasicInfo.IsDirectory = lookupResult.isDir;
              placeholderInfo.FileBasicInfo.FileSize = lookupResult.size;
              placeholderInfo.FileBasicInfo.CreationTime = prjTime;
              placeholderInfo.FileBasicInfo.LastWriteTime = prjTime;
              placeholderInfo.FileBasicInfo.ChangeTime = prjTime;
              auto inodeName = lookupResult.path.wide();

              HRESULT result;
              if (symlinksSupported() &&
                  lookupResult.symlinkDestination.has_value()) {
                std::string content = lookupResult.symlinkDestination.value();
                std::wstring wcontent =
                    std::wstring(content.begin(), content.end());
                auto targetName = std::wstring(content.begin(), content.end());
                _PRJ_EXTENDED_INFO extInfo;
                extInfo.Symlink.TargetName = targetName.c_str();
                extInfo.InfoType =
                    _PRJ_EXT_INFO_TYPE::PRJ_EXT_INFO_TYPE_SYMLINK;
                extInfo.NextInfoOffset = 0;
                result = PrjWritePlaceholderInfo2(
                    virtualizationContext,
                    inodeName.c_str(),
                    &placeholderInfo,
                    sizeof(placeholderInfo),
                    &extInfo);
              } else {
                result = PrjWritePlaceholderInfo(
                    virtualizationContext,
                    inodeName.c_str(),
                    &placeholderInfo,
                    sizeof(placeholderInfo));
              }

              if (FAILED(result)) {
                return makeImmediateFuture<folly::Unit>(
                    makeHResultErrorExplicit(
                        result,
                        fmt::format(
                            FMT_STRING("Writing placeholder for {}"),
                            lookupResult.path)));
              }

              context->sendSuccess();

              return folly::unit;
            });
  });

  detachAndCompleteCallback(
      std::move(fut),
      std::move(context),
      getStats().copy(),
      &PrjfsStats::lookupSuccessful,
      &PrjfsStats::lookupFailure);

  return HRESULT_FROM_WIN32(ERROR_IO_PENDING);
}

HRESULT PrjfsChannelInner::queryFileName(
    std::shared_ptr<PrjfsRequestContext> context,
    const PRJ_CALLBACK_DATA* callbackData) {
  auto path = RelativePath(callbackData->FilePathName);

  auto fut = makeImmediateFutureWith([this,
                                      context,
                                      path = std::move(path)]() mutable {
    auto requestWatch =
        std::shared_ptr<RequestMetricsScope::LockedRequestWatchList>(nullptr);
    context->startRequest(getStats().copy(), &PrjfsStats::access, requestWatch);
    FB_LOGF(getStraceLogger(), DBG7, "access({})", path);
    return dispatcher_
        ->access(std::move(path), context->getObjectFetchContext())
        .thenValue([context = std::move(context)](bool present) {
          if (present) {
            context->sendSuccess();
          } else {
            context->sendError(HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND));
          }
        });
  });

  detachAndCompleteCallback(
      std::move(fut),
      std::move(context),
      getStats().copy(),
      &PrjfsStats::accessSuccessful,
      &PrjfsStats::accessFailure);

  return HRESULT_FROM_WIN32(ERROR_IO_PENDING);
}

namespace {

struct PrjAlignedBufferDeleter {
  void operator()(void* buffer) noexcept {
    ::PrjFreeAlignedBuffer(buffer);
  }
};

HRESULT readMultipleFileChunks(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT namespaceVirtualizationContext,
    const Guid& dataStreamId,
    const std::string& content,
    uint64_t startOffset,
    uint64_t length,
    uint64_t chunkSize) {
  HRESULT result;
  std::unique_ptr<void, PrjAlignedBufferDeleter> writeBuffer{
      PrjAllocateAlignedBuffer(namespaceVirtualizationContext, chunkSize)};

  if (writeBuffer.get() == nullptr) {
    return E_OUTOFMEMORY;
  }

  uint64_t remainingLength = length;

  while (remainingLength > 0) {
    uint64_t copySize = std::min(remainingLength, chunkSize);

    //
    // TODO(puneetk): Once backing store has the support for chunking the file
    // contents, we can read the chunks of large files here and then write
    // them to FS.
    //
    // TODO(puneetk): Build an interface to backing store so that we can pass
    // the aligned buffer to avoid coping here.
    //
    RtlCopyMemory(writeBuffer.get(), content.data() + startOffset, copySize);

    // Write the data to the file in the local file system.
    result = PrjWriteFileData(
        namespaceVirtualizationContext,
        dataStreamId,
        writeBuffer.get(),
        startOffset,
        folly::to_narrow(copySize));

    if (FAILED(result)) {
      return result;
    }

    remainingLength -= copySize;
    startOffset += copySize;
  }

  return S_OK;
}

HRESULT readSingleFileChunk(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT namespaceVirtualizationContext,
    const Guid& dataStreamId,
    const std::string& content,
    uint64_t startOffset,
    uint64_t length) {
  return readMultipleFileChunks(
      namespaceVirtualizationContext,
      dataStreamId,
      content,
      /*startOffset=*/startOffset,
      /*length=*/length,
      /*writeLength=*/length);
}

uint64_t BlockAlignTruncate(uint64_t ptr, uint32_t alignment) {
  return ((ptr) & (0 - (static_cast<uint64_t>(alignment))));
}

constexpr uint32_t kMinChunkSize = 512 * 1024; // 512 KiB
constexpr uint32_t kMaxChunkSize = 5 * 1024 * 1024; // 5 MiB

folly::Try<folly::Unit> removeCachedFileImpl(
    folly::ReadMostlySharedPtr<PrjfsChannelInner>& inner,
    RelativePathPiece path) {
  if (!inner) {
    // TODO: The mount is being unmounted but the caller is still manipulating
    // it. This is unexpected. Not totally unexpected for background
    // invalidations, but strange for checkout.
    return folly::Try<folly::Unit>{std::runtime_error{fmt::format(
        FMT_STRING("Couldn't delete file {}: PrjfsChannel is stopped"), path)}};
  }

  DurationScope<EdenStats> statScope{
      inner->getStats(), &PrjfsStats::removeCachedFile};

  if (path.empty()) {
    return folly::Try<folly::Unit>{folly::unit};
  }

  auto winPath = path.wide();

  XLOGF(DBG6, "Invalidating: {}", path);

  PRJ_UPDATE_FAILURE_CAUSES failureReason;
  auto result = PrjDeleteFile(
      inner->getMountChannel(),
      winPath.c_str(),
      PRJ_UPDATE_ALLOW_DIRTY_METADATA | PRJ_UPDATE_ALLOW_DIRTY_DATA |
          PRJ_UPDATE_ALLOW_READ_ONLY | PRJ_UPDATE_ALLOW_TOMBSTONE,
      &failureReason);
  if (FAILED(result)) {
    if (result == HRESULT_FROM_WIN32(ERROR_REPARSE_POINT_ENCOUNTERED)) {
      // We've attempted to call PrjDeleteFile on a directory. That isn't
      // supported, let's just ignore.
    } else if (
        result == HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND) ||
        result == HRESULT_FROM_WIN32(ERROR_PATH_NOT_FOUND)) {
      // The file or a directory in the path is not cached, ignore.
    } else if (result == HRESULT_FROM_WIN32(ERROR_DIR_NOT_EMPTY)) {
      inner->getStats()->increment(&PrjfsStats::removeCachedFileFailure);
      return folly::Try<folly::Unit>{
          std::system_error(ENOTEMPTY, std::generic_category())};
    } else {
      inner->getStats()->increment(&PrjfsStats::removeCachedFileFailure);
      return folly::Try<folly::Unit>{makeHResultErrorExplicit(
          result,
          fmt::format(
              FMT_STRING("Couldn't delete file {}: {:#x}"),
              path,
              static_cast<uint32_t>(result)))};
    }
  }

  inner->getStats()->increment(&PrjfsStats::removeCachedFileSuccessful);
  return folly::Try<folly::Unit>{folly::unit};
}

} // namespace

HRESULT PrjfsChannelInner::getFileData(
    std::shared_ptr<PrjfsRequestContext> context,
    const PRJ_CALLBACK_DATA* callbackData,
    UINT64 byteOffset,
    UINT32 length) {
  auto fut = makeImmediateFutureWith([this,
                                      context,
                                      path = RelativePath(
                                          callbackData->FilePathName),
                                      virtualizationContext =
                                          callbackData
                                              ->NamespaceVirtualizationContext,
                                      dataStreamId =
                                          Guid(callbackData->DataStreamId),
                                      clientProcessName =
                                          std::wstring{
                                              callbackData
                                                  ->TriggeringProcessImageFileName},
                                      byteOffset,
                                      length]() mutable {
    auto requestWatch =
        std::shared_ptr<RequestMetricsScope::LockedRequestWatchList>(nullptr);
    context->startRequest(getStats().copy(), &PrjfsStats::read, requestWatch);

    FB_LOGF(
        getStraceLogger(),
        DBG7,
        "read({}, off={}, len={})",
        path,
        byteOffset,
        length);
    return dispatcher_->read(path, context->getObjectFetchContext())
        .thenValue([context = std::move(context),
                    virtualizationContext = virtualizationContext,
                    dataStreamId = std::move(dataStreamId),
                    byteOffset = byteOffset,
                    length = length,
                    path,
                    edenFsEventsLogger = edenFsEventsLogger_,
                    clientProcessName = std::move(clientProcessName),
                    lastTornReadLog = lastTornReadLog_,
                    config = config_,
                    mountChannel = getMountChannel(),
                    invalidationThreadPool = this->invalidationThreadPool_](
                       const std::string content) {
          if ((content.length() - byteOffset) < length) {
            auto now = std::chrono::steady_clock::now();

            // These most likely come fround background tooling reads, so
            // it's likely that there will be many at once. During one
            // checkout operation we might see a bunch of torn reads all at
            // once. We only log one per 10s to avoid spamming logs/scuba.
            bool shouldLog = false;
            {
              auto last = lastTornReadLog->wlock();
              if (now >= *last +
                      config->getEdenConfig()
                          ->prjfsTornReadLogInterval.getValue()) {
                shouldLog = true;
                *last = now;
              }
            }
            if (shouldLog) {
              auto client = wideToMultibyteString<std::string>(
                  basenameFromAppName(clientProcessName.c_str()));
              XLOGF(
                  DBG2,
                  "PrjFS asked us to read {} bytes out of {}, but there are only "
                  "{} bytes available in this file. Reading the file likely raced "
                  "with checkout/reset. Client process: {}. ",
                  length,
                  path,
                  content.length(),
                  client);
              edenFsEventsLogger->logEvent(
                  PrjFSCheckoutReadRace{std::move(client)});
            }

            // This error currently gets propagated to the user. Ideally, we
            // don't want to fail this read. However, if the requested
            // length is larger than the actual size of the file and we give
            // PrjFS less data than it expects, PrjFs/Windows going to add 0
            // bytes to the end of the file. The file will then be corrupted
            // and out of sync. The only way we can prevent the file from
            // being out of sync and corrupted is to error in this case.
            context->sendError(HRESULT_FROM_WIN32(ERROR_INVALID_PARAMETER));

            // All future reads will run into this error until the kernel's
            // cache of the file size is cleared. That means one poorly timed
            // read during checkout of a file makes the file inaccessible to
            // future reads. We trigger an invalidation of the file here to
            // ensure that future reads will succeed.
            auto timeToSleep =
                std::chrono::duration_cast<folly::HighResDuration>(
                    config->getEdenConfig()->tornReadCleanupDelay.getValue());
            // Clients will hold file handles open until we return the above
            // error. From manual testing, handles are still held at this point.
            // The invalidation fails if the handle is still open,  so we
            // artificially delay invalidation in hopes that the handle is
            // closed.
            //
            // We also run the invalidation on a separate thread to protect Eden
            // against PrjFS re-entrancy. i.e. If PrjFS makes a callback to
            // Eden during the invalidation, we don't want to be blocking the
            // same thread pool that needs to handle that callback. PrjFS
            // is probably not re-entrant in that way, but better safe than
            // sorry.
            //
            // lifetime note: we need to capture the thread pool to ensure that
            // it lives long enough to execute this callback because otherwise
            // it might be destroyed before the callback runs on eden shutdown.
            folly::futures::detachOn(
                invalidationThreadPool.get(),
                folly::futures::sleep(timeToSleep)
                    .deferValue(
                        [prjfsInner = context->getChannelForAsyncUse(), path](
                            auto&&) mutable -> folly::SemiFuture<folly::Unit> {
                          // Lifetimes are a bit tricky. Since the pointer is
                          // weak, it does not keep the mount alive like a
                          // shared pointer would. We don't wanna block shutdown
                          // on this invalidation because FSCK can fix it. That
                          // means we should handle the case where we can't
                          // acquire the pointer gracefully.
                          auto inner = prjfsInner.lock();
                          if (!inner) {
                            // This means the mount has been shutdown, there is
                            // not much we can do other than skip the
                            // invalidation. FSCK should fix it on the next
                            // startup.
                            return folly::unit;
                          }
                          // From here on out, we would block shutdown, so we
                          // better be quick.
                          return inner->faultInjector_
                              .checkAsync(
                                  "PrjFSChannelInner::getFileData-invalidation",
                                  path)
                              .semi()
                              .deferValue([inner, path](auto&&) mutable {
                                XLOG(DBG2, "Invalidating file with torn read.");
                                // this might fail for example if there is an
                                // open handle to the file still. The file will
                                // stay in the bad state and the user will have
                                // to run eden doctor to fix it.

                                // Just like during checkout:
                                // TODO: In the case where the file becomes
                                // materialized on disk now,
                                // invalidateChannelEntryCache will happily
                                // remove it, leading to a potential loss of
                                // user data. To avoid this, we could try not
                                // passing PRJ_UPDATE_ALLOW_DIRTY_DATA and
                                // dealing with the side effects to close that
                                // race.
                                try {
                                  removeCachedFileImpl(inner, path);
                                } catch (std::exception& ex) {
                                  XLOGF(
                                      DBG3,
                                      "Failed to invalidate file post torn read {} : {}",
                                      path,
                                      folly::exceptionStr(ex));
                                }
                              });
                        })
                    .deferEnsure([invalidationThreadPool]() {}));

            return;
          }
          // Note it's possible that PrjFs could be out of sync with EdenFS in
          // the opposite way from above (PrjFS thinks the file length is
          // shorter than EdenFS). That is still going to result in a corrupt
          // file (truncated). That case is indistinguishable from ProjFs just
          // requesting a portion of the file, so we can't raise an error here.
          // We need to prevent that corruption elsewhere - failing the checkout
          // that de-syncs Eden and ProjFs or storing the version of the file in
          // the placeholder.

          //
          // We should return file data which is smaller than
          // our kMaxChunkSize and meets the memory alignment
          // requirements of the virtualization instance's storage
          // device.
          //

          HRESULT result;
          if (content.length() <= kMinChunkSize) {
            //
            // If the file is small - copy the whole file in one shot.
            //
            result = readSingleFileChunk(
                virtualizationContext,
                dataStreamId,
                content,
                /*startOffset=*/0,
                /*writeLength=*/content.length());

          } else if (length <= kMaxChunkSize) {
            //
            // If the request is with in our kMaxChunkSize - copy the
            // entire request.
            //
            result = readSingleFileChunk(
                virtualizationContext,
                dataStreamId,
                content,
                /*startOffset=*/byteOffset,
                /*writeLength=*/length);
          } else {
            //
            // When the request is larger than kMaxChunkSize we split
            // the request into multiple chunks.
            //
            PRJ_VIRTUALIZATION_INSTANCE_INFO instanceInfo;
            result = PrjGetVirtualizationInstanceInfo(
                virtualizationContext, &instanceInfo);

            if (SUCCEEDED(result)) {
              uint64_t startOffset = byteOffset;
              uint64_t endOffset = BlockAlignTruncate(
                  startOffset + kMaxChunkSize, instanceInfo.WriteAlignment);
              XDCHECK_GT(endOffset, 0ul);
              XDCHECK_GT(endOffset, startOffset);

              uint64_t chunkSize = endOffset - startOffset;
              result = readMultipleFileChunks(
                  virtualizationContext,
                  dataStreamId,
                  content,
                  /*startOffset=*/startOffset,
                  /*length=*/length,
                  /*chunkSize=*/chunkSize);
            }
          }

          if (FAILED(result)) {
            context->sendError(result);
          } else {
            context->sendSuccess();
          }
        });
  });

  detachAndCompleteCallback(
      std::move(fut),
      std::move(context),
      getStats().copy(),
      &PrjfsStats::readSuccessful,
      &PrjfsStats::readFailure);

  return HRESULT_FROM_WIN32(ERROR_IO_PENDING);
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::matchEdenViewOfFileToFS(
    RelativePath relPath,
    const ObjectFetchContextPtr& context) {
  return ImmediateFuture{
      dispatcher_->matchEdenViewOfFileToFS(std::move(relPath), context)
          .semi()
          .via(dispatcher_->getNotificationExecutor())
          .semi()};
}

std::vector<PrjfsChannelInner::OutstandingRequest>
PrjfsChannelInner::getOutstandingRequests() {
  std::vector<PrjfsChannelInner::OutstandingRequest> outstandingCalls;

  auto telemetryStateLockedPtr = telemetryState_.rlock();
  for (const auto& entry : telemetryStateLockedPtr->requests) {
    outstandingCalls.push_back(entry.second);
  }
  return outstandingCalls;
}

TraceDetailedArgumentsHandle PrjfsChannelInner::traceDetailedArguments() {
  auto counter = traceDetailedArguments_;
  auto handle = std::shared_ptr<void>(nullptr, [counter](void*) {
    counter->fetch_sub(1, std::memory_order_acq_rel);
  });
  traceDetailedArguments_->fetch_add(1, std::memory_order_acq_rel);
  return handle;
}

namespace {
typedef ImmediateFuture<folly::Unit> (PrjfsChannelInner::*NotificationHandler)(
    RelativePath oldPath,
    RelativePath destPath,
    bool isDirectory,
    const ObjectFetchContextPtr& context);

typedef std::string (*NotificationArgRenderer)(
    RelativePathPiece relPath,
    RelativePathPiece destPath,
    bool isDirectory);

struct NotificationHandlerEntry {
  constexpr NotificationHandlerEntry(
      NotificationHandler h,
      NotificationArgRenderer r,
      PrjfsStats::DurationPtr d,
      PrjfsStats::CounterPtr cS,
      PrjfsStats::CounterPtr cF)
      : handler{h},
        renderer{r},
        duration{d},
        countSuccessful{cS},
        countFailure{cF} {}

  NotificationHandler handler;
  NotificationArgRenderer renderer;
  PrjfsStats::DurationPtr duration;
  PrjfsStats::CounterPtr countSuccessful;
  PrjfsStats::CounterPtr countFailure;
};

std::string newFileCreatedRenderer(
    RelativePathPiece relPath,
    RelativePathPiece /*destPath*/,
    bool isDirectory) {
  return fmt::format(
      FMT_STRING("{}Created({})"), isDirectory ? "dir" : "file", relPath);
}

std::string fileOverwrittenRenderer(
    RelativePathPiece relPath,
    RelativePathPiece /*destPath*/,
    bool /*isDirectory*/) {
  return fmt::format(FMT_STRING("fileOverwritten({})"), relPath);
}

std::string fileHandleClosedFileModifiedRenderer(
    RelativePathPiece relPath,
    RelativePathPiece /*destPath*/,
    bool /*isDirectory*/) {
  return fmt::format(FMT_STRING("fileModified({})"), relPath);
}

std::string fileRenamedRenderer(
    RelativePathPiece oldPath,
    RelativePathPiece newPath,
    bool /*isDirectory*/) {
  return fmt::format(FMT_STRING("fileRenamed({} -> {})"), oldPath, newPath);
}

std::string preRenameRenderer(
    RelativePathPiece oldPath,
    RelativePathPiece newPath,
    bool /*isDirectory*/) {
  return fmt::format(FMT_STRING("preRename({} -> {})"), oldPath, newPath);
}

std::string fileHandleClosedFileDeletedRenderer(
    RelativePathPiece relPath,
    RelativePathPiece /*destPath*/,
    bool isDirectory) {
  return fmt::format(
      FMT_STRING("{}Deleted({})"), isDirectory ? "dir" : "file", relPath);
}

std::string preDeleteRenderer(
    RelativePathPiece relPath,
    RelativePathPiece /*destPath*/,
    bool isDirectory) {
  return fmt::format(
      FMT_STRING("pre{}Deleted({})"), isDirectory ? "Dir" : "File", relPath);
}

std::string preSetHardlinkRenderer(
    RelativePathPiece oldPath,
    RelativePathPiece newPath,
    bool /*isDirectory*/) {
  return fmt::format(FMT_STRING("link({} -> {})"), oldPath, newPath);
}

std::string preConvertToFullRenderer(
    RelativePathPiece relPath,
    RelativePathPiece /*destPath*/,
    bool isDirectory) {
  return fmt::format(
      FMT_STRING("preConvertToFull({}, isDirectory={})"), relPath, isDirectory);
}

const std::unordered_map<PRJ_NOTIFICATION, NotificationHandlerEntry>
    notificationHandlerMap = {
        {
            PRJ_NOTIFICATION_NEW_FILE_CREATED,
            {&PrjfsChannelInner::newFileCreated,
             newFileCreatedRenderer,
             &PrjfsStats::newFileCreated,
             &PrjfsStats::newFileCreatedSuccessful,
             &PrjfsStats::newFileCreatedFailure},
        },
        {
            PRJ_NOTIFICATION_PRE_DELETE,
            {&PrjfsChannelInner::preDelete,
             preDeleteRenderer,
             &PrjfsStats::preDelete,
             &PrjfsStats::preDeleteSuccessful,
             &PrjfsStats::preDeleteFailure},
        },
        {
            PRJ_NOTIFICATION_FILE_OVERWRITTEN,
            {&PrjfsChannelInner::fileOverwritten,
             fileOverwrittenRenderer,
             &PrjfsStats::fileOverwritten,
             &PrjfsStats::fileOverwrittenSuccessful,
             &PrjfsStats::fileOverwrittenFailure},
        },
        {
            PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED,
            {&PrjfsChannelInner::fileHandleClosedFileModified,
             fileHandleClosedFileModifiedRenderer,
             &PrjfsStats::fileHandleClosedFileModified,
             &PrjfsStats::fileHandleClosedFileModifiedSuccessful,
             &PrjfsStats::fileHandleClosedFileModifiedFailure},
        },
        {
            PRJ_NOTIFICATION_FILE_RENAMED,
            {&PrjfsChannelInner::fileRenamed,
             fileRenamedRenderer,
             &PrjfsStats::fileRenamed,
             &PrjfsStats::fileRenamedSuccessful,
             &PrjfsStats::fileRenamedFailure},
        },
        {
            PRJ_NOTIFICATION_PRE_RENAME,
            {&PrjfsChannelInner::preRename,
             preRenameRenderer,
             &PrjfsStats::preRenamed,
             &PrjfsStats::preRenamedSuccessful,
             &PrjfsStats::preRenamedFailure},
        },
        {
            PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED,
            {&PrjfsChannelInner::fileHandleClosedFileDeleted,
             fileHandleClosedFileDeletedRenderer,
             &PrjfsStats::fileHandleClosedFileDeleted,
             &PrjfsStats::fileHandleClosedFileDeletedSuccessful,
             &PrjfsStats::fileHandleClosedFileDeletedFailure},
        },
        {
            PRJ_NOTIFICATION_PRE_SET_HARDLINK,
            {&PrjfsChannelInner::preSetHardlink,
             preSetHardlinkRenderer,
             &PrjfsStats::preSetHardlink,
             &PrjfsStats::preSetHardlinkSuccessful,
             &PrjfsStats::preSetHardlinkFailure},
        },
        {
            PRJ_NOTIFICATION_FILE_PRE_CONVERT_TO_FULL,
            {&PrjfsChannelInner::preConvertToFull,
             preConvertToFullRenderer,
             &PrjfsStats::preConvertToFull,
             &PrjfsStats::preConvertToFullSuccessful,
             &PrjfsStats::preConvertToFullFailure},
        },
};
} // namespace

ImmediateFuture<folly::Unit> PrjfsChannelInner::newFileCreated(
    RelativePath relPath,
    RelativePath /*destPath*/,
    bool isDirectory,
    const ObjectFetchContextPtr& context) {
  if (isDirectory) {
    return dispatcher_->dirCreated(std::move(relPath), context);
  } else {
    return dispatcher_->fileCreated(std::move(relPath), context);
  }
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::fileOverwritten(
    RelativePath relPath,
    RelativePath /*destPath*/,
    bool /*isDirectory*/,
    const ObjectFetchContextPtr& context) {
  return dispatcher_->fileModified(std::move(relPath), context);
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::fileHandleClosedFileModified(
    RelativePath relPath,
    RelativePath /*destPath*/,
    bool /*isDirectory*/,
    const ObjectFetchContextPtr& context) {
  return dispatcher_->fileModified(std::move(relPath), context);
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::fileRenamed(
    RelativePath oldPath,
    RelativePath newPath,
    bool isDirectory,
    const ObjectFetchContextPtr& context) {
  // When files are moved in and out of the repo, the rename paths are
  // empty, handle these like creation/removal of files.
  if (oldPath.empty()) {
    return newFileCreated(
        std::move(newPath), RelativePath{}, isDirectory, context);
  } else if (newPath.empty()) {
    return fileHandleClosedFileDeleted(
        std::move(oldPath), RelativePath{}, isDirectory, context);
  } else {
    return dispatcher_->fileRenamed(
        std::move(oldPath), std::move(newPath), context);
  }
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::preRename(
    RelativePath oldPath,
    RelativePath newPath,
    bool isDirectory,
    const ObjectFetchContextPtr& context) {
  if (isDirectory) {
    return dispatcher_->preDirRename(
        std::move(oldPath), std::move(newPath), context);
  } else {
    return dispatcher_->preFileRename(
        std::move(oldPath), std::move(newPath), context);
  }
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::preDelete(
    RelativePath relPath,
    RelativePath /*destPath*/,
    bool isDirectory,
    const ObjectFetchContextPtr& context) {
  if (isDirectory) {
    return dispatcher_->preDirDelete(std::move(relPath), context);
  } else {
    return dispatcher_->preFileDelete(std::move(relPath), context);
  }
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::fileHandleClosedFileDeleted(
    RelativePath oldPath,
    RelativePath /*destPath*/,
    bool isDirectory,
    const ObjectFetchContextPtr& context) {
  if (isDirectory) {
    return dispatcher_->dirDeleted(std::move(oldPath), context);
  } else {
    return dispatcher_->fileDeleted(std::move(oldPath), context);
  }
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::preSetHardlink(
    RelativePath relPath,
    RelativePath /*newPath*/,
    bool /*isDirectory*/,
    const ObjectFetchContextPtr& /*context*/) {
  return folly::Try<folly::Unit>(makeHResultErrorExplicit(
      HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED),
      fmt::format(FMT_STRING("Hardlinks are not supported: {}"), relPath)));
}

ImmediateFuture<folly::Unit> PrjfsChannelInner::preConvertToFull(
    RelativePath relpath,
    RelativePath /*destPath*/,
    bool /*isDirectory*/,
    const ObjectFetchContextPtr& context) {
  return dispatcher_->preFileConvertedToFull(std::move(relpath), context);
}

HRESULT PrjfsChannelInner::notification(
    std::shared_ptr<PrjfsRequestContext> context,
    const PRJ_CALLBACK_DATA* callbackData,
    BOOLEAN isDirectory,
    PRJ_NOTIFICATION notificationType,
    PCWSTR destinationFileName,
    PRJ_NOTIFICATION_PARAMETERS* /*notificationParameters*/) {
  auto it = notificationHandlerMap.find(notificationType);
  if (it == notificationHandlerMap.end()) {
    XLOGF(
        WARN,
        "Unrecognized notification: {}",
        fmt::underlying(notificationType));
    return HRESULT_FROM_WIN32(ERROR_INVALID_PARAMETER);
  } else {
    auto duration = it->second.duration;
    auto countSuccessful = it->second.countSuccessful;
    auto countFailure = it->second.countFailure;
    auto handler = it->second.handler;
    auto renderer = it->second.renderer;

    auto relPath = RelativePath(callbackData->FilePathName);
    auto destPath = RelativePath(destinationFileName);

    // The underlying handlers may call into the inode code and since this
    // notification may have been triggered by the inode code itself, we may
    // end up in a deadlock. To prevent this, let's simply bail here when this
    // happens.
    if (callbackData->TriggeringProcessId == GetCurrentProcessId()) {
      XLOGF(ERR, "Recursive EdenFS call are disallowed for: {}", relPath);
      return HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED);
    }

    auto requestWatch =
        std::shared_ptr<RequestMetricsScope::LockedRequestWatchList>(nullptr);
    context->startRequest(getStats().copy(), duration, requestWatch);

    FB_LOG(getStraceLogger(), DBG7, renderer(relPath, destPath, isDirectory));
    auto fut = (this->*handler)(
                   std::move(relPath),
                   std::move(destPath),
                   isDirectory,
                   context->getObjectFetchContext())
                   .semi();
    if (fut.isReady()) {
      // The notification is ready, this is usually coming from pre*
      // notifications to deny the operation, in that case EdenFS should return
      // the error code instead of pushing the operation to the background.
      auto result = tryToHResult(std::move(fut).getTry(0ms));
      if (result == S_OK) {
        if (getStats() && countSuccessful) {
          getStats()->increment(countSuccessful);
        }
      } else {
        if (getStats() && countFailure) {
          getStats()->increment(countFailure);
        }
      }
      return result;
    } else {
      folly::futures::detachOn(
          dispatcher_->getNotificationExecutor(),
          std::move(fut)
              .deferError([this, countFailure](folly::exception_wrapper&& ew) {
                if (getStats() && countFailure) {
                  getStats()->increment(countFailure);
                }
                ew.throw_exception();
              })
              .deferEnsure([context] {}));
      if (getStats() && countSuccessful) {
        getStats()->increment(countSuccessful);
      }
      return S_OK;
    }
  }
}

namespace {
void sendReply(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT context,
    int32_t commandId,
    HRESULT result,
    PRJ_COMPLETE_COMMAND_EXTENDED_PARAMETERS* FOLLY_NULLABLE extra) {
  result = PrjCompleteCommand(context, commandId, result, extra);
  if (FAILED(result)) {
    XLOGF(
        ERR,
        "Couldn't complete command: {}: {}",
        commandId,
        win32ErrorToString(result));
  }
}
} // namespace

void PrjfsChannelInner::sendSuccess(
    int32_t commandId,
    PRJ_COMPLETE_COMMAND_EXTENDED_PARAMETERS* FOLLY_NULLABLE extra) {
  sendReply(mountChannel_, commandId, S_OK, extra);
}

void PrjfsChannelInner::sendError(int32_t commandId, HRESULT result) {
  sendReply(mountChannel_, commandId, result, nullptr);
}

PrjfsChannel::PrjfsChannel(
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
    const std::shared_ptr<folly::Executor>& invalidationThreadPool)
    : mountPath_(mountPath),
      mountId_(std::move(guid)),
      enableSymlinks_(enableWindowsSymlinks),
      processAccessLog_(
          std::make_shared<ProcessAccessLog>(std::move(processInfoCache))),
      config_{std::move(config)} {
  auto [innerDeletedPromise, innerDeletedFuture] =
      folly::makePromiseContract<folly::Unit>();
  innerDeleted_ = std::move(innerDeletedFuture);
  inner_.store(
      std::make_shared<PrjfsChannelInner>(
          std::move(dispatcher),
          straceLogger,
          edenFsEventsLogger,
          faultInjector,
          processAccessLog_,
          config_,
          std::move(innerDeletedPromise),
          std::move(notifier),
          config_->getEdenConfig()->PrjfsTraceBusCapacity.getValue(),
          invalidationThreadPool));
}

PrjfsChannel::~PrjfsChannel() {
  XCHECK(stopPromise_.isFulfilled())
      << "stop() must be called before destroying the channel";
}

void PrjfsChannel::destroy() {
  delete this;
}

folly::Future<FsChannel::StopFuture> PrjfsChannel::initialize() {
  auto callbacks = PRJ_CALLBACKS();
  callbacks.StartDirectoryEnumerationCallback = startEnumeration;
  callbacks.EndDirectoryEnumerationCallback = endEnumeration;
  callbacks.GetDirectoryEnumerationCallback = getEnumerationData;
  callbacks.GetPlaceholderInfoCallback = getPlaceholderInfo;
  callbacks.GetFileDataCallback = getFileData;
  callbacks.NotificationCallback = notification;
  callbacks.QueryFileNameCallback = queryFileName;
  callbacks.CancelCommandCallback = cancelCommand;

  PRJ_NOTIFICATION_MAPPING notificationMappings[] = {
      {PRJ_NOTIFY_NEW_FILE_CREATED | PRJ_NOTIFY_FILE_OVERWRITTEN |
           PRJ_NOTIFY_PRE_DELETE | PRJ_NOTIFY_PRE_RENAME |
           PRJ_NOTIFY_FILE_RENAMED |
           PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED |
           PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED |
           PRJ_NOTIFY_PRE_SET_HARDLINK,
       L""},
  };

  auto config = config_->getEdenConfig();
  if (config->prjfsListenToPreConvertToFull.getValue()) {
    notificationMappings[0].NotificationBitMask |=
        PRJ_NOTIFY_FILE_PRE_CONVERT_TO_FULL;
  }

  auto startOpts = PRJ_STARTVIRTUALIZING_OPTIONS();
  startOpts.NotificationMappings = notificationMappings;
  startOpts.NotificationMappingsCount =
      folly::to_narrow(std::size(notificationMappings));

  useNegativePathCaching_ = config->prjfsUseNegativePathCaching.getValue();
  if (useNegativePathCaching_) {
    startOpts.Flags = PRJ_FLAG_USE_NEGATIVE_PATH_CACHE;
  }

  XLOGF(
      INFO,
      "Starting PrjfsChannel for: {} with GUID: {}",
      mountPath_,
      mountId_);

  auto winPath = mountPath_.wide();

  auto result = PrjMarkDirectoryAsPlaceholder(
      winPath.c_str(), nullptr, nullptr, mountId_);

  if (FAILED(result) &&
      result != HRESULT_FROM_WIN32(ERROR_REPARSE_POINT_ENCOUNTERED)) {
    throw makeHResultErrorExplicit(
        result,
        fmt::format(
            FMT_STRING("Failed to setup the mount point: {}"), mountPath_));
  }

  PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT mountChannel;
  result = PrjStartVirtualizing(
      winPath.c_str(), &callbacks, this, &startOpts, &mountChannel);

  if (FAILED(result)) {
    throw makeHResultErrorExplicit(result, "Failed to start the mount point");
  }

  if (enableSymlinks_) {
    getInner()->initializeSymlinkSupport();
  }

  getInner()->setMountChannel(mountChannel);

  // On Windows, negative path cache is kept between channels. Invalidating here
  // gives our user an easy way to get out of a situation where an incorrect
  // negative path result is cached by Windows without rebooting.
  flushNegativePathCache();

  XLOGF(INFO, "Started PrjfsChannel for: {}", mountPath_);

  stopPromise_ = folly::Promise<FsStopDataPtr>{};
  return folly::makeFuture<FsChannel::StopFuture>(getStopFuture());
}

ImmediateFuture<folly::Unit> PrjfsChannel::waitForPendingWrites() {
  // DEPRECATED: use co_waitForPendingWrites directly. Kept only because
  // FsChannel still declares the ImmediateFuture virtual; delete once
  // EdenMount::waitForPendingWrites is fully migrated to coroutines.
  // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
  return ImmediateFuture{
      folly::coro::co_invoke([this]() -> folly::coro::Task<folly::Unit> {
        co_return co_await co_waitForPendingWrites();
      }).semi()};
}

folly::coro::now_task<folly::Unit> PrjfsChannel::co_waitForPendingWrites() {
  auto inner = getInner();
  if (!inner) {
    throw std::runtime_error(
        fmt::format(
            FMT_STRING("The mount at {} has been stopped"), mountPath_));
  }
  co_await inner->co_waitForPendingNotifications();
  co_return folly::unit;
}

ImmediateFuture<folly::Unit> PrjfsChannel::matchEdenViewOfFileToFS(
    RelativePath relPath,
    const ObjectFetchContextPtr& context) {
  auto inner = getInner();
  if (!inner) {
    return makeImmediateFuture<folly::Unit>(std::runtime_error(
        fmt::format(
            FMT_STRING("The mount at {} has been stopped"), mountPath_)));
  }
  return inner->matchEdenViewOfFileToFS(std::move(relPath), context)
      .ensure([inner = std::move(inner)] {});
}

bool PrjfsChannel::StopData::isUnmounted() {
  return true;
}

FsChannelInfo PrjfsChannel::StopData::extractTakeoverInfo() {
  return ProjFsChannelData{};
}

folly::SemiFuture<folly::Unit> PrjfsChannel::unmount(
    UnmountOptions /* options */) {
  XLOGF(INFO, "Stopping PrjfsChannel for: {}", mountPath_);
  XCHECK(!stopPromise_.isFulfilled());

  inner_.store(nullptr, std::memory_order_release);
  return std::move(innerDeleted_)
      .deferValue([stopPromise = std::move(stopPromise_)](auto&&) mutable {
        stopPromise.setValue(std::make_unique<StopData>());
      });
}

FsChannel::StopFuture PrjfsChannel::getStopFuture() {
  return stopPromise_.getSemiFuture();
}

// TODO: We need to add an extra layer to absorb all the exceptions generated in
// Eden from leaking into FS. This would come in soon.

folly::Try<folly::Unit> PrjfsChannel::removeCachedFile(RelativePathPiece path) {
  auto inner = getInner();
  return removeCachedFileImpl(inner, path);
}

folly::Try<folly::Unit> PrjfsChannel::addDirectoryPlaceholder(
    RelativePathPiece path) {
  auto inner = getInner();
  if (!inner) {
    return folly::Try<folly::Unit>{std::runtime_error{fmt::format(
        FMT_STRING(
            "Couldn't add a placeholder for {}: PrjfsChannel is stopped"),
        path)}};
  }

  DurationScope<EdenStats> statScope{
      inner->getStats(), &PrjfsStats::addDirectoryPlaceholder};

  if (path.empty()) {
    return folly::Try<folly::Unit>{folly::unit};
  }

  auto winMountPath = mountPath_.wide();
  auto fullPath = mountPath_ + path;
  auto winPath = fullPath.wide();

  XLOGF(DBG6, "Adding a placeholder for: {}", path);
  auto result = PrjMarkDirectoryAsPlaceholder(
      winMountPath.c_str(), winPath.c_str(), nullptr, mountId_);
  if (FAILED(result)) {
    if (result == HRESULT_FROM_WIN32(ERROR_REPARSE_POINT_ENCOUNTERED)) {
      // This is already a placeholder, not an error.
    } else if (
        result == HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND) ||
        result == HRESULT_FROM_WIN32(ERROR_PATH_NOT_FOUND)) {
      // If EdenFS happens to be invalidating a directory that is no longer
      // present in the destination commit, PrjMarkDirectoryAsPlaceholder would
      // trigger a recursive lookup call and fail, raising this error. This is
      // harmless and thus we can just ignore.
    } else {
      inner->getStats()->increment(&PrjfsStats::addDirectoryPlaceholderFailure);
      return folly::Try<folly::Unit>{makeHResultErrorExplicit(
          result,
          fmt::format(
              FMT_STRING("Couldn't add a placeholder for {}: {:#x}"),
              path,
              static_cast<uint32_t>(result)))};
    }
  }

  inner->getStats()->increment(&PrjfsStats::addDirectoryPlaceholderSuccessful);
  return folly::Try<folly::Unit>{folly::unit};
}

ImmediateFuture<folly::Unit> PrjfsChannel::completeInvalidations() {
  // completeInvalidations() is called before filesystem-modifying Thrift calls
  // return. If new files have been added, we need to clear the negative path
  // cache.
  flushNegativePathCache();
  return folly::unit;
}

folly::coro::now_task<folly::Unit> PrjfsChannel::co_completeInvalidations() {
  flushNegativePathCache();
  co_return folly::unit;
}

void PrjfsChannel::flushNegativePathCache() {
  auto inner = getInner();
  if (!inner) {
    return;
  }

  if (useNegativePathCaching_) {
    XLOG(DBG6, "Flushing negative path cache");

    uint32_t numFlushed = 0;
    auto result =
        PrjClearNegativePathCache(inner->getMountChannel(), &numFlushed);
    if (FAILED(result)) {
      throwHResultErrorExplicit(
          result, "Couldn't flush the negative path cache");
    }

    XLOGF(DBG6, "Flushed {} entries", numFlushed);
  }
}

void PrjfsChannelInner::initializeSymlinkSupport() {
  if (placeholderExtendedInfo2_ == nullptr) {
    placeholderExtendedInfo2_ = (PPWPI2)GetProcAddress(
        GetModuleHandle(TEXT("ProjectedFSLib.dll")),
        "PrjWritePlaceholderInfo2");
  }
  if (prjFillDirEntryBuffer2_ == nullptr) {
    prjFillDirEntryBuffer2_ = (PPFDEB2)GetProcAddress(
        GetModuleHandle(TEXT("ProjectedFSLib.dll")), "PrjFillDirEntryBuffer2");
  }
  if (placeholderExtendedInfo2_ == nullptr ||
      prjFillDirEntryBuffer2_ == nullptr) {
    throw makeHResultErrorExplicit(
        255,
        "Failed to start the mount point: support for symlink requested but PrjFS does not support symlinks in the current system");
  }
  symlinksSupported_ = true;
}

} // namespace facebook::eden

#endif
