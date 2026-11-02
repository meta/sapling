/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/service/EdenServiceHandler.h"

#include <sys/types.h>
#include <algorithm>
#include <chrono>
#include <cstdlib>
#include <limits>
#include <optional>
#include <stdexcept>
#include <system_error>
#include <typeinfo>
#include <unordered_set>
#include <utility>

#include <fb303/ServiceData.h>
#include <fb303/ThreadCachedServiceData.h>
#include <fmt/format.h>
#include <fmt/ranges.h>
#include <folly/Conv.h>
#include <folly/Exception.h>
#include <folly/File.h>
#include <folly/FileUtil.h>
#include <folly/Portability.h>
#include <folly/String.h>
#include <folly/Utility.h>
#include <folly/chrono/Conv.h>
#include <folly/coro/Collect.h>
#include <folly/coro/CurrentExecutor.h>
#include <folly/coro/Invoke.h>
#include <folly/coro/Task.h>
#include <folly/coro/safe/NowTask.h>
#include <folly/executors/SerialExecutor.h>
#include <folly/futures/Future.h>
#include <folly/logging/Logger.h>
#include <folly/logging/xlog.h>
#include <folly/portability/Fcntl.h>
#include <folly/stop_watch.h>
#include <thrift/lib/cpp/util/EnumUtils.h>
#include <thrift/lib/cpp2/server/ThriftServer.h>

#include "ThriftGetObjectImpl.h"
#include "eden/common/telemetry/StructuredLogger.h"
#include "eden/common/telemetry/Tracing.h"
#include "eden/common/utils/Bug.h"
#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/ProcessInfoCache.h"
#include "eden/common/utils/StatTimes.h"
#include "eden/common/utils/String.h"
#include "eden/common/utils/TimeUtil.h"
#include "eden/common/utils/UnboundedQueueExecutor.h"
#include "eden/fs/config/CheckoutConfig.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/fuse/FuseChannel.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/InodeError.h"
#include "eden/fs/inodes/InodeMap.h"
#include "eden/fs/inodes/InodeTable.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/inodes/Traverse.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/inodes/VirtualInodeLoader.h"
#include "eden/fs/journal/Journal.h"
#include "eden/fs/journal/JournalDelta.h"
#include "eden/fs/model/Blob.h"
#include "eden/fs/model/BlobAuxData.h"
#include "eden/fs/model/Hash.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/model/TreeEntry.h"
#include "eden/fs/model/git/TopLevelIgnores.h"
#include "eden/fs/nfs/NfsServer.h"
#include "eden/fs/nfs/Nfsd3.h"
#ifdef _WIN32
#include "eden/fs/notifications/Notifier.h"
#include "eden/fs/prjfs/PrjfsChannel.h" // @manual
#endif
#include "eden/fs/privhelper/PrivHelper.h"
#include "eden/fs/rust/redirect_ffi/include/ffi.h"
#include "eden/fs/rust/redirect_ffi/src/lib.rs.h"
#include "eden/fs/service/EdenServer.h"
#include "eden/fs/service/ThriftGlobImpl.h"
#include "eden/fs/service/ThriftPermissionChecker.h"
#include "eden/fs/service/ThriftUtil.h"
#include "eden/fs/service/gen-cpp2/eden_constants.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/service/gen-cpp2/streamingeden_constants.h"
#include "eden/fs/store/BackingStore.h"
#include "eden/fs/store/Diff.h"
#include "eden/fs/store/DiffContext.h"
#include "eden/fs/store/FilteredBackingStore.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/store/ScmStatusDiffCallback.h"
#include "eden/fs/store/StatsFetchContext.h"
#include "eden/fs/store/TreeCache.h"
#include "eden/fs/store/filter/GlobFilter.h"
#include "eden/fs/store/sl/SaplingBackingStore.h"
#include "eden/fs/telemetry/EdenErrorInfoBuilder.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/telemetry/LogEvent.h"
#include "eden/fs/telemetry/TaskTrace.h"
#include "eden/fs/utils/EdenError.h"
#include "eden/fs/utils/GlobMatcher.h"
#include "eden/fs/utils/MountInfoTable.h"
#include "eden/fs/utils/NotImplemented.h"
#include "eden/fs/utils/ProcUtil.h"
#include "eden/fs/utils/SourceLocation.h"

using folly::Future;
using folly::makeFuture;
using folly::StringPiece;
using folly::Try;
using folly::Unit;
using folly::coro::co_awaitTry;
using std::string;
using std::unique_ptr;
using std::vector;
using namespace std::literals::string_view_literals;

namespace {
using namespace facebook::eden;

constexpr auto kAclPathAttributes =
    ENTRY_ATTRIBUTE_UNDER_ACL | ENTRY_ATTRIBUTE_ACLs;

std::string getClientCmdline(
    const std::shared_ptr<ServerState>& serverState_,
    const ObjectFetchContextPtr& context_) {
  std::string client_cmdline = "<unknown>";
  if (auto clientPid = context_->getClientPid()) {
    // TODO: we should look up client scope here instead of command line
    // since it will give move context into the overarching process or
    // system producing the expensive query
    const ProcessInfo* processInfoPtr = serverState_->getProcessInfoCache()
                                            ->lookup(clientPid.value().get())
                                            .get_optional();
    if (processInfoPtr) {
      client_cmdline = processInfoPtr->name;
      std::replace(client_cmdline.begin(), client_cmdline.end(), '\0', ' ');
    }
  }
  return client_cmdline;
}

bool isAclOnlyRequest(EntryAttributeFlags requestedAttributes) {
  return requestedAttributes.containsAnyOf(kAclPathAttributes) &&
      (requestedAttributes & kAclPathAttributes) == requestedAttributes;
}

bool isPermissionDenied(const folly::exception_wrapper& exception) {
  bool permissionDenied = false;
  exception.with_exception([&](const std::system_error& error) {
    permissionDenied = error.code() == std::errc::permission_denied;
  });
  return permissionDenied;
}

void setPathAclLookupError(
    EntryAttributes& attributes,
    EntryAttributeFlags requestedAttributes,
    const folly::exception_wrapper& exception) {
  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_UNDER_ACL) &&
      (!attributes.underAcl.has_value() ||
       attributes.underAcl->hasException())) {
    attributes.underAcl = folly::Try<bool>{exception};
  }
  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_ACLs)) {
    attributes.aclInfo = folly::Try<EntryAclInfo>{exception};
  }
}

folly::coro::Task<std::vector<folly::Try<EntryAttributes>>>
co_fetchPathAclLookupsForAttributes(
    std::shared_ptr<ObjectStore> objectStore,
    RootId checkedOutRootId,
    ObjectFetchContextPtr fetchContext,
    std::vector<folly::Try<EntryAttributes>> allRes,
    std::vector<std::string> paths,
    EntryAttributeFlags reqBitmask) {
  const auto aclOnlyRequest = isAclOnlyRequest(reqBitmask);
  std::vector<size_t> aclPathIndices;
  aclPathIndices.reserve(paths.size());

  for (size_t i = 0; i < allRes.size(); ++i) {
    auto& tryAttributes = allRes[i];
    if (tryAttributes.hasException()) {
      if (aclOnlyRequest && isPermissionDenied(tryAttributes.exception())) {
        XLOGF(
            WARN,
            "ACL-only getAttributesFromFilesV2 request for {} hit permission denied while reading local metadata; falling back to path ACL lookup",
            paths.at(i));
        allRes[i] = folly::Try<EntryAttributes>{EntryAttributes{}};
        aclPathIndices.push_back(i);
      }
      continue;
    }

    auto& attributes = tryAttributes.value();
    if (attributes.underAcl.has_value()) {
      if (attributes.underAcl->hasException()) {
        if (reqBitmask.contains(ENTRY_ATTRIBUTE_ACLs)) {
          attributes.aclInfo =
              folly::Try<EntryAclInfo>{attributes.underAcl->exception()};
        }
        continue;
      }
      if (!attributes.underAcl->value()) {
        if (reqBitmask.contains(ENTRY_ATTRIBUTE_ACLs)) {
          attributes.aclInfo =
              folly::Try<EntryAclInfo>{EntryAclInfo{/*underAcl=*/false, {}}};
        }
        continue;
      }

      if (!reqBitmask.contains(ENTRY_ATTRIBUTE_ACLs)) {
        continue;
      }

      aclPathIndices.push_back(i);
      continue;
    }

    if (reqBitmask.containsAnyOf(kAclPathAttributes)) {
      aclPathIndices.push_back(i);
    }
  }

  if (aclPathIndices.empty()) {
    co_return allRes;
  }

  std::vector<std::string> lookupPaths;
  lookupPaths.reserve(aclPathIndices.size());
  for (auto index : aclPathIndices) {
    lookupPaths.push_back(paths.at(index));
  }

  auto aclInfosTry = co_await co_awaitTry(
      objectStore->co_getPathAcls(checkedOutRootId, lookupPaths, fetchContext));
  if (aclInfosTry.hasException()) {
    XLOGF(
        WARN,
        "getAttributesFromFilesV2 path ACL lookup failed for {} path(s): {}",
        lookupPaths.size(),
        folly::exceptionStr(aclInfosTry.exception()));
    for (auto index : aclPathIndices) {
      if (allRes.at(index).hasValue()) {
        setPathAclLookupError(
            allRes.at(index).value(), reqBitmask, aclInfosTry.exception());
      }
    }
    co_return allRes;
  }

  auto aclInfos = std::move(aclInfosTry).value();
  for (size_t offset = 0; offset < aclInfos.size(); ++offset) {
    auto index = aclPathIndices.at(offset);
    if (!allRes.at(index).hasValue()) {
      continue;
    }

    auto& attributes = allRes.at(index).value();
    if (aclInfos.at(offset).hasException()) {
      XLOGF(
          WARN,
          "getAttributesFromFilesV2 path ACL lookup failed for {}: {}",
          paths.at(index),
          folly::exceptionStr(aclInfos.at(offset).exception()));
      setPathAclLookupError(
          attributes, reqBitmask, aclInfos.at(offset).exception());
      continue;
    }

    auto acls = std::move(aclInfos.at(offset).value());
    auto underAcl = !acls.empty();
    if (reqBitmask.contains(ENTRY_ATTRIBUTE_UNDER_ACL)) {
      attributes.underAcl = folly::Try<bool>{underAcl};
    }
    if (reqBitmask.contains(ENTRY_ATTRIBUTE_ACLs)) {
      attributes.aclInfo =
          folly::Try<EntryAclInfo>{EntryAclInfo{underAcl, std::move(acls)}};
    }
  }
  co_return allRes;
}

std::string logHash(StringPiece thriftArg) {
  if (thriftArg.size() == Hash20::RAW_SIZE) {
    return Hash20{folly::ByteRange{thriftArg}}.toString();
  } else if (thriftArg.size() == Hash20::RAW_SIZE * 2) {
    return Hash20{thriftArg}.toString();
  } else {
    return folly::hexlify(thriftArg);
  }
}

std::optional<std::unordered_set<std::string>>
getMountPointsVisibleInDaemonNamespace() {
#ifdef __linux__
  auto result = facebook::eden::getAllMounts();
  if (result.hasError()) {
    XLOGF(
        WARN,
        "Failed to read mount namespace visibility: {}",
        folly::errnoStr(result.error()));
    return std::nullopt;
  }
  std::unordered_set<std::string> mountPoints;
  for (auto& mount : result.value()) {
    mountPoints.insert(std::move(mount.mountPoint));
  }
  return mountPoints;
#else
  return std::nullopt;
#endif
}

std::string logPosition(JournalPosition position) {
  return fmt::format(
      "{}:{}:{}",
      position.mountGeneration().value(),
      position.sequenceNumber().value(),
      logHash(position.snapshotHash().value()));
}

/**
 * Convert a vector of strings from a thrift argument to a field
 * that we can log in an INSTRUMENT_THRIFT_CALL() log message.
 *
 * This truncates very log lists to only log the first few elements.
 */
std::string toLogArg(const std::vector<std::string>& args) {
  constexpr size_t limit = 5;
  if (args.size() <= limit) {
    return fmt::format("[{}]", fmt::join(args, ", "));
  } else {
    return fmt::format(
        "[{}, and {} more]",
        fmt::join(args.begin(), args.begin() + limit, ", "),
        args.size() - limit);
  }
}

bool mountIsUsingFilteredFS(const EdenMountHandle& mount) {
  return mount.getEdenMountPtr()
             ->getCheckoutConfig()
             ->getRepoBackingStoreType() == BackingStoreType::FILTEREDHG;
}

std::string resolveRootId(
    std::string rootId,
    const RootIdOptions& rootIdOptions,
    const EdenMountHandle& mount) {
  if (mountIsUsingFilteredFS(mount)) {
    if (rootIdOptions.fid()) {
      return FilteredBackingStore::createFilteredRootId(
          rootId, *rootIdOptions.fid());
    } else {
      return FilteredBackingStore::createNullFilteredRootId(rootId);
    }
  } else {
    return rootId;
  }
}

// parseRootId() assumes that the provided id will contain information
// about the active filter. Some legacy code paths do not respect
// filters (or accept Filters as arguments), so we need to construct a
// FilteredRootId using the last active filter. For non-FilteredFS repos, the
// last filterID will be std::nullopt.
std::string resolveRootIdWithLastFilter(
    std::string rootId,
    const EdenMountHandle& handle) {
  auto filterId =
      handle.getEdenMount().getCheckoutConfig()->getLastActiveFilter();
  RootIdOptions rootIdOptions{};
  rootIdOptions.fid().from_optional(std::move(filterId));
  return resolveRootId(std::move(rootId), rootIdOptions, handle);
}

// Similar to the above function, but can be used with endpoints that pass in
// many RootIds.
std::vector<std::string> resolveRootsWithLastFilter(
    std::vector<std::string>& originalRootIds,
    const EdenMountHandle& mountHandle) {
  std::vector<std::string> resolvedRootIds;
  resolvedRootIds.reserve(originalRootIds.size());
  for (auto& rev : originalRootIds) {
    resolvedRootIds.push_back(
        resolveRootIdWithLastFilter(std::move(rev), mountHandle));
  }
  return resolvedRootIds;
}

#define EDEN_MICRO reinterpret_cast<const char*>(u8"\u00B5s")

class ThriftFetchContext : public ObjectFetchContext {
 public:
  explicit ThriftFetchContext(
      OptionalProcessId pid,
      CauseDetail endpoint,
      Cause cause)
      : pid_(pid), endpoint_(std::move(endpoint)), cause_(cause) {}

  OptionalProcessId getClientPid() const override {
    return pid_;
  }

  Cause getCause() const override {
    return cause_;
  }

  std::optional<std::string_view> getCauseDetail() const override {
    return endpoint_.asStringView();
  }

  const std::unordered_map<std::string, std::string>* FOLLY_NULLABLE
  getRequestInfo() const override {
    return &requestInfo_;
  }

  /**
   * Update the request info map.
   *
   * This is not thread safe and the caller should make sure that this function
   * isn't called in an unsafe manner.
   */
  void updateRequestInfo(const std::map<std::string, std::string>& another) {
    requestInfo_.insert(another.begin(), another.end());
  }

  void fillClientRequestInfo(
      apache::thrift::optional_field_ref<ClientRequestInfo&>
          clientRequestInfo) {
    if (clientRequestInfo.has_value()) {
      auto correlator = clientRequestInfo->correlator();
      auto entry_point = clientRequestInfo->entry_point();
      if (!(correlator->empty() || entry_point->empty())) {
        updateRequestInfo(
            {{ObjectFetchContext::kClientCorrelator, *correlator},
             {ObjectFetchContext::kClientEntryPoint, *entry_point}});
      }
    }
  }

 private:
  OptionalProcessId pid_;
  CauseDetail endpoint_;
  Cause cause_;
  std::unordered_map<std::string, std::string> requestInfo_;
};

class PrefetchFetchContext : public StatsFetchContext {
 public:
  explicit PrefetchFetchContext(
      OptionalProcessId pid,
      CauseDetail endpoint,
      bool enablePrefetchStats)
      : StatsFetchContext(
            pid,
            ObjectFetchContext::Cause::Prefetch,
            std::move(endpoint),
            nullptr),
        enablePrefetchStats_(enablePrefetchStats) {}

  ImportPriority getPriority() const override {
    return kThriftPrefetchPriority;
  }

  bool shouldCollectStats() const override {
    return enablePrefetchStats_;
  }

  void addPrefetchedBlobSize(uint64_t bytes) override {
    if (enablePrefetchStats_) {
      StatsFetchContext::addPrefetchedBlobSize(bytes);
    }
  }

 private:
  bool enablePrefetchStats_;
};

/**
 * Lives as long as a Thrift request and primarily exists to record logging and
 * telemetry.
 */
class ThriftRequestScope {
 public:
  ThriftRequestScope(ThriftRequestScope&&) = delete;
  ThriftRequestScope& operator=(ThriftRequestScope&&) = delete;

  template <typename JoinFn>
  ThriftRequestScope(
      std::shared_ptr<TraceBus<ThriftRequestTraceEvent>> traceBus,
      const folly::Logger& logger,
      folly::LogLevel level,
      SourceLocation sourceLocation,
      EdenStatsPtr edenStats,
      ThriftStats::DurationPtr statPtr,
      OptionalProcessId pid,
      JoinFn&& join,
      std::shared_ptr<EdenServiceHandler> serviceHandler,
      bool enableCancellation = false,
      bool enablePrefetchStats = false,
      ObjectFetchContext::Cause fetchCause = ObjectFetchContext::Cause::Thrift)
      : traceBus_{std::move(traceBus)},
        requestId_(generateUniqueID()),
        sourceLocation_{sourceLocation},
        edenStats_{std::move(edenStats)},
        statPtr_{std::move(statPtr)},
        level_(level),
        itcLogger_(logger),
        thriftFetchContext_{makeRefPtr<ThriftFetchContext>(
            pid,
            ObjectFetchContext::StaticCauseDetail::fromSourceLocation(
                sourceLocation_),
            fetchCause)},
        prefetchFetchContext_{makeRefPtr<PrefetchFetchContext>(
            pid,
            ObjectFetchContext::StaticCauseDetail::fromSourceLocation(
                sourceLocation_),
            enablePrefetchStats)},
        handler_(serviceHandler) {
    if (auto handler = handler_.lock()) {
      if (enableCancellation) {
        folly::CancellationSource cancellationSource;
        auto token = cancellationSource.getToken();
        thriftFetchContext_->setCancellationToken(token);
        prefetchFetchContext_->setCancellationToken(std::move(token));
        handler->insertCancellationSource(
            requestId_,
            std::move(cancellationSource),
            sourceLocation_.function_name(),
            pid);
      } else {
        handler->insertUncancelableRequest(
            requestId_, sourceLocation_.function_name(), pid);
      }
    }

    FB_LOG_RAW(
        itcLogger_,
        level,
        sourceLocation.file_name(),
        sourceLocation.line(),
        "")
        << "[" << requestId_ << "] " << sourceLocation.function_name() << "("
        << join() << ")";

    traceBus_->publish(
        ThriftRequestTraceEvent::start(
            requestId_, sourceLocation_.function_name(), pid));
  }

  ~ThriftRequestScope() {
    if (auto handler = handler_.lock()) {
      handler->removeCancellationSource(requestId_);
    }

    // Logging completion time for the request
    // The line number points to where the object was originally created
    auto elapsed = itcTimer_.elapsed();
    auto level = level_;
    if (elapsed > std::chrono::seconds(1)) {
      // When a request takes over a second, let's raise the loglevel to draw
      // attention to it
      level += 1;
    }
    FB_LOG_RAW(
        itcLogger_,
        level,
        sourceLocation_.file_name(),
        sourceLocation_.line(),
        "")
        << "[" << requestId_ << "] "
        << fmt::format(
               "{}() took {} {}",
               sourceLocation_.function_name(),
               elapsed.count(),
               EDEN_MICRO);
    if (edenStats_) {
      edenStats_->addDuration(statPtr_, elapsed);
    }
    traceBus_->publish(
        ThriftRequestTraceEvent::finish(
            requestId_,
            sourceLocation_.function_name(),
            thriftFetchContext_->getClientPid()));
  }

  const ObjectFetchContextPtr& getPrefetchFetchContext() {
    return prefetchFetchContext_.as<ObjectFetchContext>();
  }

  ThriftFetchContext& getThriftFetchContext() {
    return *thriftFetchContext_;
  }

  const ObjectFetchContextPtr& getFetchContext() {
    return thriftFetchContext_.as<ObjectFetchContext>();
  }

  folly::StringPiece getFunctionName() {
    return sourceLocation_.function_name();
  }

  uint64_t getRequestId() const {
    return requestId_;
  }

  /**
   * Record that this request's work continues after its response was sent,
   * so that a graceful restart does not wait for it and a stop deadline does
   * not blame it.
   */
  void markBackground() {
    if (auto handler = handler_.lock()) {
      handler->markRequestAsBackground(requestId_);
    }
  }

 private:
  std::shared_ptr<TraceBus<ThriftRequestTraceEvent>> traceBus_;
  uint64_t requestId_;
  SourceLocation sourceLocation_;
  EdenStatsPtr edenStats_;
  ThriftStats::DurationPtr statPtr_;
  folly::LogLevel level_;
  folly::Logger itcLogger_;
  folly::stop_watch<std::chrono::microseconds> itcTimer_ = {};
  RefPtr<ThriftFetchContext> thriftFetchContext_;
  RefPtr<PrefetchFetchContext> prefetchFetchContext_;

  // allow EdenServiceHandler destruction for in-flight thrift requests
  std::weak_ptr<EdenServiceHandler> handler_;
};

template <typename ReturnType>
Future<ReturnType> wrapFuture(
    std::unique_ptr<ThriftRequestScope> logHelper,
    folly::Future<ReturnType>&& f) {
  return std::move(f).ensure([logHelper = std::move(logHelper)]() {});
}

template <typename ReturnType>
ImmediateFuture<ReturnType> wrapImmediateFuture(
    std::unique_ptr<ThriftRequestScope> logHelper,
    ImmediateFuture<ReturnType>&& f) {
  return std::move(f).ensure([logHelper = std::move(logHelper)]() {});
}

/**
 * Lives as long as a glob files request and primarily exists to record logging
 * and telemetry.
 */
class GlobFilesRequestScope {
 public:
  GlobFilesRequestScope(GlobFilesRequestScope&&) = delete;
  GlobFilesRequestScope& operator=(GlobFilesRequestScope&&) = delete;

  GlobFilesRequestScope(
      std::shared_ptr<ServerState> serverState,
      std::string logString,
      const ObjectFetchContextPtr& context)
      : serverState_{std::move(serverState)},
        logString_{std::move(logString)},
        context_{context.copy()} {}

  ~GlobFilesRequestScope() {
    auto elapsed = itcTimer_.elapsed();
    auto duration = std::chrono::duration<double>{elapsed}.count();
    XLOGF(DBG4, "EdenFS completed globFiles request in {}s", duration);
    if (duration >= EXPENSIVE_GLOB_FILES_DURATION) {
      std::string client_cmdline = getClientCmdline(serverState_, context_);
      serverState_->getEdenFsEventsLogger()->logEvent(
          ExpensiveGlob{
              duration,
              logString_,
              std::move(client_cmdline),
              /*is_local=*/true});
    }
  }

 private:
  std::shared_ptr<ServerState> serverState_;
  std::string logString_;
  ObjectFetchContextPtr context_;
  folly::stop_watch<std::chrono::microseconds> itcTimer_ = {};
};
#undef EDEN_MICRO

RelativePath relpathFromUserPath(StringPiece userPath) {
  if (userPath.empty() || userPath == ".") {
    return RelativePath{};
  } else {
    return RelativePath{userPath};
  }
}

RelativePathPiece relpathPieceFromUserPath(StringPiece userPath) {
  if (userPath.empty() || userPath == ".") {
    return RelativePathPiece{};
  } else {
    return RelativePathPiece{userPath};
  }
}

facebook::eden::InodePtr inodeFromUserPath(
    facebook::eden::EdenMount& mount,
    StringPiece rootRelativePath,
    const ObjectFetchContextPtr& context) {
  auto relPath = relpathFromUserPath(rootRelativePath);
  return mount.getInodeSlow(relPath, context).get();
}

} // namespace

// INSTRUMENT_THRIFT_CALL returns a unique pointer to
// ThriftRequestScope object. The returned pointer can be used to call
// wrapFuture() to attach a log message on the completion of the Future. This
// must be called in a Thrift worker thread because the calling pid of
// getAndRegisterClientPid is stored in a thread local variable.
//
// For coroutine methods, thread-local storage is unreliable due to
// potential thread switching. Use INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION
// directly and pass the request context explicitly.

// When not attached to Future it will log the completion of the operation and
// time taken to complete it.

// Core macro: all INSTRUMENT_THRIFT_CALL variants delegate here.
#define INSTRUMENT_THRIFT_CALL_IMPL(                          \
    level,                                                    \
    edenStats,                                                \
    statPtr,                                                  \
    requestContext,                                           \
    enableCancellation,                                       \
    enablePrefetchStats,                                      \
    fetchCause,                                               \
    ...)                                                      \
  ([&](SourceLocation loc) {                                  \
    static folly::Logger logger(                              \
        fmt::format("eden.thrift.{}", loc.function_name()));  \
    return std::make_unique<ThriftRequestScope>(              \
        this->thriftRequestTraceBus_,                         \
        logger,                                               \
        folly::LogLevel::level,                               \
        loc,                                                  \
        edenStats,                                            \
        statPtr,                                              \
        getAndRegisterClientPid(requestContext),              \
        [&] {                                                 \
          return fmt::to_string(                              \
              fmt::join(std::make_tuple(__VA_ARGS__), ", ")); \
        },                                                    \
        this->shared_from_this(),                             \
        enableCancellation,                                   \
        enablePrefetchStats,                                  \
        fetchCause);                                          \
  }(EDEN_CURRENT_SOURCE_LOCATION))

#define INSTRUMENT_THRIFT_CALL(level, ...) \
  INSTRUMENT_THRIFT_CALL_IMPL(             \
      level,                               \
      nullptr,                             \
      nullptr,                             \
      nullptr,                             \
      false,                               \
      false,                               \
      ObjectFetchContext::Cause::Thrift,   \
      __VA_ARGS__)

#define INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(   \
    level, enableCancellation, requestContext, ...) \
  INSTRUMENT_THRIFT_CALL_IMPL(                      \
      level,                                        \
      nullptr,                                      \
      nullptr,                                      \
      requestContext,                               \
      enableCancellation,                           \
      false,                                        \
      ObjectFetchContext::Cause::Thrift,            \
      __VA_ARGS__)

#define INSTRUMENT_THRIFT_GLOB_CALL_WITH_CANCELLATION( \
    level, enableCancellation, requestContext, ...)    \
  INSTRUMENT_THRIFT_CALL_IMPL(                         \
      level,                                           \
      nullptr,                                         \
      nullptr,                                         \
      requestContext,                                  \
      enableCancellation,                              \
      false,                                           \
      ObjectFetchContext::Cause::Glob,                 \
      __VA_ARGS__)

#define INSTRUMENT_THRIFT_CALL_WITH_STAT(level, stat, ...) \
  INSTRUMENT_THRIFT_CALL_IMPL(                             \
      level,                                               \
      server_->getStats().copy(),                          \
      stat,                                                \
      nullptr,                                             \
      false,                                               \
      false,                                               \
      ObjectFetchContext::Cause::Thrift,                   \
      __VA_ARGS__)

#define INSTRUMENT_THRIFT_CALL_WITH_STAT_AND_CANCELLATION( \
    level, stat, enableCancellation, requestContext, ...)  \
  INSTRUMENT_THRIFT_CALL_IMPL(                             \
      level,                                               \
      server_->getStats().copy(),                          \
      stat,                                                \
      requestContext,                                      \
      enableCancellation,                                  \
      false,                                               \
      ObjectFetchContext::Cause::Thrift,                   \
      __VA_ARGS__)

#define INSTRUMENT_THRIFT_CALL_WITH_PREFETCH_STATS(level, enableStats, ...) \
  INSTRUMENT_THRIFT_CALL_IMPL(                                              \
      level,                                                                \
      nullptr,                                                              \
      nullptr,                                                              \
      nullptr,                                                              \
      false,                                                                \
      enableStats,                                                          \
      ObjectFetchContext::Cause::Thrift,                                    \
      __VA_ARGS__)

#define INSTRUMENT_THRIFT_CALL_WITH_PREFETCH_STATS_AND_CANCELLATION( \
    level, enableStats, requestContext, ...)                         \
  INSTRUMENT_THRIFT_CALL_IMPL(                                       \
      level,                                                         \
      nullptr,                                                       \
      nullptr,                                                       \
      requestContext,                                                \
      true,                                                          \
      enableStats,                                                   \
      ObjectFetchContext::Cause::Thrift,                             \
      __VA_ARGS__)

ThriftRequestTraceEvent ThriftRequestTraceEvent::start(
    uint64_t requestId,
    folly::StringPiece method,
    OptionalProcessId clientPid) {
  return ThriftRequestTraceEvent{
      ThriftRequestTraceEvent::START, requestId, method, clientPid};
}

ThriftRequestTraceEvent ThriftRequestTraceEvent::finish(
    uint64_t requestId,
    folly::StringPiece method,
    OptionalProcessId clientPid) {
  return ThriftRequestTraceEvent{
      ThriftRequestTraceEvent::FINISH, requestId, method, clientPid};
}

template <>
struct fmt::formatter<facebook::eden::MountId> : public formatter<std::string> {
  template <typename Context>
  auto format(const facebook::eden::MountId& mountId, Context& ctx) const {
    return formatter<std::string>::format(*mountId.mountPoint(), ctx);
  }
};

namespace facebook::eden {

const char* const kServiceName = "EdenFS";

std::optional<ActivityBuffer<ThriftRequestTraceEvent>>
EdenServiceHandler::initThriftRequestActivityBuffer() {
  if (server_->getServerState()
          ->getEdenConfig()
          ->enableActivityBuffer.getValue()) {
    return std::make_optional<ActivityBuffer<ThriftRequestTraceEvent>>(
        server_->getServerState()
            ->getEdenConfig()
            ->activityBufferMaxEvents.getValue());
  }
  return std::nullopt;
}

EdenServiceHandler::EdenServiceHandler(
    std::vector<std::string> originalCommandLine,
    EdenServer* server)
    : BaseService{kServiceName},
      originalCommandLine_{std::move(originalCommandLine)},
      server_{server},
      streamJournalChangedShuttingDown_{
          std::make_shared<std::atomic<bool>>(false)},
      thriftRequestActivityBuffer_(initThriftRequestActivityBuffer()),
      thriftRequestTraceBus_(
          TraceBus<ThriftRequestTraceEvent>::create(
              "ThriftRequestTrace",
              server_->getServerState()
                  ->getEdenConfig()
                  ->ThriftTraceBusCapacity.getValue())) {
  thriftRequestTraceHandle_ = thriftRequestTraceBus_->subscribeFunction(
      "Outstanding Thrift request tracing",
      [this](const ThriftRequestTraceEvent& event) {
        switch (event.type) {
          case ThriftRequestTraceEvent::START:
            outstandingThriftRequests_.wlock()->emplace(event.requestId, event);
            break;
          case ThriftRequestTraceEvent::FINISH:
            outstandingThriftRequests_.wlock()->erase(event.requestId);
            break;
        }
        if (thriftRequestActivityBuffer_.has_value()) {
          thriftRequestActivityBuffer_->addEvent(event);
        }
      });
}

EdenServiceHandler::~EdenServiceHandler() {
  disarmDelayedCancellation();
}

std::shared_ptr<folly::Executor> EdenServiceHandler::getTraceStreamExecutor() {
  return server_->getServerState()->getThreadPool();
}

EdenMountHandle EdenServiceHandler::lookupMount(const MountId& mountId) {
  return lookupMount(mountId.mountPoint());
}

EdenMountHandle EdenServiceHandler::lookupMount(
    const std::unique_ptr<std::string>& mountId) {
  return lookupMount(*mountId);
}

EdenMountHandle EdenServiceHandler::lookupMount(
    apache::thrift::field_ref<std::string&> mountId) {
  return lookupMount(*mountId);
}

EdenMountHandle EdenServiceHandler::lookupMount(
    apache::thrift::field_ref<const std::string&> mountId) {
  return lookupMount(*mountId);
}

EdenMountHandle EdenServiceHandler::lookupMount(const std::string& mountId) {
  auto mountPath = absolutePathFromThrift(mountId);
  return server_->getMount(mountPath);
}

std::unique_ptr<apache::thrift::AsyncProcessor>
EdenServiceHandler::getProcessor() {
  auto processor = StreamingEdenServiceSvIf::getProcessor();
  if (server_->getServerState()
          ->getEdenConfig()
          ->thriftUseCustomPermissionChecking.getValue()) {
    processor->addEventHandler(
        std::make_shared<ThriftPermissionChecker>(server_->getServerState()));
  }
  return processor;
}

folly::SemiFuture<folly::Unit> EdenServiceHandler::semifuture_mount(
    std::unique_ptr<MountArgument> argument) {
  auto helper = INSTRUMENT_THRIFT_CALL(INFO, (*argument->mountPoint()));
  server_->noteActivity("mount");
  return wrapImmediateFuture(
             std::move(helper),
             makeImmediateFutureWith([&] {
               auto mountPoint =
                   absolutePathFromThrift(*argument->mountPoint());
               auto edenClientPath =
                   absolutePathFromThrift(*argument->edenClientPath());
               auto initialConfig = CheckoutConfig::loadFromClientDirectory(
                   mountPoint, edenClientPath);

               return server_
                   ->mount(std::move(initialConfig), *argument->readOnly())
                   .unit();
             }).thenError([this](const folly::exception_wrapper& ex) {
               auto outcome = ErrorLogOutcome::Disabled;
               ex.with_exception([&](const std::exception& e) {
                 outcome = server_->getServerState()->getErrorLogger().log(
                     EdenErrorInfo::thrift(
                         ErrorArg::fromExceptionWithoutTrace(e), "mount"));
               });
               if (outcome != ErrorLogOutcome::RateLimited) {
                 XLOGF(ERR, "Error: {}", ex.what());
               }
               throw newEdenError(ex);
             }))
      .semi();
}

folly::SemiFuture<folly::Unit> EdenServiceHandler::semifuture_unmount(
    std::unique_ptr<std::string> mountPoint) {
  auto requestContext = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      INFO, true, requestContext, *mountPoint);
  server_->noteActivity("unmount");
  auto requestId = helper->getRequestId();
  return wrapImmediateFuture(
             std::move(helper),
             unmountUnlessCancelled(
                 requestId,
                 absolutePathFromThrift(*mountPoint),
                 UnmountOptions{}))
      .semi();
}

folly::SemiFuture<folly::Unit> EdenServiceHandler::semifuture_unmountV2(
    std::unique_ptr<UnmountArgument> unmountArg) {
  auto requestContext = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      INFO, true, requestContext, *unmountArg->mountId()->mountPoint());
  server_->noteActivity("unmount");
  auto requestId = helper->getRequestId();
  return wrapImmediateFuture(
             std::move(helper),
             unmountUnlessCancelled(
                 requestId,
                 absolutePathFromThrift(*unmountArg->mountId()->mountPoint()),
                 UnmountOptions{.force = *unmountArg->useForce()}))
      .semi();
}

ImmediateFuture<folly::Unit> EdenServiceHandler::unmountUnlessCancelled(
    uint64_t requestId,
    AbsolutePath mountPath,
    UnmountOptions options) {
  // The unmount answers once the mount has shut down, and that shutdown
  // finishes on the main event base. EdenServer::stop() cancels every active
  // request and then waits for Thrift to drain them while the main event
  // base is not running, so an unmount that waited through the stop would
  // hold the drain until Thrift aborts the daemon. Cancellation answers the
  // request instead; the mount's shutdown carries on without it, and
  // unmountAll() waits for it during cleanup.
  struct Wait {
    folly::Promise<folly::Unit> promise;
    std::atomic<bool> answered{false};
    void answer(folly::Try<folly::Unit>&& result) {
      if (!answered.exchange(true)) {
        promise.setTry(std::move(result));
      }
    }
  };
  auto wait = std::make_shared<Wait>();
  auto answer = wait->promise.getSemiFuture();

  std::shared_ptr<folly::CancellationCallback> onCancel;
  if (auto token = getCancellationToken(requestId)) {
    onCancel = std::make_shared<
        folly::CancellationCallback>(std::move(*token), [wait, mountPath] {
      wait->answer(
          folly::Try<folly::Unit>{newEdenError(
              EdenErrorType::GENERIC_ERROR,
              fmt::format(
                  "EdenFS is stopping; the unmount of {} completes as part of "
                  "the shutdown",
                  mountPath))});
    });
  }

  folly::futures::detachOn(
      server_->getServerState()->getThreadPool().get(),
      makeImmediateFutureWith([this, mountPath, options] {
        return server_->unmount(mountPath, options);
      })
          .semi()
          .defer([wait](folly::Try<folly::Unit>&& result) {
            wait->answer(std::move(result));
          }));

  return ImmediateFuture<folly::Unit>{std::move(answer)}
      .thenError([this](const folly::exception_wrapper& ex) {
        ex.with_exception([&](const std::exception& e) {
          server_->getServerState()->getErrorLogger().log(
              EdenErrorInfo::thrift(
                  ErrorArg::fromExceptionWithoutTrace(e), "unmount"));
        });
        throw newEdenError(ex);
      })
      .ensure([onCancel = std::move(onCancel)] {});
}

void EdenServiceHandler::listMounts(std::vector<MountInfo>& results) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);
  auto visibleMountPoints = getMountPointsVisibleInDaemonNamespace();
  for (const auto& edenMount : server_->getAllMountPoints()) {
    MountInfo info;
    info.mountPoint() = absolutePathToThrift(edenMount->getPath());
    info.edenClientPath() = absolutePathToThrift(
        edenMount->getCheckoutConfig()->getClientDirectory());
    info.state() = edenMount->getState();
    info.backingRepoPath() = edenMount->getCheckoutConfig()->getRepoSource();
    if (auto* fsChannel = edenMount->getFsChannel()) {
      info.fsChannelType() = fsChannel->getName();
#ifdef __linux__
      if (auto* fuseChannel = dynamic_cast<FuseChannel*>(fsChannel)) {
        info.fuseTransport() = fuseChannel->getTransportName();
      }
#endif
#ifndef _WIN32
      if (dynamic_cast<Nfsd3*>(fsChannel) != nullptr) {
        // Every NFS mount shares the running mountd's transport.
        if (auto& nfsServer = server_->getServerState()->getNfsServer()) {
          info.nfsTransport() =
              nfsServer->getMountdAddr().getFamily() == AF_UNIX ? "unix"
                                                                : "tcp";
        }
      }
#endif
    }
    if (visibleMountPoints.has_value()) {
      info.visibleInDaemonNamespace() =
          visibleMountPoints->count(edenMount->getPath().asString()) != 0;
    }
    results.push_back(info);
  }
}

folly::SemiFuture<std::unique_ptr<std::vector<CheckoutConflict>>>
EdenServiceHandler::semifuture_checkOutRevision(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::string> hash,
    CheckoutMode checkoutMode,
    std::unique_ptr<CheckOutRevisionParams> params) {
  // Noted when the checkout starts and again when it ends, so that a
  // graceful restart asked to wait for an idle daemon stays away from it.
  server_->noteActivity("checkOutRevision");
  auto noteEnd = [this] { server_->noteActivity("checkOutRevision"); };
  auto config = server_->getServerState()->getEdenConfig();
  bool useCoroutines = config->enableCoroutines.getValue() &&
      config->enableCoroutinesPhase7.getValue();
  if (useCoroutines) {
    auto requestContext = getRequestContext();
    // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
    return folly::coro::co_invoke(
               [this](
                   apache::thrift::Cpp2RequestContext* ctx,
                   std::unique_ptr<std::string> mp,
                   std::unique_ptr<std::string> h,
                   CheckoutMode mode,
                   std::unique_ptr<CheckOutRevisionParams> p)
                   -> folly::coro::Task<
                       std::unique_ptr<std::vector<CheckoutConflict>>> {
                 co_return co_await co_checkOutRevisionImpl(
                     ctx, std::move(mp), std::move(h), mode, std::move(p));
               },
               requestContext,
               std::move(mountPoint),
               std::move(hash),
               checkoutMode,
               std::move(params))
        .semi()
        .deferEnsure(noteEnd);
  }
  return semifuture_checkOutRevisionImpl(
             std::move(mountPoint),
             std::move(hash),
             checkoutMode,
             std::move(params))
      .deferEnsure(noteEnd);
}

folly::coro::now_task<std::unique_ptr<std::vector<CheckoutConflict>>>
EdenServiceHandler::co_checkOutRevisionImpl(
    apache::thrift::Cpp2RequestContext* requestContext,
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::string> hash,
    CheckoutMode checkoutMode,
    std::unique_ptr<CheckOutRevisionParams> params) {
  // Yield once at entry so we don't tie up the thrift worker thread for the
  // synchronous prologue (lookupMount, parsing, helper construction).
  co_await folly::coro::co_reschedule_on_current_executor;

  auto rootIdOptions = params->rootIdOptions().ensure();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      DBG1,
      true,
      requestContext,
      *mountPoint,
      logHash(*hash),
      apache::thrift::util::enumName(checkoutMode, "(unknown)"),
      params->hgRootManifest().has_value() ? logHash(*params->hgRootManifest())
                                           : "(unspecified hg root manifest)",
      rootIdOptions.fid().has_value() ? folly::hexlify(*rootIdOptions.fid())
                                      : "no fid provided");
  helper->getThriftFetchContext().fillClientRequestInfo(params->cri());
  auto& fetchContext = helper->getFetchContext();

  auto mountHandle = lookupMount(mountPoint);

  // If we were passed a FilterID, create a RootID that contains the
  // filter and a varint that indicates the length of the original id.
  std::string parsedId =
      resolveRootId(std::move(*hash), rootIdOptions, mountHandle);
  hash.reset();

  auto mountPath = absolutePathFromThrift(*mountPoint);
  auto result = co_await server_->co_checkOutRevision(
      mountPath,
      parsedId,
      params->hgRootManifest().to_optional(),
      fetchContext,
      helper->getFunctionName(),
      checkoutMode);
  co_return std::make_unique<std::vector<CheckoutConflict>>(
      std::move(result.conflicts));
}

folly::SemiFuture<std::unique_ptr<std::vector<CheckoutConflict>>>
EdenServiceHandler::semifuture_checkOutRevisionImpl(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::string> hash,
    CheckoutMode checkoutMode,
    std::unique_ptr<CheckOutRevisionParams> params) {
  auto rootIdOptions = params->rootIdOptions().ensure();
  auto requestContext = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      DBG1,
      true,
      requestContext,
      *mountPoint,
      logHash(*hash),
      apache::thrift::util::enumName(checkoutMode, "(unknown)"),
      params->hgRootManifest().has_value() ? logHash(*params->hgRootManifest())
                                           : "(unspecified hg root manifest)",
      rootIdOptions.fid().has_value() ? folly::hexlify(*rootIdOptions.fid())
                                      : "no fid provided");
  helper->getThriftFetchContext().fillClientRequestInfo(params->cri());
  auto& fetchContext = helper->getFetchContext();

  auto mountHandle = lookupMount(mountPoint);

  // If we were passed a FilterID, create a RootID that contains the
  // filter and a varint that indicates the length of the original id.
  std::string parsedId =
      resolveRootId(std::move(*hash), rootIdOptions, mountHandle);
  hash.reset();

  auto mountPath = absolutePathFromThrift(*mountPoint);
  auto checkoutFuture = server_->checkOutRevision(
      mountPath,
      parsedId,
      params->hgRootManifest().to_optional(),
      fetchContext,
      helper->getFunctionName(),
      checkoutMode);

  return wrapImmediateFuture(
             std::move(helper),
             std::move(checkoutFuture).thenValue([](CheckoutResult&& result) {
               return std::make_unique<std::vector<CheckoutConflict>>(
                   std::move(result.conflicts));
             }))
      .semi();
}

folly::SemiFuture<folly::Unit>
EdenServiceHandler::semifuture_resetParentCommits(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<WorkingDirectoryParents> parents,
    std::unique_ptr<ResetParentCommitsParams> params) {
  server_->noteActivity("resetParentCommits");
  auto rootIdOptions = params->rootIdOptions().ensure();
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG1,
      *mountPoint,
      logHash(*parents->parent1()),
      params->hgRootManifest().has_value() ? logHash(*params->hgRootManifest())
                                           : "(unspecified hg root manifest)",
      rootIdOptions.fid().has_value() ? folly::hexlify(*rootIdOptions.fid())
                                      : "no fid provided");
  helper->getThriftFetchContext().fillClientRequestInfo(params->cri());

  auto mountHandle = lookupMount(mountPoint);

  // If we were passed a FilterID, create a RootID that contains the filter and
  // a varint that indicates the length of the original id.
  std::string parsedParent =
      resolveRootId(std::move(*parents->parent1()), rootIdOptions, mountHandle);
  auto parent1 = mountHandle.getObjectStore().parseRootId(parsedParent);

  auto fut = ImmediateFuture<folly::Unit>{std::in_place};
  if (params->hgRootManifest().has_value()) {
    auto& fetchContext = helper->getFetchContext();
    // The hg client has told us what the root manifest is.
    //
    // This is useful when a commit has just been created.  We won't be able to
    // ask the import helper to map the commit to its root manifest because it
    // won't know about the new commit until it reopens the repo.  Instead,
    // import the manifest for this commit directly.
    auto rootManifest = hash20FromThrift(*params->hgRootManifest());
    fut = mountHandle.getObjectStore().getBackingStore()->importManifestForRoot(
        parent1, rootManifest, fetchContext);
  }

  return wrapImmediateFuture(
             std::move(helper),
             std::move(fut).thenValue([parent1, mountHandle](folly::Unit) {
               mountHandle.getEdenMount().resetParent(parent1);
             }))
      .semi();
}

void EdenServiceHandler::getCurrentSnapshotInfo(
    GetCurrentSnapshotInfoResponse& out,
    std::unique_ptr<GetCurrentSnapshotInfoRequest> params) {
  auto mountId = params->mountId();
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountId);
  helper->getThriftFetchContext().fillClientRequestInfo(params->cri());

  auto mountHandle = lookupMount(*mountId);

  auto filterId =
      mountHandle.getEdenMount().getCheckoutConfig()->getLastActiveFilter();

  if (filterId.has_value()) {
    out.fid() = std::move(filterId.value());
  }
}

namespace {
int64_t getSyncTimeout(const SyncBehavior& sync) {
  return sync.syncTimeoutSeconds().value_or(60);
}

/**
 * Wait for all the pending notifications to be processed.
 *
 * When the SyncBehavior is unset, this default to a timeout of 60 seconds. A
 * negative SyncBehavior mean to wait indefinitely.
 */
folly::coro::now_task<void> co_waitForPendingWrites(
    const EdenMount& mount,
    const SyncBehavior& sync) {
  auto seconds = getSyncTimeout(sync);
  if (seconds == 0) {
    co_return;
  }
  if (seconds > 0) {
    co_await folly::coro::timeout(
        mount.co_waitForPendingWrites(), std::chrono::seconds{seconds});
  } else {
    co_await mount.co_waitForPendingWrites();
  }
  co_return;
}

/**
 * Wait for all the pending notifications to be processed.
 *
 * When the SyncBehavior is unset, this default to a timeout of 60 seconds. A
 * negative SyncBehavior mean to wait indefinitely.
 */
ImmediateFuture<folly::Unit> waitForPendingWrites(
    const EdenMount& mount,
    const SyncBehavior& sync) {
  // DEPRECATED: use co_waitForPendingWrites directly. Kept for the remaining
  // ImmediateFuture-based Thrift handler paths; delete after they migrate to
  // coroutines.
  auto mountHandle = EdenMountHandle{
      std::const_pointer_cast<EdenMount>(mount.shared_from_this()),
      mount.getRootInode()};
  return ImmediateFuture{
      // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
      folly::coro::co_invoke(
          [](auto&& mountPtr, auto&& sync) -> folly::coro::Task<folly::Unit> {
            co_await co_waitForPendingWrites(*mountPtr, sync);
            co_return folly::unit;
          },
          mount.shared_from_this(),
          sync)
          .semi()}
      .ensure([mountHandle = std::move(mountHandle)]() {});
}

} // namespace

folly::SemiFuture<folly::Unit>
EdenServiceHandler::semifuture_synchronizeWorkingCopy(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<SynchronizeWorkingCopyParams> params) {
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG3, *mountPoint, getSyncTimeout(*params->sync()));
  auto mountHandle = lookupMount(mountPoint);

  return wrapImmediateFuture(
             std::move(helper),
             waitForPendingWrites(mountHandle.getEdenMount(), *params->sync()))
      .ensure([mountHandle] {})
      .semi();
}

folly::coro::now_task<std::unique_ptr<std::vector<Blake3Result>>>
EdenServiceHandler::getBlake3Impl(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::vector<std::string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  TraceBlock block("getBlake3");
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG3, *mountPoint, getSyncTimeout(*sync), toLogArg(*paths));
  auto& fetchContext = helper->getFetchContext();
  auto mountHandle = lookupMount(mountPoint);
  auto objectStore = mountHandle.getObjectStorePtr();

  co_await co_waitForPendingWrites(mountHandle.getEdenMount(), *sync);

  auto results = co_await co_applyToVirtualInode(
      mountHandle.getRootInode(),
      *paths,
      [mountHandle, objectStore, fetchContext = fetchContext.copy()](
          VirtualInode inode,
          RelativePath path) -> folly::coro::now_task<Hash32> {
        co_return co_await inode.co_getBlake3(
            path, mountHandle.getObjectStorePtr(), fetchContext);
      },
      objectStore,
      fetchContext);

  auto out = std::make_unique<std::vector<Blake3Result>>();
  out->reserve(results.size());

  for (auto& result : results) {
    auto& blake3Result = out->emplace_back();
    if (result.hasValue()) {
      blake3Result.blake3() = thriftHash32(result.value());
    } else {
      blake3Result.error() = newEdenError(result.exception());
    }
  }

  co_return out;
}

folly::coro::Task<std::unique_ptr<std::vector<Blake3Result>>>
EdenServiceHandler::co_getBlake3(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::vector<std::string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  co_return co_await getBlake3Impl(
      std::move(mountPoint), std::move(paths), std::move(sync));
}

folly::coro::now_task<std::unique_ptr<std::vector<DigestHashResult>>>
EdenServiceHandler::getDigestHashImpl(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::vector<std::string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  TraceBlock block("getDigestHash");
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG3, *mountPoint, getSyncTimeout(*sync), toLogArg(*paths));
  auto& fetchContext = helper->getFetchContext();
  auto mountHandle = lookupMount(mountPoint);
  auto objectStore = mountHandle.getObjectStorePtr();

  co_await co_waitForPendingWrites(mountHandle.getEdenMount(), *sync);

  auto results = co_await co_applyToVirtualInode(
      mountHandle.getRootInode(),
      *paths,
      [objectStore, fetchContext = fetchContext.copy()](
          VirtualInode inode,
          RelativePath path) -> folly::coro::now_task<std::optional<Hash32>> {
        co_return co_await inode.co_getDigestHash(
            path, objectStore, fetchContext);
      },
      objectStore,
      fetchContext);

  auto out = std::make_unique<std::vector<DigestHashResult>>();
  out->reserve(results.size());

  for (auto& result : results) {
    auto& digestHashResult = out->emplace_back();
    if (result.hasValue()) {
      if (result.value().has_value()) {
        digestHashResult.digestHash() = thriftHash32(result.value().value());
      } else {
        digestHashResult.error() = newEdenError(
            ENOENT,
            EdenErrorType::ATTRIBUTE_UNAVAILABLE,
            "tree aux data missing for tree");
      }
    } else {
      digestHashResult.error() = newEdenError(result.exception());
    }
  }

  co_return out;
}

folly::coro::Task<std::unique_ptr<std::vector<DigestHashResult>>>
EdenServiceHandler::co_getDigestHash(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::vector<std::string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  co_return co_await getDigestHashImpl(
      std::move(mountPoint), std::move(paths), std::move(sync));
}

folly::coro::now_task<std::unique_ptr<std::vector<SHA1Result>>>
EdenServiceHandler::getSHA1Impl(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::vector<std::string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  TraceBlock block("getSHA1");
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG3, *mountPoint, getSyncTimeout(*sync), toLogArg(*paths));
  auto& fetchContext = helper->getFetchContext();
  auto mountHandle = lookupMount(mountPoint);
  auto objectStore = mountHandle.getObjectStorePtr();

  co_await co_waitForPendingWrites(mountHandle.getEdenMount(), *sync);

  auto results = co_await co_applyToVirtualInode(
      mountHandle.getRootInode(),
      *paths,
      [objectStore, fetchContext = fetchContext.copy()](
          VirtualInode inode,
          RelativePath path) -> folly::coro::now_task<Hash20> {
        co_return co_await inode.co_getSHA1(path, objectStore, fetchContext);
      },
      objectStore,
      fetchContext);

  auto out = std::make_unique<std::vector<SHA1Result>>();
  out->reserve(results.size());

  for (auto& result : results) {
    auto& sha1Result = out->emplace_back();
    if (result.hasValue()) {
      sha1Result.sha1() = thriftHash20(result.value());
    } else {
      sha1Result.error() = newEdenError(result.exception());
    }
  }

  co_return out;
}

folly::coro::Task<std::unique_ptr<std::vector<SHA1Result>>>
EdenServiceHandler::co_getSHA1(
    std::unique_ptr<string> mountPoint,
    std::unique_ptr<vector<string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  co_return co_await getSHA1Impl(
      std::move(mountPoint), std::move(paths), std::move(sync));
}

folly::SemiFuture<folly::Unit> EdenServiceHandler::semifuture_addBindMount(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::string> repoPathStr,
    std::unique_ptr<std::string> targetPath) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountHandle = lookupMount(mountPoint);

  auto repoPath = RelativePathPiece{*repoPathStr};
  auto absRepoPath = mountHandle.getEdenMount().getPath() + repoPath;
  auto* privHelper = server_->getServerState()->getPrivHelper();

  auto fut = mountHandle.getEdenMount().ensureDirectoryExists(
      repoPath, helper->getFetchContext());
  return std::move(fut)
      .thenValue([self = shared_from_this(),
                  privHelper,
                  target = absolutePathFromThrift(*targetPath),
                  pathInMountDir = absRepoPath.copy(),
                  mountPath = mountHandle.getEdenMount().getPath().asString(),
                  filePath = absRepoPath.asString()](TreeInodePtr) {
        return privHelper->bindMount(target.view(), pathInMountDir.view())
            .thenError([self, mountPath, filePath](
                           const folly::exception_wrapper& ex) {
              ex.with_exception([&](const std::exception& e) {
                self->server_->getServerState()->getErrorLogger().log(
                    EdenErrorInfo::privhelper(
                        ErrorArg::fromExceptionWithoutTrace(e))
                        .withMountPoint(mountPath)
                        .withFilePath(filePath)
                        .withErrorType("bind_mount"));
              });
              throw newEdenError(ex);
            });
      })
      .ensure([mountHandle, helper = std::move(helper)] {})
      .semi();
}

folly::SemiFuture<folly::Unit> EdenServiceHandler::semifuture_removeBindMount(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::string> repoPathStr) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountHandle = lookupMount(mountPoint);

  auto repoPath = RelativePathPiece{*repoPathStr};
  auto absRepoPath = mountHandle.getEdenMount().getPath() + repoPath;
  return server_->getServerState()
      ->getPrivHelper()
      ->bindUnMount(absRepoPath.view())
      .thenError([this,
                  self = shared_from_this(),
                  mountPath = mountHandle.getEdenMount().getPath().asString(),
                  filePath = absRepoPath.asString()](
                     const folly::exception_wrapper& ex) {
        ex.with_exception([&](const std::exception& e) {
          server_->getServerState()->getErrorLogger().log(
              EdenErrorInfo::privhelper(ErrorArg::fromExceptionWithoutTrace(e))
                  .withMountPoint(mountPath)
                  .withFilePath(filePath)
                  .withErrorType("bind_unmount"));
        });
        throw newEdenError(ex);
      })
      .ensure([mountHandle, helper = std::move(helper)] {})
      .semi();
}

void EdenServiceHandler::getCurrentJournalPosition(
    JournalPosition& out,
    std::unique_ptr<std::string> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountHandle = lookupMount(mountPoint);
  auto latest = mountHandle.getEdenMount().getJournal().observeLatest();

  out.mountGeneration() = mountHandle.getEdenMount().getMountGeneration();
  if (latest) {
    out.sequenceNumber() = latest->sequenceID;
    out.snapshotHash() =
        mountHandle.getObjectStore().renderRootId(latest->toRoot);
  } else {
    out.sequenceNumber() = 0;
    out.snapshotHash() = mountHandle.getObjectStore().renderRootId(RootId{});
  }
}

void EdenServiceHandler::peekCurrentJournalPosition(
    PeekCurrentJournalPositionResponse& out,
    std::unique_ptr<PeekCurrentJournalPositionRequest> params) {
  auto mountId = params->mountId();
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountId);
  auto mountHandle = lookupMount(*mountId);
  auto latest = mountHandle.getEdenMount().getJournal().peekLatest();

  auto& pos = out.position().ensure();
  pos.mountGeneration() = mountHandle.getEdenMount().getMountGeneration();
  if (latest) {
    pos.sequenceNumber() = latest->sequenceID;
    pos.snapshotHash() =
        mountHandle.getObjectStore().renderRootId(latest->toRoot);
  } else {
    pos.sequenceNumber() = 0;
    pos.snapshotHash() = mountHandle.getObjectStore().renderRootId(RootId{});
  }
}

apache::thrift::ServerStream<JournalPosition>
EdenServiceHandler::subscribeStreamTemporary(
    std::unique_ptr<std::string> mountPoint) {
  return streamJournalChanged(std::move(mountPoint));
}

apache::thrift::ServerStream<JournalPosition>
EdenServiceHandler::streamJournalChanged(
    std::unique_ptr<std::string> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountHandle = lookupMount(mountPoint);

  // We need a weak ref on the mount because the thrift stream plumbing
  // may outlive the mount point
  std::weak_ptr<EdenMount> weakMount(mountHandle.getEdenMountPtr());

  // We'll need to pass the subscriber id to both the disconnect
  // and change callbacks.  We can't know the id until after we've
  // created them both, so we need to share an optional id between them.
  auto handle = std::make_shared<std::optional<Journal::SubscriberId>>();
  auto disconnected = std::make_shared<std::atomic<bool>>(false);
  auto shuttingDown = folly::copy(streamJournalChangedShuttingDown_);

  // This is called when the subscription channel is torn down
  auto onDisconnect = [weakMount, handle, disconnected] {
    XLOG(INFO, "streaming client disconnected");
    auto mount = weakMount.lock();
    if (mount) {
      disconnected->store(true);
      mount->getJournal().cancelSubscriber(handle->value());
    }
  };

  // Set up the actual publishing instance
  auto streamAndPublisher =
      apache::thrift::ServerStream<JournalPosition>::createPublisher(
          std::move(onDisconnect));

  // A little wrapper around the StreamPublisher.
  // This is needed because the destructor for StreamPublisherState
  // triggers a FATAL if the stream has not been completed.
  // We don't have an easy way to trigger this outside of just calling
  // it in a destructor, so that's what we do here.
  struct Publisher {
    apache::thrift::ServerStreamPublisher<JournalPosition> publisher;
    std::shared_ptr<std::atomic<bool>> disconnected;
    std::shared_ptr<std::atomic<bool>> shuttingDown;
    // Set while a delayed notification is scheduled; further changes in the
    // window are covered by it.
    std::atomic<bool> notifyPending{false};

    explicit Publisher(
        apache::thrift::ServerStreamPublisher<JournalPosition> publisher,
        std::shared_ptr<std::atomic<bool>> disconnected,
        std::shared_ptr<std::atomic<bool>> shuttingDown)
        : publisher(std::move(publisher)),
          disconnected(std::move(disconnected)),
          shuttingDown(std::move(shuttingDown)) {}

    ~Publisher() {
      // We have to send an exception as part of the completion, otherwise
      // thrift doesn't seem to notify the peer of the shutdown
      if (!disconnected->load()) {
        if (shuttingDown->load()) {
          std::move(publisher).complete(
              folly::make_exception_wrapper<EdenError>(newEdenError(
                  EdenErrorType::SHUTTING_DOWN, "edenfs is shutting down")));
        } else {
          std::move(publisher).complete(
              folly::make_exception_wrapper<std::runtime_error>(
                  "subscriber terminated"));
        }
      }
    }
  };

  auto stream = std::make_shared<Publisher>(
      std::move(streamAndPublisher.second),
      std::move(disconnected),
      std::move(shuttingDown));

  // The position value sent below is intentionally undefined and should not
  // be used. Instead, the subscriber should call getCurrentJournalPosition or
  // getFilesChangedSince.
  auto notify = [](Publisher& stream) {
    if (stream.disconnected->load() || stream.shuttingDown->load()) {
      return;
    }
    JournalPosition pos;
    stream.publisher.next(pos);
  };

  // Register onJournalChange with the journal subsystem, and assign
  // the subscriber id into the handle so that the callbacks can consume it.
  // folly's timekeeper resolves microseconds, so a configured delay below
  // that takes the immediate path rather than a sleep of zero.
  const auto delay = std::chrono::duration_cast<std::chrono::microseconds>(
      server_->getServerState()
          ->getEdenConfig()
          ->thriftJournalChangedDelay.getValue());
  auto executor = server_->getServerState()->getThreadPool();
  handle->emplace(mountHandle.getEdenMount().getJournal().registerSubscriber(
      [stream = std::move(stream), notify, delay = delay, executor]() mutable {
        if (delay.count() == 0) {
          notify(*stream);
          return;
        }
        // A write burst can change the journal tens of thousands of times a
        // second. One notification at the end of the window covers all of
        // them, since the subscriber reads the journal position itself.
        if (stream->notifyPending.exchange(true)) {
          return;
        }
        folly::futures::sleep(delay)
            .via(executor.get())
            // The thread pool has no keep-alive support, so the task itself
            // holds the executor until it has run.
            .thenTry([stream, notify, executor](folly::Try<folly::Unit>&&) {
              // Reset on every outcome. If the executor refused the task at
              // shutdown the flag would otherwise stay set and starve the
              // subscriber of every later notification.
              stream->notifyPending.store(false);
              notify(*stream);
            });
      }));

  return std::move(streamAndPublisher.first);
}

void EdenServiceHandler::beginStreamJournalChangedShutdown() {
  streamJournalChangedShuttingDown_->store(
      server_->getServerState()
          ->getEdenConfig()
          ->thriftStreamJournalChangedShuttingDownError.getValue());
}

namespace {
TraceEventTimes thriftTraceEventTimes(const TraceEventBase& event) {
  using namespace std::chrono;

  TraceEventTimes times;
  times.timestamp() =
      duration_cast<nanoseconds>(event.systemTime.time_since_epoch()).count();
  times.monotonic_time_ns() =
      duration_cast<nanoseconds>(event.monotonicTime.time_since_epoch())
          .count();
  return times;
}

RequestInfo thriftRequestInfo(pid_t pid, ProcessInfoCache& processInfoCache) {
  RequestInfo info;
  info.pid() = pid;
  info.processName().from_optional(processInfoCache.getProcessName(pid));
  return info;
}

template <typename T>
class ThriftStreamPublisherOwner {
 public:
  explicit ThriftStreamPublisherOwner(
      apache::thrift::ServerStreamPublisher<T> publisher)
      : owner(true), publisher{std::move(publisher)} {}

  ThriftStreamPublisherOwner(ThriftStreamPublisherOwner&& that) noexcept
      : owner{std::exchange(that.owner, false)},
        publisher{std::move(that.publisher)} {}

  ThriftStreamPublisherOwner& operator=(ThriftStreamPublisherOwner&&) = delete;

  void next(T payload) const {
    if (owner) {
      publisher.next(std::move(payload));
    }
  }

  void next(folly::exception_wrapper ew) && {
    if (owner) {
      owner = false;
      std::move(publisher).complete(std::move(ew));
    }
  }

  // Destroying a publisher without calling complete() aborts the process, so
  // ensure complete() is called when this object is dropped.
  ~ThriftStreamPublisherOwner() {
    if (owner) {
      std::move(publisher).complete();
    }
  }

 private:
  bool owner;
  apache::thrift::ServerStreamPublisher<T> publisher;
};

/**
 * What keeps a trace stream open: the request scope, so the stream counts as
 * an outstanding Thrift request for as long as it is open, and the TraceBus
 * subscription whose subscriber owns the stream publisher.
 *
 * The stream ends when the subscription is dropped: the bus thread destroys
 * the subscriber, and the ThriftStreamPublisherOwner it holds completes the
 * publisher. That happens when the client disconnects, which destroys this
 * object, and when the request is cancelled, which EdenServiceHandler does for
 * every stream when EdenFS stops.
 */
template <typename Subscription>
class TraceStreamSubscription {
 public:
  TraceStreamSubscription(
      std::unique_ptr<ThriftRequestScope> helper,
      std::optional<folly::CancellationToken> cancellationToken,
      std::shared_ptr<folly::Executor> executor)
      : helper_{std::move(helper)}, executor_{std::move(executor)} {
    if (cancellationToken) {
      onCancel_.emplace(std::move(*cancellationToken), [this] { close(); });
    }
  }

  TraceStreamSubscription(const TraceStreamSubscription&) = delete;
  TraceStreamSubscription(TraceStreamSubscription&&) = delete;
  TraceStreamSubscription& operator=(const TraceStreamSubscription&) = delete;
  TraceStreamSubscription& operator=(TraceStreamSubscription&&) = delete;

  ~TraceStreamSubscription() {
    // Completing the publisher can run the stream's completion callback, and
    // with it this destructor, on the TraceBus thread. The request scope's
    // destructor publishes to the Thrift request bus, which may be that very
    // bus, so it must not run there.
    executor_->add([helper = std::move(helper_)]() mutable { helper.reset(); });
  }

  /**
   * Store the subscription that feeds the stream. A subscription that arrives
   * after the request was cancelled is dropped right away, which ends the
   * stream before the client sees an event.
   */
  void subscribe(Subscription subscription) {
    auto state = state_.wlock();
    if (!state->closed) {
      state->subscription.emplace(std::move(subscription));
    }
  }

 private:
  void close() {
    auto state = state_.wlock();
    state->closed = true;
    state->subscription.reset();
  }

  struct State {
    bool closed = false;
    std::optional<Subscription> subscription;
  };

  std::unique_ptr<ThriftRequestScope> helper_;
  // Owned rather than kept alive: the pool does not hand out a keep-alive.
  std::shared_ptr<folly::Executor> executor_;
  folly::Synchronized<State> state_;
  std::optional<folly::CancellationCallback> onCancel_;
};

template <typename Subscription>
std::shared_ptr<TraceStreamSubscription<Subscription>>
makeTraceStreamSubscription(
    EdenServiceHandler& handler,
    std::shared_ptr<folly::Executor> executor,
    std::unique_ptr<ThriftRequestScope> helper) {
  auto requestId = helper->getRequestId();
  handler.markRequestAsStream(requestId);
  auto cancellationToken = handler.getCancellationToken(requestId);
  return std::make_shared<TraceStreamSubscription<Subscription>>(
      std::move(helper), std::move(cancellationToken), std::move(executor));
}

} // namespace

#ifndef _WIN32

namespace {
FuseCall populateFuseCall(
    uint64_t unique,
    const FuseTraceEvent::RequestHeader& request,
    ProcessInfoCache& processInfoCache) {
  FuseCall fc;
  fc.opcode() = request.opcode;
  fc.unique() = unique;
  fc.nodeid() = request.nodeid;
  fc.uid() = request.uid;
  fc.gid() = request.gid;
  fc.pid() = request.pid;

  fc.opcodeName() = fuseOpcodeName(request.opcode);
  fc.processName().from_optional(processInfoCache.getProcessName(request.pid));
  return fc;
}

NfsCall populateNfsCall(const NfsTraceEvent& event) {
  NfsCall nfsCall;
  nfsCall.xid() = event.getXid();
  nfsCall.procNumber() = event.getProcNumber();
  nfsCall.procName() = nfsProcName(event.getProcNumber());
  return nfsCall;
}

/**
 * Returns true if event should not be traced.
 */

bool isEventMasked(
    int64_t eventCategoryMask,
    ProcessAccessLog::AccessType accessType) {
  using AccessType = ProcessAccessLog::AccessType;
  switch (accessType) {
    case AccessType::FsChannelRead:
      return 0 == (eventCategoryMask & streamingeden_constants::FS_EVENT_READ_);
    case AccessType::FsChannelWrite:
      return 0 ==
          (eventCategoryMask & streamingeden_constants::FS_EVENT_WRITE_);
    case AccessType::FsChannelOther:
    default:
      return 0 ==
          (eventCategoryMask & streamingeden_constants::FS_EVENT_OTHER_);
  }
}

bool isEventMasked(int64_t eventCategoryMask, const FuseTraceEvent& event) {
  return isEventMasked(
      eventCategoryMask, fuseOpcodeAccessType(event.getRequest().opcode));
}

bool isEventMasked(int64_t eventCategoryMask, const NfsTraceEvent& event) {
  return isEventMasked(
      eventCategoryMask, nfsProcAccessType(event.getProcNumber()));
}

} // namespace

#endif //!_WIN32

#ifdef _WIN32
PrjfsCall populatePrjfsCall(
    const PrjfsTraceCallType callType,
    const PrjfsTraceEvent::PrjfsOperationData& data) {
  PrjfsCall prjfsCall;
  prjfsCall.callType_ref() = callType;
  prjfsCall.commandId_ref() = data.commandId;
  prjfsCall.pid_ref() = data.pid;
  return prjfsCall;
}

PrjfsCall populatePrjfsCall(const PrjfsTraceEvent& event) {
  return populatePrjfsCall(event.getCallType(), event.getData());
}
#endif

ThriftRequestMetadata populateThriftRequestMetadata(
    const ThriftRequestTraceEvent& request) {
  ThriftRequestMetadata thriftRequestMetadata;
  thriftRequestMetadata.requestId() = request.requestId;
  thriftRequestMetadata.method() = request.method;
  if (auto client_pid = request.clientPid) {
    thriftRequestMetadata.clientPid() = client_pid.value().get();
  }
  return thriftRequestMetadata;
}

/**
 * Helper function to convert a ThriftRequestTraceEvent to a ThriftRequestEvent
 * type. Used in EdenServiceHandler::traceThriftRequestEvents and
 * EdenServiceHandler::getRetroactiveThriftRequestEvents.
 */
void convertThriftRequestTraceEventToThriftRequestEvent(
    const ThriftRequestTraceEvent& event,
    ThriftRequestEvent& te) {
  te.times() = thriftTraceEventTimes(event);
  switch (event.type) {
    case ThriftRequestTraceEvent::START:
      te.eventType() = ThriftRequestEventType::START;
      break;
    case ThriftRequestTraceEvent::FINISH:
      te.eventType() = ThriftRequestEventType::FINISH;
      break;
  }
  te.requestMetadata() = populateThriftRequestMetadata(event);
}

apache::thrift::ServerStream<ThriftRequestEvent>
EdenServiceHandler::traceThriftRequestEvents() {
  auto requestContext = getRequestContext();
  auto helper =
      INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(DBG3, true, requestContext);

  auto stream = makeTraceStreamSubscription<
      TraceBus<ThriftRequestTraceEvent>::SubscriptionHandle>(
      *this, getTraceStreamExecutor(), std::move(helper));

  auto [serverStream, publisher] =
      apache::thrift::ServerStream<ThriftRequestEvent>::createPublisher(
          [stream] {
            // on disconnect, release the subscription and the request scope
          });

  stream->subscribe(thriftRequestTraceBus_->subscribeFunction(
      "Live Thrift request tracing",
      [publisher_2 = ThriftStreamPublisherOwner{std::move(publisher)}](
          const ThriftRequestTraceEvent& event) mutable {
        ThriftRequestEvent thriftEvent;
        convertThriftRequestTraceEventToThriftRequestEvent(event, thriftEvent);
        publisher_2.next(thriftEvent);
      }));

  return std::move(serverStream);
}

apache::thrift::ServerStream<TaskEvent> EdenServiceHandler::traceTaskEvents(
    std::unique_ptr<::facebook::eden::TraceTaskEventsRequest> /* request */) {
  auto requestContext = getRequestContext();
  auto helper =
      INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(DBG3, true, requestContext);

  auto stream =
      makeTraceStreamSubscription<TraceBus<TaskTraceEvent>::SubscriptionHandle>(
          *this, getTraceStreamExecutor(), std::move(helper));

  auto [serverStream, publisher] =
      apache::thrift::ServerStream<TaskEvent>::createPublisher([stream] {
        // on disconnect, release the subscription and the request scope
      });

  stream->subscribe(
      TaskTraceEvent::getTraceBus()->subscribeFunction(
          "Live Thrift request tracing",
          [publisher_2 = ThriftStreamPublisherOwner{std::move(publisher)}](
              const TaskTraceEvent& event) mutable {
            TaskEvent taskEvent;
            taskEvent.times() = thriftTraceEventTimes(event);
            taskEvent.name() = event.name;
            taskEvent.threadName() = event.threadName;
            taskEvent.threadId() = event.threadId;
            taskEvent.duration() = event.duration.count();
            taskEvent.start() = event.start.count();
            publisher_2.next(taskEvent);
          }));

  return std::move(serverStream);
}

apache::thrift::ServerStream<FsEvent> EdenServiceHandler::traceFsEvents(
    std::unique_ptr<std::string> mountPoint,
    int64_t eventCategoryMask) {
  auto requestContext = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      DBG3, true, requestContext, *mountPoint);
  auto mountHandle = lookupMount(mountPoint);
  auto& edenMount = mountHandle.getEdenMount();

  // Treat an empty bitset as an unfiltered stream. This is for clients that
  // predate the addition of the mask and for clients that don't care.
  // 0 would be meaningless anyway: it would never return any events.
  if (0 == eventCategoryMask) {
    eventCategoryMask = ~0;
  }

  struct FsTraceSubscription {
    // While subscribed to FuseChannel's TraceBus, request detailed argument
    // strings.
    TraceDetailedArgumentsHandle argHandle;
#ifdef _WIN32
    TraceSubscriptionHandle<PrjfsTraceEvent> subHandle;
#else
    std::variant<
        TraceSubscriptionHandle<FuseTraceEvent>,
        TraceSubscriptionHandle<NfsTraceEvent>>
        subHandle;
#endif // _WIN32
  };

  FsTraceSubscription subscription;
#ifdef _WIN32
  auto prjfsChannel = edenMount.getPrjfsChannel()->getInner();
  if (prjfsChannel) {
    subscription.argHandle = prjfsChannel->traceDetailedArguments();
  } else {
    EDEN_BUG() << "tracing isn't supported yet for the "
               << fmt::underlying(
                      edenMount.getCheckoutConfig()->getMountProtocol())
               << " filesystem type";
  }
#else
  auto* fuseChannel = edenMount.getFuseChannel();
  auto* nfsdChannel = edenMount.getNfsdChannel();
  if (fuseChannel) {
    subscription.argHandle = fuseChannel->traceDetailedArguments();
  } else if (nfsdChannel) {
    subscription.argHandle = nfsdChannel->traceDetailedArguments();
  } else {
    EDEN_BUG() << "tracing isn't supported yet for the "
               << fmt::underlying(
                      edenMount.getCheckoutConfig()->getMountProtocol())
               << " filesystem type";
  }
#endif // _WIN32

  auto stream = makeTraceStreamSubscription<FsTraceSubscription>(
      *this, getTraceStreamExecutor(), std::move(helper));

  auto [serverStream, publisher] =
      apache::thrift::ServerStream<FsEvent>::createPublisher([stream] {
        // on disconnect, release the subscription and the request scope
      });

#ifdef _WIN32
  if (prjfsChannel) {
    subscription.subHandle = prjfsChannel->getTraceBusPtr()->subscribeFunction(
        fmt::format("strace-{}", edenMount.getPath().basename()),
        [publisher = ThriftStreamPublisherOwner{std::move(publisher)}](
            const PrjfsTraceEvent& event) {
          FsEvent te;
          auto times = thriftTraceEventTimes(event);
          te.times_ref() = times;

          // Legacy timestamp fields.
          te.timestamp_ref() = *times.timestamp_ref();
          te.monotonic_time_ns_ref() = *times.monotonic_time_ns_ref();

          te.prjfsRequest_ref() = populatePrjfsCall(event);

          switch (event.getType()) {
            case PrjfsTraceEvent::START:
              te.type_ref() = FsEventType::START;
              if (auto& arguments = event.getArguments()) {
                te.arguments_ref() = *arguments;
              }
              break;
            case PrjfsTraceEvent::FINISH:
              te.type_ref() = FsEventType::FINISH;
              auto result = event.getResult();
              if (result) {
                te.result() = *result;
              }
              break;
          }

          te.requestInfo_ref() = RequestInfo{};

          publisher.next(te);
        });
  }
#else
  if (fuseChannel) {
    subscription.subHandle = fuseChannel->getTraceBus().subscribeFunction(
        fmt::format("strace-{}", edenMount.getPath().basename()),
        [publisher_2 = ThriftStreamPublisherOwner{std::move(publisher)},
         serverState = server_->getServerState(),
         eventCategoryMask](const FuseTraceEvent& event) {
          if (isEventMasked(eventCategoryMask, event)) {
            return;
          }

          FsEvent te;
          auto times = thriftTraceEventTimes(event);
          te.times() = times;

          // Legacy timestamp fields.
          te.timestamp() = *times.timestamp();
          te.monotonic_time_ns() = *times.monotonic_time_ns();

          te.fuseRequest() = populateFuseCall(
              event.getUnique(),
              event.getRequest(),
              *serverState->getProcessInfoCache());

          switch (event.getType()) {
            case FuseTraceEvent::START:
              te.type() = FsEventType::START;
              if (auto& arguments = event.getArguments()) {
                te.arguments() = *arguments;
              }
              break;
            case FuseTraceEvent::FINISH:
              te.type() = FsEventType::FINISH;
              te.result().from_optional(event.getResponseCode());
              break;
          }

          te.requestInfo() = thriftRequestInfo(
              event.getRequest().pid, *serverState->getProcessInfoCache());

          publisher_2.next(te);
        });
  } else if (nfsdChannel) {
    subscription.subHandle = nfsdChannel->getTraceBus().subscribeFunction(
        fmt::format("strace-{}", edenMount.getPath().basename()),
        [publisher_2 = ThriftStreamPublisherOwner{std::move(publisher)},
         eventCategoryMask](const NfsTraceEvent& event) {
          if (isEventMasked(eventCategoryMask, event)) {
            return;
          }

          FsEvent te;
          auto times = thriftTraceEventTimes(event);
          te.times() = times;

          // Legacy timestamp fields.
          te.timestamp() = *times.timestamp();
          te.monotonic_time_ns() = *times.monotonic_time_ns();

          te.nfsRequest() = populateNfsCall(event);

          switch (event.getType()) {
            case NfsTraceEvent::START:
              te.type() = FsEventType::START;
              if (auto arguments = event.getArguments()) {
                te.arguments() = arguments.value();
              }
              break;
            case NfsTraceEvent::FINISH:
              te.type() = FsEventType::FINISH;
              break;
          }

          te.requestInfo() = RequestInfo{};

          publisher_2.next(te);
        });
  }
#endif // _WIN32
  stream->subscribe(std::move(subscription));
  return std::move(serverStream);
}

/**
 * Unwraps a possible FilteredBackingStore to find the concrete
 * SaplingBackingStore underneath, if any. Returns nullptr (never throws) if
 * backingStore is not, and does not wrap, a SaplingBackingStore - for
 * callers where a non-Sapling backing store is an expected, benign case
 * (e.g. hgcache stats are simply unavailable for a Git/RE-CAS-backed mount)
 * rather than a hard error. See castToSaplingBackingStore for the
 * throwing variant used where a non-Sapling mount really is a caller error.
 */
std::shared_ptr<SaplingBackingStore> tryCastToSaplingBackingStore(
    std::shared_ptr<BackingStore>& backingStore) {
  // If FilteredFS is enabled, we'll see a FilteredBackingStore first
  auto filteredBackingStore =
      std::dynamic_pointer_cast<FilteredBackingStore>(backingStore);
  if (filteredBackingStore) {
    // FilteredBackingStore -> SaplingBackingStore
    return std::dynamic_pointer_cast<SaplingBackingStore>(
        filteredBackingStore->getBackingStore());
  }
  // BackingStore -> SaplingBackingStore
  return std::dynamic_pointer_cast<SaplingBackingStore>(backingStore);
}

/**
 * Helper function to get a cast a BackingStore shared_ptr to a
 * SaplingBackingStore shared_ptr. Returns an error if the type of backingStore
 * provided is not truly an SaplingBackingStore. Used in
 * EdenServiceHandler::traceHgEvents,
 * EdenServiceHandler::getRetroactiveHgEvents and
 * EdenServiceHandler::debugOutstandingHgEvents.
 */
std::shared_ptr<SaplingBackingStore> castToSaplingBackingStore(
    std::shared_ptr<BackingStore>& backingStore,
    AbsolutePathPiece mountPath) {
  auto saplingBackingStore = tryCastToSaplingBackingStore(backingStore);

  if (!saplingBackingStore) {
    // typeid() does not evaluate expressions
    auto& r = *backingStore.get();
    throw newEdenError(
        EdenErrorType::GENERIC_ERROR,
        fmt::format(
            "mount {} must use SaplingBackingStore, type is {}",
            mountPath,
            typeid(r).name()));
  }

  return saplingBackingStore;
}

/**
 * Helper function to convert an HgImportTraceEvent to a thrift HgEvent type.
 * Used in EdenServiceHandler::traceHgEvents,
 * EdenServiceHandler::getRetroactiveHgEvents and
 * EdenServiceHandler::debugOutstandingHgEvents.
 */
void convertHgImportTraceEventToHgEvent(
    const HgImportTraceEvent& event,
    ProcessInfoCache& processInfoCache,
    HgEvent& te) {
  te.times() = thriftTraceEventTimes(event);
  switch (event.eventType) {
    case HgImportTraceEvent::QUEUE:
      te.eventType() = HgEventType::QUEUE;
      break;
    case HgImportTraceEvent::START:
      te.eventType() = HgEventType::START;
      break;
    case HgImportTraceEvent::FINISH:
      te.eventType() = HgEventType::FINISH;
      break;
  }

  switch (event.resourceType) {
    case HgImportTraceEvent::BLOB:
      te.resourceType() = HgResourceType::BLOB;
      break;
    case HgImportTraceEvent::TREE:
      te.resourceType() = HgResourceType::TREE;
      break;
    case HgImportTraceEvent::BLOB_AUX:
      te.resourceType() = HgResourceType::BLOBMETA;
      break;
    case HgImportTraceEvent::TREE_AUX:
      te.resourceType() = HgResourceType::TREEMETA;
      break;
    case HgImportTraceEvent::BLOB_BATCH:
      te.resourceType() = HgResourceType::BLOBBATCH;
      break;
  }

  switch (event.importPriority) {
    case ImportPriority::Class::Low:
      te.importPriority() = HgImportPriority::LOW;
      break;
    case ImportPriority::Class::Normal:
      te.importPriority() = HgImportPriority::NORMAL;
      break;
    case ImportPriority::Class::High:
      te.importPriority() = HgImportPriority::HIGH;
      break;
  }

  switch (event.importCause) {
    case ObjectFetchContext::Cause::Unknown:
    case ObjectFetchContext::Cause::InodeGc:
      te.importCause() = HgImportCause::UNKNOWN;
      break;
    case ObjectFetchContext::Cause::Fs:
      te.importCause() = HgImportCause::FS;
      break;
    case ObjectFetchContext::Cause::Thrift:
      te.importCause() = HgImportCause::THRIFT;
      break;
    case ObjectFetchContext::Cause::Prefetch:
      te.importCause() = HgImportCause::PREFETCH;
      break;
    case ObjectFetchContext::Cause::Glob:
      te.importCause() = HgImportCause::THRIFT;
      break;
  }

  if (event.fetchedSource.has_value()) {
    switch (event.fetchedSource.value()) {
      case ObjectFetchContext::FetchedSource::Local:
        te.fetchedSource() = FetchedSource::LOCAL;
        break;
      case ObjectFetchContext::FetchedSource::Remote:
        te.fetchedSource() = FetchedSource::REMOTE;
        break;
      case ObjectFetchContext::FetchedSource::Unknown:
        te.fetchedSource() = FetchedSource::UNKNOWN;
        break;
    }
  } else {
    te.fetchedSource() = FetchedSource::NOT_AVAILABLE_YET;
  }

  te.unique() = event.unique;

  te.manifestNodeId() = event.manifestNodeId.toString();
  te.path() = event.getPath();

  if (auto pid = event.pid) {
    te.requestInfo() = thriftRequestInfo(pid.value().get(), processInfoCache);
  }
}

apache::thrift::ServerStream<HgEvent> EdenServiceHandler::traceHgEvents(
    std::unique_ptr<std::string> mountPoint) {
  auto requestContext = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      DBG3, true, requestContext, *mountPoint);
  auto mountHandle = lookupMount(mountPoint);
  auto backingStore = mountHandle.getObjectStore().getBackingStore();
  std::shared_ptr<SaplingBackingStore> saplingBackingStore =
      castToSaplingBackingStore(
          backingStore, mountHandle.getEdenMount().getPath());

  auto stream =
      makeTraceStreamSubscription<TraceSubscriptionHandle<HgImportTraceEvent>>(
          *this, getTraceStreamExecutor(), std::move(helper));

  auto [serverStream, publisher] =
      apache::thrift::ServerStream<HgEvent>::createPublisher([stream] {
        // on disconnect, release the subscription and the request scope
      });

  stream->subscribe(saplingBackingStore->getTraceBus().subscribeFunction(
      fmt::format(
          "hgtrace-{}", mountHandle.getEdenMount().getPath().basename()),
      [publisher_2 = ThriftStreamPublisherOwner{std::move(publisher)},
       processInfoCache =
           mountHandle.getEdenMount().getServerState()->getProcessInfoCache()](
          const HgImportTraceEvent& event) {
        HgEvent thriftEvent;
        convertHgImportTraceEventToHgEvent(
            event, *processInfoCache, thriftEvent);
        publisher_2.next(thriftEvent);
      }));

  return std::move(serverStream);
}

/**
 * Helper function to convert an InodeTraceEvent to a thrift InodeEvent type.
 * Used in EdenServiceHandler::traceInodeEvents and
 * EdenServiceHandler::getRetroactiveInodeEvents. Note paths are not set here
 * and are set by the calling functions. For traceInodeEvents full paths may
 * need to be computed whereas for getRetroactiveInodeEvents full paths would
 * have already been computed when the event gets added to the ActivityBuffer.
 */
void ConvertInodeTraceEventToThriftInodeEvent(
    InodeTraceEvent traceEvent,
    InodeEvent& thriftEvent) {
  thriftEvent.times() = thriftTraceEventTimes(traceEvent);
  thriftEvent.ino() = traceEvent.ino.getRawValue();
  thriftEvent.inodeType() = traceEvent.inodeType;
  thriftEvent.eventType() = traceEvent.eventType;
  thriftEvent.progress() = traceEvent.progress;
  thriftEvent.duration() = traceEvent.duration.count();
  // TODO: trace requesting pid
  // thriftEvent.requestInfo() = thriftRequestInfo(pid);
}

apache::thrift::ServerStream<InodeEvent> EdenServiceHandler::traceInodeEvents(
    std::unique_ptr<std::string> mountPoint) {
  auto requestContext = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      DBG3, true, requestContext, *mountPoint);
  auto mountHandle = lookupMount(mountPoint);
  auto inodeMap = mountHandle.getEdenMount().getInodeMap();

  auto stream =
      makeTraceStreamSubscription<TraceSubscriptionHandle<InodeTraceEvent>>(
          *this, getTraceStreamExecutor(), std::move(helper));

  auto [serverStream, publisher] =
      apache::thrift::ServerStream<InodeEvent>::createPublisher([stream] {
        // on disconnect, release the subscription and the request scope
      });

  stream->subscribe(
      mountHandle.getEdenMount().getInodeTraceBus().subscribeFunction(
          fmt::format(
              "inodetrace-{}", mountHandle.getEdenMount().getPath().basename()),
          [publisher_2 = ThriftStreamPublisherOwner{std::move(publisher)},
           inodeMap](const InodeTraceEvent& event) {
            InodeEvent thriftEvent;
            ConvertInodeTraceEventToThriftInodeEvent(event, thriftEvent);
            try {
              auto relativePath = inodeMap->getPathForInode(event.ino);
              if (relativePath) {
                thriftEvent.path() = relativePath->asString();
              } else {
                thriftEvent.path() = event.getPath();
              }
            } catch (const std::system_error& /* e */) {
              thriftEvent.path() = event.getPath();
            }
            publisher_2.next(thriftEvent);
          }));

  return std::move(serverStream);
}

namespace {
void checkMountGeneration(
    const JournalPosition& position,
    const EdenMount& mount,
    std::string_view fieldName) {
  if (folly::to_unsigned(*position.mountGeneration()) !=
      mount.getMountGeneration()) {
    throw newEdenError(
        ERANGE,
        EdenErrorType::MOUNT_GENERATION_CHANGED,
        fieldName,
        ".mountGeneration does not match the current "
        "mountGeneration.  "
        "You need to compute a new basis for delta queries.");
  }
}

void publishFile(
    const folly::Synchronized<ThriftStreamPublisherOwner<ChangedFileResult>>&
        publisher,
    folly::StringPiece path,
    ScmFileStatus status,
    dtype_t type) {
  ChangedFileResult fileResult;
  fileResult.name() = path.str();
  fileResult.status() = status;
  fileResult.dtype() = static_cast<Dtype>(type);
  publisher.rlock()->next(std::move(fileResult));
}

/**
 * This method computes all uncommitted changes and save the result to publisher
 */
void sumUncommittedChanges(
    const JournalDeltaRange& range,
    const folly::Synchronized<ThriftStreamPublisherOwner<ChangedFileResult>>&
        publisher,
    std::optional<std::reference_wrapper<GlobFilter>> filter) {
  for (auto& entry : range.changedFilesInOverlay) {
    const auto& changeInfo = entry.second;

    // the path is filtered don't consider it
    if (filter) {
      // TODO(T167750650): This .get() will block Thrift threads and could lead
      // to Queue Timeouts. Instead of calling .get(), we should chain futures
      // together.
      if (filter->get()
              .getFilterCoverageForPath(entry.first, folly::StringPiece(""))
              .get() == FilterCoverage::RECURSIVELY_FILTERED) {
        continue;
      }
    }

    ScmFileStatus status;
    if (!changeInfo.existedBefore && changeInfo.existedAfter) {
      status = ScmFileStatus::ADDED;
    } else if (changeInfo.existedBefore && !changeInfo.existedAfter) {
      status = ScmFileStatus::REMOVED;
    } else {
      status = ScmFileStatus::MODIFIED;
    }

    publishFile(publisher, entry.first.asString(), status, dtype_t::Unknown);
  }

  for (const auto& name : range.uncleanPaths) {
    if (filter) {
      // TODO(T167750650): This .get() will block Thrift threads and could lead
      // to Queue Timeouts. Instead of calling .get(), we should chain futures
      // together.
      if (filter->get()
              .getFilterCoverageForPath(name, folly::StringPiece(""))
              .get() == FilterCoverage::RECURSIVELY_FILTERED) {
        continue;
      }
    }
    publishFile(
        publisher, name.asString(), ScmFileStatus::MODIFIED, dtype_t::Unknown);
  }
}

class StreamingDiffCallback : public DiffCallback {
 public:
  explicit StreamingDiffCallback(
      std::shared_ptr<
          folly::Synchronized<ThriftStreamPublisherOwner<ChangedFileResult>>>
          publisher)
      : publisher_{std::move(publisher)} {}

  void ignoredPath(RelativePathPiece, dtype_t) override {}

  void addedPath(RelativePathPiece path, dtype_t type) override {
    publishFile(*publisher_, path.view(), ScmFileStatus::ADDED, type);
  }

  void removedPath(RelativePathPiece path, dtype_t type) override {
    publishFile(*publisher_, path.view(), ScmFileStatus::REMOVED, type);
  }

  void modifiedPath(RelativePathPiece path, dtype_t type) override {
    publishFile(*publisher_, path.view(), ScmFileStatus::MODIFIED, type);
  }

  void diffError(RelativePathPiece /*path*/, const folly::exception_wrapper& ew)
      override {
    auto publisher = std::move(*publisher_->wlock());
    std::move(publisher).next(newEdenError(ew));
  }

 private:
  std::shared_ptr<
      folly::Synchronized<ThriftStreamPublisherOwner<ChangedFileResult>>>
      publisher_;
};

/**
 * Compute the difference between the passed in roots.
 *
 * The order of the roots matters: a file added in toRoot will be returned as
 * ScmFileStatus::ADDED, while if the order of arguments were reversed, it
 * would be returned as ScmFileStatus::REMOVED.
 */
ImmediateFuture<folly::Unit> diffBetweenRoots(
    const RootId& fromRoot,
    const RootId& toRoot,
    const CheckoutConfig& checkoutConfig,
    const std::shared_ptr<ObjectStore>& objectStore,
    const folly::CancellationToken& cancellation,
    const ObjectFetchContextPtr& fetchContext,
    DiffCallback* callback) {
  auto diffContext = std::make_unique<DiffContext>(
      callback,
      cancellation,
      fetchContext,
      true,
      checkoutConfig.getCaseSensitive(),
      objectStore,
      nullptr);
  auto fut = diffRoots(diffContext.get(), fromRoot, toRoot);
  return std::move(fut).ensure([diffContext = std::move(diffContext)] {});
}

} // namespace

apache::thrift::ResponseAndServerStream<ChangesSinceResult, ChangedFileResult>
EdenServiceHandler::streamChangesSince(
    std::unique_ptr<StreamChangesSinceParams> params) {
  auto requestContext = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_STAT_AND_CANCELLATION(
      DBG3,
      &ThriftStats::streamChangesSince,
      true,
      requestContext,
      *params->mountPoint());
  auto mountHandle = lookupMount(params->mountPoint());
  const auto& fromPosition = *params->fromPosition();
  auto& fetchContext = helper->getFetchContext();

  // Streaming in Thrift can be done via a Stream Generator, or via a Stream
  // Publisher. We're using the latter here as the former can only be used with
  // coroutines which EdenFS hasn't been converted to. Generators also have the
  // property to be driven by the client, internally, Thrift will wait for the
  // client to have consumed an element before requesting more from the server.
  // Publishers on the other hand are driven by the server and are publishing
  // as fast as possible.
  //
  // What this means is that in the case where EdenFS can publish elements
  // faster than the client can read them, EdenFS's memory usage can grow
  // potentially unbounded.

  checkMountGeneration(
      fromPosition, mountHandle.getEdenMount(), "fromPosition"sv);

  // The +1 is because the core merge stops at the item prior to
  // its limitSequence parameter and we want the changes *since*
  // the provided sequence number.
  auto summed = mountHandle.getJournal().accumulateRange(
      *fromPosition.sequenceNumber() + 1);

  ChangesSinceResult result;
  if (!summed) {
    // No changes, just return the fromPosition and an empty stream.
    result.toPosition() = fromPosition;

    return {
        std::move(result),
        apache::thrift::ServerStream<ChangedFileResult>::createEmpty()};
  }

  if (summed->isTruncated) {
    throw newEdenError(
        EDOM,
        EdenErrorType::JOURNAL_TRUNCATED,
        "Journal entry range has been truncated.");
  }

  auto cancellationSource = std::make_shared<folly::CancellationSource>();
  // Cancelling the request, which EdenFS does for a forced restart, stops the
  // diffs the same way a client disconnect does.
  std::shared_ptr<folly::CancellationCallback> onRequestCancel;
  if (auto requestToken = getCancellationToken(helper->getRequestId())) {
    onRequestCancel = std::make_shared<folly::CancellationCallback>(
        std::move(*requestToken),
        [cancellationSource] { cancellationSource->requestCancellation(); });
  }
  auto [serverStream, publisher] =
      apache::thrift::ServerStream<ChangedFileResult>::createPublisher(
          [cancellationSource] { cancellationSource->requestCancellation(); });
  auto sharedPublisherLock = std::make_shared<
      folly::Synchronized<ThriftStreamPublisherOwner<ChangedFileResult>>>(
      ThriftStreamPublisherOwner{std::move(publisher)});

  RootIdCodec& rootIdCodec = mountHandle.getObjectStore();

  JournalPosition toPosition;
  toPosition.mountGeneration() =
      mountHandle.getEdenMount().getMountGeneration();
  toPosition.sequenceNumber() = summed->toSequence;
  toPosition.snapshotHash() =
      rootIdCodec.renderRootId(summed->snapshotTransitions.back());
  result.toPosition() = toPosition;

  sumUncommittedChanges(*summed, *sharedPublisherLock, std::nullopt);

  if (summed->snapshotTransitions.size() > 1) {
    auto callback =
        std::make_shared<StreamingDiffCallback>(sharedPublisherLock);

    std::vector<ImmediateFuture<folly::Unit>> futures;
    for (auto rootIt = summed->snapshotTransitions.begin();
         std::next(rootIt) != summed->snapshotTransitions.end();
         ++rootIt) {
      const auto& from = *rootIt;
      const auto& to = *(rootIt + 1);

      // We want to make sure the diff is performed on a background thread so
      // the Thrift client can interrupt us whenever desired. To do this, let's
      // start from an not ready ImmediateFuture.
      futures.push_back(
          makeNotReadyImmediateFuture().thenValue(
              [from,
               to,
               mountHandle,
               token = cancellationSource->getToken(),
               fetchContext = fetchContext.copy(),
               callback = callback.get()](auto&&) {
                return diffBetweenRoots(
                    from,
                    to,
                    *mountHandle.getEdenMount().getCheckoutConfig(),
                    mountHandle.getObjectStorePtr(),
                    token,
                    fetchContext,
                    callback);
              }));
    }

    folly::futures::detachOn(
        server_->getServerState()->getThreadPool().get(),
        collectAllSafe(std::move(futures))
            // Make sure that the edenMount, callback, helper and
            // cancellationSource lives for the duration of the stream by
            // copying them.
            .thenTry(
                [mountHandle,
                 sharedPublisherLock,
                 callback = std::move(callback),
                 helper = std::move(helper),
                 onRequestCancel = std::move(onRequestCancel),
                 cancellationSource](
                    folly::Try<std::vector<folly::Unit>>&& result) mutable {
                  if (result.hasException()) {
                    auto sharedPublisher =
                        std::move(*sharedPublisherLock->wlock());
                    std::move(sharedPublisher)
                        .next(newEdenError(std::move(result).exception()));
                  } else if (cancellationSource->getToken()
                                 .isCancellationRequested()) {
                    // A cancelled diff stops early. A client that is still
                    // connected must not take the stream for complete.
                    auto sharedPublisher =
                        std::move(*sharedPublisherLock->wlock());
                    std::move(sharedPublisher)
                        .next(newEdenError(
                            folly::make_exception_wrapper<
                                folly::OperationCancelled>()));
                  }
                })
            .semi());
  }

  return {std::move(result), std::move(serverStream)};
}

std::pair<std::vector<RelativePath>, std::vector<RelativePath>>
buildIncludedAndExcludedRoots(
    bool includeVCSRoots,
    bool includeStateChanges,
    const std::vector<RelativePath>& vcsDirectories,
    const std::vector<PathString>& includedRoots,
    const std::vector<PathString>& excludedRoots,
    const RelativePathPiece& root,
    const RelativePathPiece& notificationsStateDirectory) {
  // This uses/returns RelativePath instead of RelativePathPiece due to the
  // value constructed with the root + include/excludeRoot going out of
  // scope
  std::vector<RelativePath> outIncludedRoots;
  outIncludedRoots.reserve(includedRoots.size());
  // If there are includedRoots, append them to the root if there
  // is one, otherwise just fill out the vector with the values
  if (includedRoots.size() > 0) {
    std::transform(
        includedRoots.begin(),
        includedRoots.end(),
        std::back_inserter(outIncludedRoots),
        [root](const PathString& includedRoot) {
          return root + relpathPieceFromUserPath(includedRoot);
        });
  } else if (!root.empty()) {
    // If there are no includedRoots and there is a root, use
    // it as an includedRoot
    outIncludedRoots.emplace_back(root);
  }

  std::vector<RelativePath> outExcludedRoots(excludedRoots.size());
  std::transform(
      excludedRoots.begin(),
      excludedRoots.end(),
      outExcludedRoots.begin(),
      [root](const PathString& excludedRoot) {
        return root + relpathPieceFromUserPath(excludedRoot);
      });

  if (!includeVCSRoots) {
    outExcludedRoots.insert(
        outExcludedRoots.end(), vcsDirectories.begin(), vcsDirectories.end());
  } else if (!outIncludedRoots.empty()) {
    outIncludedRoots.insert(
        outIncludedRoots.end(), vcsDirectories.begin(), vcsDirectories.end());
  }

  if (!includeStateChanges) {
    outExcludedRoots.emplace_back(notificationsStateDirectory);
  } else if (!outIncludedRoots.empty()) {
    outIncludedRoots.emplace_back(notificationsStateDirectory);
  }

  return std::make_pair(
      std::move(outIncludedRoots), std::move(outExcludedRoots));
}

/*
 * Determines if a given path should be returned based on the includedRoots and
 * excludedRoots provided by the caller of changesSinceV2.
 *
 */
bool isPathIncluded(
    const std::vector<RelativePath>& includedRoots,
    const std::vector<RelativePath>& excludedRoots,
    const std::vector<std::string>& includedSuffixes,
    const std::vector<std::string>& excludedSuffixes,
    const RelativePath& path,
    dtype_t type) {
  if (!includedRoots.empty()) {
    bool included = false;
    // test to see if path matches includedRoots - include the path
    for (const auto& includedRoot : includedRoots) {
      if (includedRoot == path || includedRoot.isParentDirOf(path)) {
        included = true;
        break;
      }
    }

    // includedRoots not empty and no match - do not include the path
    if (!included) {
      return false;
    }
  }

  // Ignore suffixes for directory changes
  if (!includedSuffixes.empty() && type != dtype_t::Dir) {
    bool included = false;
    // test to see if path matches includedSuffixes - include the path
    for (const auto& includedSuffix : includedSuffixes) {
      if (ends_with(path.asString(), includedSuffix)) {
        included = true;
        break;
      }
    }

    // includedSuffixes not empty and no match - do not include the path
    if (!included) {
      return false;
    }
  }

  // if exclude filter is not empty
  if (!excludedRoots.empty()) {
    // test to see if path matches excludedRoots - exclude the path
    for (const auto& excludedRoot : excludedRoots) {
      if (excludedRoot == path || excludedRoot.isParentDirOf(path)) {
        return false;
      }
    }
  }

  if (!excludedSuffixes.empty() && type != dtype_t::Dir) {
    for (const auto& excludedSuffix : excludedSuffixes) {
      if (ends_with(path.asString(), excludedSuffix)) {
        return false;
      }
    }
  }

  // Path should be included
  return true;
}

void EdenServiceHandler::sync_changesSinceV2(
    ChangesSinceV2Result& result,
    std::unique_ptr<ChangesSinceV2Params> params) {
  uint64_t numSmallChanges = 0;
  uint64_t numStateChanges = 0;
  uint64_t numRenamedDirectory = 0;
  uint64_t numCommitTransition = 0;
  std::optional<uint64_t> lostChangesReason;
  uint64_t numFilteredResults = 0;

  auto mountHandle = lookupMount(params->mountPoint());
  const auto& fromPosition = *params->fromPosition();
  RelativePathPiece root = params->root().has_value()
      ? RelativePathPiece{params->root().value()}
      : RelativePathPiece{};

  auto includedRoots = params->includedRoots().has_value()
      ? params->includedRoots().value()
      : std::vector<PathString>{};
  auto excludedRoots = params->excludedRoots().has_value()
      ? params->excludedRoots().value()
      : std::vector<PathString>{};

  auto includedSuffixes = params->includedSuffixes().has_value()
      ? params->includedSuffixes().value()
      : std::vector<std::string>{};
  auto excludedSuffixes = params->excludedSuffixes().has_value()
      ? params->excludedSuffixes().value()
      : std::vector<std::string>{};

  bool includeVCSRoots = params->includeVCSRoots().has_value()
      ? params->includeVCSRoots().value()
      : false;

  auto includeStateChanges = params->includeStateChanges().has_value()
      ? params->includeStateChanges().value()
      : false;

  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG3,
      *params->mountPoint(),
      fmt::format(
          "fromPosition={}, root:{}, includedRoots:{}, excludedRoots:{}, includedSuffixes:{}, excludedSuffixes:{}, includeVCSRoots: {}, includeStateChanges: {}",
          logPosition(fromPosition),
          root,
          toLogArg(includedRoots),
          toLogArg(excludedRoots),
          toLogArg(includedSuffixes),
          toLogArg(excludedSuffixes),
          includeVCSRoots,
          includeStateChanges));

  auto latestJournalEntry = mountHandle.getJournal().observeLatest();
  std::optional<JournalDelta::SequenceNumber> toSequence;
  RootId toSnapshotId = RootId{};
  if (latestJournalEntry.has_value()) {
    toSequence = latestJournalEntry->sequenceID;
    toSnapshotId = latestJournalEntry->toRoot;
  }
  RootId currentRoot = toSnapshotId;
  RootIdCodec& rootIdCodec = mountHandle.getObjectStore();

  auto& faultInjector = server_->getServerState()->getFaultInjector();
  faultInjector.checkAsync("changesSince", "sequence").get();

  auto& fetchContext = helper->getFetchContext();

  auto& mount = mountHandle.getEdenMount();
  auto& mountPath = mount.getPath();
  if (!root.empty()) {
    bool rootExists =
        waitForPendingWrites(mount, *params->sync())
            .thenValue(
                [&mount, root, fetchContext = fetchContext.copy()](auto&&) {
                  return mount.getVirtualInode(root, fetchContext)
                      .thenTry([](folly::Try<VirtualInode> tree) mutable {
                        if (tree.hasException()) {
                          // Root does not exist, or something else went wrong
                          return false;
                        }
                        // Files are not valid roots
                        return tree.value().isDirectory();
                      });
                })
            .get();
    if (!rootExists) {
      throw newEdenError(
          EINVAL,
          EdenErrorType::ARGUMENT_ERROR,
          fmt::format("Invalid root path \"{}\" in mount {}", root, mountPath));
    }
  }

  // Has EdenFS restarted or remounted
  if (folly::to_unsigned(*fromPosition.mountGeneration()) !=
      mountHandle.getEdenMount().getMountGeneration()) {
    if (!toSequence.has_value()) {
      // If there is no journal entry after restart/remount it should be
      // OK to use the initial SequenceNumber - 1.
      toSequence = 1;
    }
    LostChanges lostChanges;
    lostChanges.reason() = LostChangesReason::EDENFS_REMOUNTED;

    LargeChangeNotification largeChange;
    largeChange.lostChanges() = std::move(lostChanges);

    ChangeNotification change;
    change.largeChange() = std::move(largeChange);
    lostChangesReason =
        static_cast<uint64_t>(LostChangesReason::EDENFS_REMOUNTED);

    result.changes()->push_back(std::move(change));
  } else {
    // TODO: move to helper
    auto config = server_->getServerState()->getEdenConfig();
    auto maxNumberOfChanges = config->notifyMaxNumberOfChanges.getValue();
    auto notificationsStateDirectory =
        config->notificationsStateDirectory.getValue();
    auto includedAndExcludedRoots = buildIncludedAndExcludedRoots(
        includeVCSRoots,
        includeStateChanges,
        config->vcsDirectories.getValue(),
        includedRoots,
        excludedRoots,
        root,
        notificationsStateDirectory);
    auto includedAndExcludedSuffixes =
        std::make_pair(includedSuffixes, excludedSuffixes);

    const auto isTruncated = mountHandle.getJournal().forEachDelta(
        *fromPosition.sequenceNumber() + 1,
        std::nullopt,
        [&](const FileChangeJournalDelta& current) -> bool {
          if (!current.isPath1Valid) {
            XLOG(
                DFATAL,
                "FileChangeJournalDetal::isPath1Valid should never be false");
            server_->getServerState()->getErrorLogger().log(
                EdenErrorInfo::thrift(
                    "FileChangeJournalDelta::isPath1Valid should never be false",
                    "changesSince"));
          }

          // Even though we picked up toSequence previously, if there was a
          // change in between then and calling forEachDelta the latest sequence
          // could be different.
          if (toSequence.has_value() &&
              current.sequenceID > toSequence.value()) {
            toSequence = current.sequenceID;
          }

          // Check if it's a notifications state event.
          // Changes that happen inside the notifications state directory
          // will not be reported with other changes.
          if (notificationsStateDirectory.isParentDirOf(current.path1)) {
            XLOGF(
                DBG3,
                "Eden notifications file event at path {}",
                current.path1.asString());
            const auto& info = current.info1;
            ChangeNotification change;
            StateChangeNotification stateChange;
            if (ends_with(current.path1.asString(), ".notify")) {
              if (!info.existedBefore) {
                StateEntered stateEntered;
                XLOGF(
                    DBG3,
                    "Entered notifications state {}",
                    current.path1.stem().asString());
                stateEntered.name() = current.path1.stem().asString();
                stateChange.stateEntered() = std::move(stateEntered);
              } else if (!info.existedAfter) {
                StateLeft stateLeft;
                XLOGF(
                    DBG3,
                    "Left notifications state {}",
                    current.path1.stem().asString());
                stateLeft.name() = current.path1.stem().asString();
                stateChange.stateLeft() = std::move(stateLeft);
              } else {
                // Modified state file happens on linux platforms immediately
                // after creation. Ignore it, since it doesn't change the state
                return true;
              }
            } else {
              // Other changes caused by the locking mechanism. Ignored
              // Return value = Should continue
              return true;
            }
            if (includeStateChanges) {
              StateChangeNotification stateChangeCopy =
                  StateChangeNotification(stateChange);
              change.stateChange() = std::move(stateChange);
              result.changes()->push_back(std::move(change));
              numStateChanges += 1;
            }
            // Return value = Should continue
            return true;
          }

          // Changes can effect either path1 or both paths
          // Determine if path1 pass the filters and default path2 to not
          bool includePath1 = isPathIncluded(
              includedAndExcludedRoots.first,
              includedAndExcludedRoots.second,
              includedAndExcludedSuffixes.first,
              includedAndExcludedSuffixes.second,
              current.path1,
              current.type);
          bool includePath2 = false;

          ChangeNotification change;
          LargeChangeNotification largeChange;
          SmallChangeNotification smallChange;

          if (current.isPath2Valid) {
            // Determine if path2 passes filters, but only if path1 one
            // doesn't to avoid an extra lookup
            includePath2 = includePath1
                ? false
                : isPathIncluded(
                      includedAndExcludedRoots.first,
                      includedAndExcludedRoots.second,
                      includedAndExcludedSuffixes.first,
                      includedAndExcludedSuffixes.second,
                      current.path2,
                      current.type);

            if (includePath1 || includePath2) {
              const auto& info = current.info2;
              // NOTE: we could do a bunch of runtime checks here to
              // validate the infoN states would be a lot simpler if we
              // removed this infoN state and replaced them with a simple
              // enum

              // Constructs a Piece from the RelativePath
              RelativePathPiece pathString1 = current.path1;
              RelativePathPiece pathString2 = current.path2;

              // If root is empty, returns true if pathString is not also empty
              bool path1InRoot = pathString1.isSubDirOf(root);
              bool path2InRoot = pathString2.isSubDirOf(root);

              // if state change, skip over root handling
              if (!root.empty()) {
                // Filters include the path that matches the filter, but we want
                // to exclude it from a root
                // This is to match watchman's behavior regarding
                // relative roots.
                if (pathString1 == root || pathString2 == root) {
                  // Return value ignored here
                  return true;
                }
                // Trim the root + the separator
                if (path1InRoot) {
                  pathString1 = pathString1.substr(root.view().size() + 1);
                }
                if (path2InRoot) {
                  pathString2 = pathString2.substr(root.view().size() + 1);
                }
              }

              if (path1InRoot && path2InRoot) {
                if (info.existedBefore) {
                  // Replaced
                  Replaced replaced;
                  replaced.from() = pathString1.asString();
                  replaced.to() = pathString2.asString();
                  replaced.fileType() = static_cast<Dtype>(current.type);
                  smallChange.replaced() = std::move(replaced);
                  change.smallChange() = std::move(smallChange);
                  numSmallChanges += 1;
                } else {
                  // Renamed
                  if (current.type == dtype_t::Dir) {
                    DirectoryRenamed directoryRenamed;
                    directoryRenamed.from() = pathString1.asString();
                    directoryRenamed.to() = pathString2.asString();
                    largeChange.directoryRenamed() =
                        std::move(directoryRenamed);
                    change.largeChange() = std::move(largeChange);
                    numRenamedDirectory += 1;
                  } else {
                    Renamed renamed;
                    renamed.from() = pathString1.asString();
                    renamed.to() = pathString2.asString();
                    renamed.fileType() = static_cast<Dtype>(current.type);
                    smallChange.renamed() = std::move(renamed);
                    change.smallChange() = std::move(smallChange);
                    numSmallChanges += 1;
                  }
                }
              } else {
                if (path1InRoot) {
                  // File/Directory was renamed or replaced to a path outside of
                  // root. Report change as removed.
                  Removed removed;
                  removed.path() = pathString1.asString();
                  removed.fileType() = static_cast<Dtype>(current.type);
                  smallChange.removed() = std::move(removed);
                  change.smallChange() = std::move(smallChange);
                  numSmallChanges += 1;
                } else {
                  // File/Directory was renamed or replaced to a path inside of
                  // root. Report change as added (if renamed) or modified (if
                  // replaced).
                  if (info.existedBefore) {
                    // Modified
                    Modified modified;
                    modified.path() = pathString2.asString();
                    modified.fileType() = static_cast<Dtype>(current.type);
                    smallChange.modified() = std::move(modified);
                    change.smallChange() = std::move(smallChange);
                    numSmallChanges += 1;
                  } else {
                    Added added;
                    added.path() = pathString2.asString();
                    added.fileType() = static_cast<Dtype>(current.type);
                    smallChange.added() = std::move(added);
                    change.smallChange() = std::move(smallChange);
                    numSmallChanges += 1;
                  }
                }
              }
            }
          }
          // All single file changes have path1 pass the filters
          else if (includePath1) {
            const auto& info = current.info1;

            // Filters include the path that matches the filter, but we want
            // to exclude it from a root
            // This is to match watchman's behavior regarding
            // relative roots.
            if (!root.empty() && current.path1 == root) {
              // Return value ignored here
              return true;
            }

            // If a root is specified, it should be present due to being added
            // to includedRoots. Strip it and the first '/' out

            // Need to explicitly allocate storage in this scope for
            // mac/windows
            RelativePathPiece pathString = current.path1;

            // if state change, or VCS entry, skip over roots truncation
            if (!root.empty() && pathString.isSubDirOf(root)) {
              pathString = pathString.substr(root.view().size());
            }
            if (!info.existedBefore) {
              // Added
              Added added;
              added.path() = pathString.asString();
              added.fileType() = static_cast<Dtype>(current.type);
              smallChange.added() = std::move(added);
              change.smallChange() = std::move(smallChange);
              numSmallChanges += 1;
            } else if (!info.existedAfter) {
              // Removed
              Removed removed;
              removed.path() = pathString.asString();
              removed.fileType() = static_cast<Dtype>(current.type);
              smallChange.removed() = std::move(removed);
              change.smallChange() = std::move(smallChange);
              numSmallChanges += 1;
            } else {
              // Modified
              Modified modified;
              modified.path() = pathString.asString();
              modified.fileType() = static_cast<Dtype>(current.type);
              smallChange.modified() = std::move(modified);
              change.smallChange() = std::move(smallChange);
              numSmallChanges += 1;
            }
          }

          // Include a change if either path passes the filters or is a dir, and
          // the dir rename is in the roots
          if (includePath1 || includePath2) {
            result.changes()->push_back(std::move(change));
          } else {
            numFilteredResults += 1;
          }
          // Return value ignored here
          return true;
        },
        [&](const RootUpdateJournalDelta& current) -> bool {
          if (toSequence.has_value() &&
              current.sequenceID > toSequence.value()) {
            toSequence = current.sequenceID;
          }

          CommitTransition commitTransition;
          commitTransition.from() = rootIdCodec.renderRootId(current.fromRoot);
          commitTransition.to() = rootIdCodec.renderRootId(currentRoot);
          currentRoot = current.fromRoot;

          LargeChangeNotification largeChange;
          largeChange.commitTransition() = std::move(commitTransition);

          ChangeNotification change;
          change.largeChange() = std::move(largeChange);

          result.changes()->push_back(std::move(change));
          numCommitTransition += 1;
          // Return value ignored here
          return true;
        });

    if (isTruncated || result.changes()->size() > maxNumberOfChanges) {
      LostChanges lostChanges;
      lostChanges.reason() = isTruncated ? LostChangesReason::JOURNAL_TRUNCATED
                                         : LostChangesReason::TOO_MANY_CHANGES;
      lostChangesReason = static_cast<uint64_t>(lostChanges.reason().value());

      LargeChangeNotification largeChange;
      largeChange.lostChanges() = std::move(lostChanges);

      ChangeNotification change;
      change.largeChange() = std::move(largeChange);

      result.changes()->clear();
      result.changes()->push_back(std::move(change));
    } else {
      // TODO: this will be replace soon with in order processing. For now
      // we can accept a slight performance hit here by reversing the vector.

      // Results are neither truncated nor too many to return - reverse the
      // order to be oldest to newest
      std::reverse(result.changes()->begin(), result.changes()->end());
    }
  }

  if (!toSequence.has_value()) {
    // No changes, just use fromPosition.
    result.toPosition() = fromPosition;
  } else {
    JournalPosition toPosition;
    toPosition.mountGeneration() =
        mountHandle.getEdenMount().getMountGeneration();
    toPosition.sequenceNumber() = toSequence.value();
    toPosition.snapshotHash() = rootIdCodec.renderRootId(toSnapshotId);

    result.toPosition() = std::move(toPosition);
  }

  server_->getServerState()->getNotificationsStructuredLogger()->logEvent(
      ChangesSince{
          getClientCmdline(server_->getServerState(), fetchContext),
          logPosition(fromPosition),
          mountPath.asString(),
          root.asString(),
          includedRoots,
          excludedRoots,
          includedSuffixes,
          excludedSuffixes,
          includeVCSRoots,
          numSmallChanges,
          numStateChanges,
          numRenamedDirectory,
          numCommitTransition,
          lostChangesReason,
          numFilteredResults,
      });
}

folly::SemiFuture<std::unique_ptr<StartFileAccessMonitorResult>>
EdenServiceHandler::semifuture_startFileAccessMonitor(
    [[maybe_unused]] std::unique_ptr<StartFileAccessMonitorParams> params) {
#ifdef __APPLE__
  auto helper = INSTRUMENT_THRIFT_CALL(DBG1, *params->paths_ref());

  constexpr std::string_view FAM_TMP_OUTPUT_DIR = "/tmp/edenfs/fam/";

  std::string tmpPath = fmt::format("{}fam_XXXXXX", FAM_TMP_OUTPUT_DIR);
  const auto outputFd = mkstemp(tmpPath.data());
  folly::checkUnixError(outputFd, "failed to create FAM output file");
  folly::File outputFile(outputFd, /*ownsFd=*/true);
  folly::checkUnixError(
      fcntl(outputFd, F_SETFD, FD_CLOEXEC), "setting FAM output close-on-exec");

  auto fut = ImmediateFuture<pid_t>(
      server_->getServerState()->getPrivHelper()->startFam(
          *params->paths(),
          tmpPath,
          params->specifiedOutputPath().value_or(tmpPath),
          *params->shouldUpload_ref(),
          std::move(outputFile)));
  return wrapImmediateFuture(
             std::move(helper),
             std::move(fut).thenValue(
                 [tmpPath = std::move(tmpPath)](pid_t pid) mutable {
                   auto out = std::make_unique<StartFileAccessMonitorResult>();
                   out->pid() = pid;
                   out->tmpOutputPath() = std::move(tmpPath);
                   return out;
                 }))
      .semi();
#else // !__APPLE__
  NOT_IMPLEMENTED();
#endif
}

folly::SemiFuture<std::unique_ptr<StopFileAccessMonitorResult>>
EdenServiceHandler::semifuture_stopFileAccessMonitor() {
#ifdef __APPLE__
  auto helper = INSTRUMENT_THRIFT_CALL(DBG1);

  auto fut = ImmediateFuture<StopFileAccessMonitorResponse>(
      server_->getServerState()->getPrivHelper()->stopFam());

  return wrapImmediateFuture(
             std::move(helper), std::move(fut).thenValue([&](auto&& response) {
               auto out = std::make_unique<StopFileAccessMonitorResult>();
               out->tmpOutputPath() = response.tmpOutputPath;
               out->specifiedOutputPath() = response.specifiedOutputPath;
               out->shouldUpload() = response.shouldUpload;
               return out;
             }))
      .semi();
#else // !__APPLE__
  NOT_IMPLEMENTED();
#endif
}

void EdenServiceHandler::sendNotification(
    [[maybe_unused]] SendNotificationResponse&,
    [[maybe_unused]] std::unique_ptr<SendNotificationRequest> request) {
#ifdef _WIN32
  server_->getServerState()->getNotifier()->showHealthReportNotification(
      request->title().value(), request->description().value());
#else
  (void)request;
  NOT_IMPLEMENTED();
#endif
}

apache::thrift::ResponseAndServerStream<ChangesSinceResult, ChangedFileResult>
EdenServiceHandler::streamSelectedChangesSince(
    std::unique_ptr<StreamSelectedChangesSinceParams> params) {
  auto requestContext = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_STAT_AND_CANCELLATION(
      DBG3,
      &ThriftStats::streamSelectedChangesSince,
      true,
      requestContext,
      *params->changesParams()->mountPoint());
  auto mountHandle = lookupMount(params->changesParams()->mountPoint().value());
  const auto& fromPosition = *params->changesParams()->fromPosition();
  auto& fetchContext = helper->getFetchContext();

  checkMountGeneration(
      fromPosition, mountHandle.getEdenMount(), "fromPosition"sv);

  auto summed = mountHandle.getJournal().accumulateRange(
      *fromPosition.sequenceNumber() + 1);

  ChangesSinceResult result;
  if (!summed) {
    // No changes, just return the fromPosition and an empty stream.
    result.toPosition() = fromPosition;

    return {
        std::move(result),
        apache::thrift::ServerStream<ChangedFileResult>::createEmpty()};
  }

  if (summed->isTruncated) {
    throw newEdenError(
        EDOM,
        EdenErrorType::JOURNAL_TRUNCATED,
        "Journal entry range has been truncated.");
  }

  auto cancellationSource = std::make_shared<folly::CancellationSource>();
  // Cancelling the request, which EdenFS does for a forced restart, stops the
  // diffs the same way a client disconnect does.
  std::shared_ptr<folly::CancellationCallback> onRequestCancel;
  if (auto requestToken = getCancellationToken(helper->getRequestId())) {
    onRequestCancel = std::make_shared<folly::CancellationCallback>(
        std::move(*requestToken),
        [cancellationSource] { cancellationSource->requestCancellation(); });
  }
  auto [serverStream, publisher] =
      apache::thrift::ServerStream<ChangedFileResult>::createPublisher(
          [cancellationSource] { cancellationSource->requestCancellation(); });
  auto sharedPublisherLock = std::make_shared<
      folly::Synchronized<ThriftStreamPublisherOwner<ChangedFileResult>>>(
      ThriftStreamPublisherOwner{std::move(publisher)});

  RootIdCodec& rootIdCodec = mountHandle.getObjectStore();

  JournalPosition toPosition;
  toPosition.mountGeneration() =
      mountHandle.getEdenMount().getMountGeneration();
  toPosition.sequenceNumber() = summed->toSequence;
  toPosition.snapshotHash() =
      rootIdCodec.renderRootId(summed->snapshotTransitions.back());
  result.toPosition() = toPosition;

  auto caseSensitivity =
      mountHandle.getEdenMount().getCheckoutConfig()->getCaseSensitive();
  auto filter =
      std::make_unique<GlobFilter>(params->globs().value(), caseSensitivity);

  sumUncommittedChanges(
      *summed, *sharedPublisherLock, std::reference_wrapper(*filter));

  if (summed->snapshotTransitions.size() > 1) {
    // create filtered backing store
    std::shared_ptr<FilteredBackingStore> backingStore =
        std::make_shared<FilteredBackingStore>(
            mountHandle.getEdenMountPtr()->getObjectStore()->getBackingStore(),
            std::move(filter),
            server_->getServerState()->getReloadableConfig());
    // pass filtered backing store to object store
    auto objectStore = ObjectStore::create(
        backingStore,
        server_->getTreeCache(),
        server_->getServerState()->getStats().copy(),
        server_->getServerState()->getProcessInfoCache(),
        server_->getServerState()->getEdenFsEventsLogger(),
        server_->getServerState()->getReloadableConfig(),
        caseSensitivity,
        folly::getKeepAliveToken(
            server_->getServerState()->getThreadPool().get()));
    auto callback =
        std::make_shared<StreamingDiffCallback>(sharedPublisherLock);

    std::vector<ImmediateFuture<folly::Unit>> futures;
    // now iterate all commits
    for (auto rootIt = summed->snapshotTransitions.begin();
         std::next(rootIt) != summed->snapshotTransitions.end();
         ++rootIt) {
      const auto from =
          backingStore->createFilteredRootId(rootIt->value(), rootIt->value());
      const auto& toRootId = *(rootIt + 1);
      const auto to = backingStore->createFilteredRootId(
          toRootId.value(), toRootId.value());

      futures.push_back(
          makeNotReadyImmediateFuture().thenValue(
              [from,
               to,
               mountHandle,
               objectStore,
               token = cancellationSource->getToken(),
               fetchContext = fetchContext.copy(),
               callback = callback.get()](auto&&) {
                return diffBetweenRoots(
                    RootId{from},
                    RootId{to},
                    *mountHandle.getEdenMount().getCheckoutConfig(),
                    objectStore,
                    token,
                    fetchContext,
                    callback);
              }));
    }

    folly::futures::detachOn(
        server_->getServerState()->getThreadPool().get(),
        collectAllSafe(std::move(futures))
            .thenTry(
                [mountHandle,
                 sharedPublisherLock,
                 callback = std::move(callback),
                 helper = std::move(helper),
                 onRequestCancel = std::move(onRequestCancel),
                 cancellationSource](
                    folly::Try<std::vector<folly::Unit>>&& result) mutable {
                  if (result.hasException()) {
                    auto sharedPublisher =
                        std::move(*sharedPublisherLock->wlock());
                    std::move(sharedPublisher)
                        .next(newEdenError(std::move(result).exception()));
                  } else if (cancellationSource->getToken()
                                 .isCancellationRequested()) {
                    // A cancelled diff stops early. A client that is still
                    // connected must not take the stream for complete.
                    auto sharedPublisher =
                        std::move(*sharedPublisherLock->wlock());
                    std::move(sharedPublisher)
                        .next(newEdenError(
                            folly::make_exception_wrapper<
                                folly::OperationCancelled>()));
                  }
                })
            .semi());
  }
  return {std::move(result), std::move(serverStream)};
}

void EdenServiceHandler::getFilesChangedSince(
    FileDelta& out,
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<JournalPosition> fromPosition) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountHandle = lookupMount(mountPoint);

  checkMountGeneration(
      *fromPosition, mountHandle.getEdenMount(), "fromPosition"sv);

  // The +1 is because the core merge stops at the item prior to
  // its limitSequence parameter and we want the changes *since*
  // the provided sequence number.
  auto summed = mountHandle.getJournal().accumulateRange(
      *fromPosition->sequenceNumber() + 1);

  // We set the default toPosition to be where we where if summed is null
  out.toPosition()->sequenceNumber() = *fromPosition->sequenceNumber();
  out.toPosition()->snapshotHash() = *fromPosition->snapshotHash();
  out.toPosition()->mountGeneration() =
      mountHandle.getEdenMount().getMountGeneration();

  out.fromPosition() = *out.toPosition();

  if (summed) {
    if (summed->isTruncated) {
      throw newEdenError(
          EDOM,
          EdenErrorType::JOURNAL_TRUNCATED,
          "Journal entry range has been truncated.");
    }

    RootIdCodec& rootIdCodec = mountHandle.getObjectStore();

    out.toPosition()->sequenceNumber() = summed->toSequence;
    out.toPosition()->snapshotHash() =
        rootIdCodec.renderRootId(summed->snapshotTransitions.back());
    out.toPosition()->mountGeneration() =
        mountHandle.getEdenMount().getMountGeneration();

    out.fromPosition()->sequenceNumber() = summed->fromSequence;
    out.fromPosition()->snapshotHash() =
        rootIdCodec.renderRootId(summed->snapshotTransitions.front());
    out.fromPosition()->mountGeneration() =
        *out.toPosition()->mountGeneration();

    for (const auto& entry : summed->changedFilesInOverlay) {
      auto& path = entry.first;
      auto& changeInfo = entry.second;
      if (changeInfo.isNew()) {
        out.createdPaths()->emplace_back(path.asString());
      } else {
        out.changedPaths()->emplace_back(path.asString());
      }
    }

    for (auto& path : summed->uncleanPaths) {
      out.uncleanPaths()->emplace_back(path.asString());
    }

    out.snapshotTransitions()->reserve(summed->snapshotTransitions.size());
    for (auto& id : summed->snapshotTransitions) {
      out.snapshotTransitions()->push_back(rootIdCodec.renderRootId(id));
    }
  }
}

void EdenServiceHandler::setJournalMemoryLimit(
    std::unique_ptr<PathString> mountPoint,
    int64_t limit) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2, *mountPoint);
  auto mountHandle = lookupMount(mountPoint);
  if (limit < 0) {
    throw newEdenError(
        EINVAL,
        EdenErrorType::ARGUMENT_ERROR,
        "memory limit must be non-negative");
  }
  mountHandle.getJournal().setMemoryLimit(static_cast<size_t>(limit));
}

int64_t EdenServiceHandler::getJournalMemoryLimit(
    std::unique_ptr<PathString> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2, *mountPoint);
  auto mountHandle = lookupMount(mountPoint);
  return static_cast<int64_t>(mountHandle.getJournal().getMemoryLimit());
}

void EdenServiceHandler::flushJournal(std::unique_ptr<PathString> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2, *mountPoint);
  auto mountHandle = lookupMount(mountPoint);
  mountHandle.getJournal().flush();
}

void EdenServiceHandler::debugGetRawJournal(
    DebugGetRawJournalResponse& out,
    std::unique_ptr<DebugGetRawJournalParams> params) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2, *params->mountPoint());
  auto mountHandle = lookupMount(params->mountPoint());
  auto mountGeneration =
      static_cast<ssize_t>(mountHandle.getEdenMount().getMountGeneration());

  std::optional<size_t> limitopt = std::nullopt;
  if (auto limit = params->limit()) {
    limitopt = static_cast<size_t>(*limit);
  }

  out.allDeltas() = mountHandle.getJournal().getDebugRawJournalInfo(
      *params->fromSequenceNumber(),
      limitopt,
      mountGeneration,
      mountHandle.getObjectStore());
}

folly::SemiFuture<std::unique_ptr<std::vector<EntryInformationOrError>>>
EdenServiceHandler::semifuture_getEntryInformationImpl(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::vector<std::string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG3, *mountPoint, getSyncTimeout(*sync), toLogArg(*paths));
  auto mountHandle = lookupMount(mountPoint);
  auto& fetchContext = helper->getFetchContext();

  return wrapImmediateFuture(
             std::move(helper),
             waitForPendingWrites(mountHandle.getEdenMount(), *sync)
                 .thenValue([mountHandle,
                             paths = std::move(paths),
                             fetchContext = fetchContext.copy()](auto&&) {
                   return applyToVirtualInode(
                       mountHandle.getRootInode(),
                       *paths,
                       [](const VirtualInode& inode, const RelativePath&) {
                         return inode.getDtype();
                       },
                       mountHandle.getObjectStorePtr(),
                       fetchContext);
                 })
                 .thenValue([](vector<Try<dtype_t>> done) {
                   auto out =
                       std::make_unique<vector<EntryInformationOrError>>();
                   out->reserve(done.size());
                   for (auto& item : done) {
                     EntryInformationOrError result;
                     if (item.hasException()) {
                       result.error() = newEdenError(item.exception());
                     } else {
                       EntryInformation info;
                       info.dtype() = static_cast<Dtype>(item.value());
                       result.info() = info;
                     }
                     out->emplace_back(std::move(result));
                   }
                   return out;
                 }))
      .semi();
}

folly::coro::now_task<std::unique_ptr<std::vector<EntryInformationOrError>>>
EdenServiceHandler::co_getEntryInformationImpl(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::vector<std::string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  XLOG(DBG6, "Using coroutine path for getEntryInformation");
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG3, *mountPoint, getSyncTimeout(*sync), toLogArg(*paths));
  auto& fetchContext = helper->getFetchContext();
  auto mountHandle = lookupMount(mountPoint);
  auto objectStore = mountHandle.getObjectStorePtr();

  co_await co_waitForPendingWrites(mountHandle.getEdenMount(), *sync);

  auto results = co_await co_applyToVirtualInode(
      mountHandle.getRootInode(),
      *paths,
      [](const VirtualInode& inode, const RelativePath&)
          -> folly::coro::now_task<dtype_t> { co_return inode.getDtype(); },
      objectStore,
      fetchContext);

  auto out = std::make_unique<std::vector<EntryInformationOrError>>();
  out->reserve(results.size());

  for (auto& item : results) {
    EntryInformationOrError result;
    if (item.hasException()) {
      result.error() = newEdenError(item.exception());
    } else {
      EntryInformation info;
      info.dtype() = static_cast<Dtype>(item.value());
      result.info() = info;
    }
    out->emplace_back(std::move(result));
  }

  co_return out;
}

folly::SemiFuture<std::unique_ptr<std::vector<EntryInformationOrError>>>
EdenServiceHandler::semifuture_getEntryInformation(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::vector<std::string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  if (server_->getServerState()
          ->getEdenConfig()
          ->enableCoroutinesPhase8.getValue()) {
    // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
    return folly::coro::co_invoke(
               [self = shared_from_this()](
                   std::unique_ptr<std::string> mountPoint,
                   std::unique_ptr<std::vector<std::string>> paths,
                   std::unique_ptr<SyncBehavior> sync)
                   -> folly::coro::Task<
                       std::unique_ptr<std::vector<EntryInformationOrError>>> {
                 co_return co_await self->co_getEntryInformationImpl(
                     std::move(mountPoint), std::move(paths), std::move(sync));
               },
               std::move(mountPoint),
               std::move(paths),
               std::move(sync))
        .semi();
  }
  return semifuture_getEntryInformationImpl(
      std::move(mountPoint), std::move(paths), std::move(sync));
}

folly::SemiFuture<std::unique_ptr<std::vector<FileInformationOrError>>>
EdenServiceHandler::semifuture_getFileInformation(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::vector<std::string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  if (server_->getServerState()
          ->getEdenConfig()
          ->enableCoroutinesPhase9.getValue()) {
    // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
    return folly::coro::co_invoke(
               [self = shared_from_this()](
                   std::unique_ptr<std::string> mountPoint,
                   std::unique_ptr<std::vector<std::string>> paths,
                   std::unique_ptr<SyncBehavior> sync)
                   -> folly::coro::Task<
                       std::unique_ptr<std::vector<FileInformationOrError>>> {
                 co_return co_await self->co_getFileInformationImpl(
                     std::move(mountPoint), std::move(paths), std::move(sync));
               },
               std::move(mountPoint),
               std::move(paths),
               std::move(sync))
        .semi();
  }
  return semifuture_getFileInformationImpl(
      std::move(mountPoint), std::move(paths), std::move(sync));
}

folly::coro::now_task<std::unique_ptr<std::vector<FileInformationOrError>>>
EdenServiceHandler::co_getFileInformationImpl(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::vector<std::string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  XLOG(DBG6, "Using coroutine path for getFileInformation");
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG3, *mountPoint, getSyncTimeout(*sync), toLogArg(*paths));
  auto& fetchContext = helper->getFetchContext();
  auto mountHandle = lookupMount(mountPoint);
  auto objectStore = mountHandle.getObjectStorePtr();
  auto lastCheckoutTime =
      mountHandle.getEdenMount().getLastCheckoutTime().toTimespec();

  co_await co_waitForPendingWrites(mountHandle.getEdenMount(), *sync);

  auto results = co_await co_applyToVirtualInode(
      mountHandle.getRootInode(),
      *paths,
      [lastCheckoutTime, objectStore, fetchContext = fetchContext.copy()](
          VirtualInode inode,
          RelativePath) -> folly::coro::now_task<FileInformationOrError> {
        auto st =
            co_await inode.co_stat(lastCheckoutTime, objectStore, fetchContext);
        FileInformation info;
        info.size() = st.st_size;
        auto ts = stMtime(st);
        info.mtime()->seconds() = ts.tv_sec;
        info.mtime()->nanoSeconds() = ts.tv_nsec;
        info.mode() = st.st_mode;

        FileInformationOrError result;
        result.info() = info;
        co_return result;
      },
      objectStore,
      fetchContext);

  auto out = std::make_unique<std::vector<FileInformationOrError>>();
  out->reserve(results.size());

  for (auto& item : results) {
    if (item.hasException()) {
      FileInformationOrError result;
      result.error() = newEdenError(item.exception());
      out->emplace_back(std::move(result));
    } else {
      out->emplace_back(item.value());
    }
  }

  co_return out;
}

folly::SemiFuture<std::unique_ptr<std::vector<FileInformationOrError>>>
EdenServiceHandler::semifuture_getFileInformationImpl(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::vector<std::string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG3, *mountPoint, getSyncTimeout(*sync), toLogArg(*paths));
  auto mountHandle = lookupMount(mountPoint);
  auto& fetchContext = helper->getFetchContext();
  auto lastCheckoutTime =
      mountHandle.getEdenMount().getLastCheckoutTime().toTimespec();

  return wrapImmediateFuture(
             std::move(helper),
             waitForPendingWrites(mountHandle.getEdenMount(), *sync)
                 .thenValue([mountHandle,
                             paths = std::move(paths),
                             lastCheckoutTime,
                             fetchContext = fetchContext.copy()](auto&&) {
                   return applyToVirtualInode(
                       mountHandle.getRootInode(),
                       *paths,
                       [mountHandle,
                        lastCheckoutTime,
                        fetchContext = fetchContext.copy()](
                           const VirtualInode& inode, const RelativePath&) {
                         return inode
                             .stat(
                                 lastCheckoutTime,
                                 mountHandle.getObjectStorePtr(),
                                 fetchContext)
                             .thenValue([](struct stat st) {
                               FileInformation info;
                               info.size() = st.st_size;
                               auto ts = stMtime(st);
                               info.mtime()->seconds() = ts.tv_sec;
                               info.mtime()->nanoSeconds() = ts.tv_nsec;
                               info.mode() = st.st_mode;

                               FileInformationOrError result;
                               result.info() = info;

                               return result;
                             })
                             .semi();
                       },
                       mountHandle.getObjectStorePtr(),
                       fetchContext);
                 })
                 .thenValue([](vector<Try<FileInformationOrError>>&& done) {
                   auto out =
                       std::make_unique<vector<FileInformationOrError>>();
                   out->reserve(done.size());
                   for (auto& item : done) {
                     if (item.hasException()) {
                       FileInformationOrError result;
                       result.error() = newEdenError(item.exception());
                       out->emplace_back(std::move(result));
                     } else {
                       out->emplace_back(item.value());
                     }
                   }
                   return out;
                 }))
      .ensure([mountHandle] {})
      .semi();
}

namespace {
SourceControlType entryTypeToThriftType(std::optional<TreeEntryType> type) {
  if (!type.has_value()) {
    return SourceControlType::UNKNOWN;
  }
  switch (type.value()) {
    case TreeEntryType::TREE:
      return SourceControlType::TREE;
    case TreeEntryType::REGULAR_FILE:
      return SourceControlType::REGULAR_FILE;
    case TreeEntryType::EXECUTABLE_FILE:
      return SourceControlType::EXECUTABLE_FILE;
    case TreeEntryType::SYMLINK:
      return SourceControlType::SYMLINK;
    default:
      throw std::system_error(EINVAL, std::generic_category());
  }
}

template <typename SerializedT, typename T>
bool fillErrorRef(
    SerializedT& result,
    std::optional<folly::Try<T>> rawResult,
    folly::StringPiece path,
    folly::StringPiece attributeName) {
  if (!rawResult.has_value()) {
    result.error_ref() = newEdenError(
        ENOENT,
        EdenErrorType::ATTRIBUTE_UNAVAILABLE,
        fmt::format(
            "{}: {} requested, but no {} available",
            path,
            attributeName,
            attributeName));
    return true;
  }
  if (rawResult.value().hasException()) {
    result.error_ref() = newEdenError(rawResult.value().exception());
    return true;
  }
  return false;
}

FileAttributeDataOrErrorV2 serializeEntryAttributes(
    ObjectStore& objectStore,
    folly::StringPiece entryPath,
    const folly::Try<EntryAttributes>& attributes,
    EntryAttributeFlags requestedAttributes) {
  FileAttributeDataOrErrorV2 fileResult;

  if (attributes.hasException()) {
    fileResult.error() = newEdenError(attributes.exception());
    return fileResult;
  }

  FileAttributeDataV2 fileData;
  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_SHA1)) {
    Sha1OrError sha1;
    if (!fillErrorRef(sha1, attributes->sha1, entryPath, "sha1")) {
      sha1.sha1() = thriftHash20(attributes->sha1.value().value());
    }
    fileData.sha1() = std::move(sha1);
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_BLAKE3)) {
    Blake3OrError blake3;
    if (!fillErrorRef(blake3, attributes->blake3, entryPath, "blake3")) {
      blake3.blake3() = thriftHash32(attributes->blake3.value().value());
    }
    fileData.blake3() = std::move(blake3);
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_SIZE)) {
    SizeOrError size;
    if (!fillErrorRef(size, attributes->size, entryPath, "size")) {
      size.size() = attributes->size.value().value();
    }
    fileData.size() = std::move(size);
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_SOURCE_CONTROL_TYPE)) {
    SourceControlTypeOrError type;
    if (!fillErrorRef(type, attributes->type, entryPath, "type")) {
      type.sourceControlType() =
          entryTypeToThriftType(attributes->type.value().value());
    }
    fileData.sourceControlType() = std::move(type);
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_OBJECT_ID)) {
    ObjectIdOrError objectId;
    if (!fillErrorRef(objectId, attributes->objectId, entryPath, "objectid")) {
      const std::optional<ObjectId>& oid = attributes->objectId.value().value();
      if (oid) {
        objectId.objectId() = objectStore.renderObjectId(*oid);
      }
    }
    fileData.objectId() = std::move(objectId);
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_DIGEST_SIZE)) {
    DigestSizeOrError digestSize;
    if (!fillErrorRef(
            digestSize, attributes->digestSize, entryPath, "digestsize")) {
      digestSize.digestSize() = attributes->digestSize.value().value();
    }
    fileData.digestSize() = std::move(digestSize);
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_DIGEST_HASH)) {
    DigestHashOrError digestHash;
    if (!fillErrorRef(
            digestHash, attributes->digestHash, entryPath, "digesthash")) {
      digestHash.digestHash() =
          thriftHash32(attributes->digestHash.value().value());
    }
    fileData.digestHash() = std::move(digestHash);
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_MTIME)) {
    MtimeOrError mtime;
    if (!fillErrorRef(mtime, attributes->mtime, entryPath, "mtime")) {
      mtime.mtime() = thriftTimeSpec(attributes->mtime.value().value());
    }
    fileData.mtime() = std::move(mtime);
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_MODE)) {
    ModeOrError mode;
    if (!fillErrorRef(mode, attributes->mode, entryPath, "mode")) {
      mode.mode() = attributes->mode.value().value();
    }
    fileData.mode() = std::move(mode);
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_UNDER_ACL)) {
    UnderAclOrError underAcl;
    if (!fillErrorRef(underAcl, attributes->underAcl, entryPath, "underAcl")) {
      underAcl.underAcl() = attributes->underAcl.value().value();
    }
    fileData.underAcl() = std::move(underAcl);
  }

  if (requestedAttributes.contains(ENTRY_ATTRIBUTE_ACLs)) {
    AclInfoOrError aclInfoResult;
    if (!fillErrorRef(
            aclInfoResult, attributes->aclInfo, entryPath, "aclInfo")) {
      const auto& info = attributes->aclInfo.value().value();
      AclInfo thriftInfo;
      thriftInfo.underAcl() = info.underAcl;
      std::vector<AclEntry> thriftAcls;
      for (const auto& entry : info.acls) {
        AclEntry thriftEntry;
        thriftEntry.restrictionRoot() = entry.restrictionRoot;
        thriftEntry.repoRegionAcl() = entry.repoRegionAcl;
        if (entry.requestAcl.has_value()) {
          thriftEntry.requestAcl() = entry.requestAcl.value();
        }
        thriftAcls.push_back(std::move(thriftEntry));
      }
      thriftInfo.acls() = std::move(thriftAcls);
      aclInfoResult.aclInfo() = std::move(thriftInfo);
    }
    fileData.aclInfo() = std::move(aclInfoResult);
  }

  fileResult.fileAttributeData() = fileData;
  return fileResult;
}

DirListAttributeDataOrError serializeEntryAttributes(
    ObjectStore& objectStore,
    const folly::Try<
        std::vector<std::pair<PathComponent, folly::Try<EntryAttributes>>>>&
        entries,
    EntryAttributeFlags requestedAttributes) {
  DirListAttributeDataOrError result;
  if (entries.hasException()) {
    result.error() = newEdenError(*entries.exception().get_exception());
    return result;
  }
  std::map<std::string, FileAttributeDataOrErrorV2> thriftEntryResult;
  for (auto& [path_component, attributes] : entries.value()) {
    thriftEntryResult.emplace(
        path_component.asString(),
        serializeEntryAttributes(
            objectStore,
            path_component.piece().view(),
            attributes,
            requestedAttributes));
  }

  result.dirListAttributeData() = std::move(thriftEntryResult);
  return result;
}

folly::coro::now_task<DirListAttributeDataOrError> co_getAllEntryAttributes(
    EntryAttributeFlags requestedAttributes,
    const EdenMount& edenMount,
    std::string path,
    const ObjectFetchContextPtr& fetchContext) {
  fetchContext->throwIfCanceled();
  // Capture lastCheckoutTime before any co_await so synthetic tree mtimes
  // are stable across concurrent checkout.
  auto lastCheckoutTime = edenMount.getLastCheckoutTime().toTimespec();

  // RelativePath construction throws for malformed input (e.g. an absolute
  // path). That failure must land in this path's slot of the result rather
  // than escape the per-path task, which would fail the whole readdir batch.
  std::optional<RelativePath> relativePath;
  try {
    relativePath = RelativePath{path};
  } catch (const std::exception& e) {
    DirListAttributeDataOrError result;
    result.error() =
        newEdenError(EINVAL, EdenErrorType::ARGUMENT_ERROR, e.what());
    co_return result;
  }

  auto viTry = co_await folly::coro::co_awaitTry(
      edenMount.co_getVirtualInode(*relativePath, fetchContext));
  if (viTry.hasException()) {
    DirListAttributeDataOrError result;
    result.error() = newEdenError(viTry.exception());
    co_return result;
  }
  auto& virtualInode = viTry.value();

  if (!virtualInode.isDirectory()) {
    DirListAttributeDataOrError result;
    result.error() = newEdenError(
        EINVAL,
        EdenErrorType::ARGUMENT_ERROR,
        fmt::format("{}: path must be a directory", path));
    co_return result;
  }

  auto entriesTry =
      co_await folly::coro::co_awaitTry(virtualInode.co_getChildrenAttributes(
          requestedAttributes,
          std::move(*relativePath),
          edenMount.getObjectStore(),
          lastCheckoutTime,
          fetchContext));
  if (entriesTry.hasException()) {
    DirListAttributeDataOrError result;
    result.error() = newEdenError(entriesTry.exception());
    co_return result;
  }

  co_return serializeEntryAttributes(
      *edenMount.getObjectStore(),
      folly::Try<
          std::vector<std::pair<PathComponent, folly::Try<EntryAttributes>>>>{
          std::move(entriesTry.value())},
      requestedAttributes);
}

} // namespace

folly::coro::Task<std::unique_ptr<ReaddirResult>>
EdenServiceHandler::co_readdir(std::unique_ptr<ReaddirParams> params) {
  auto mountHandle = lookupMount(params->mountPoint());
  auto paths = *params->directoryPaths();
  auto requestContext = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      DBG3,
      true,
      requestContext,
      *params->mountPoint(),
      getSyncTimeout(*params->sync()),
      toLogArg(paths));
  auto& fetchContext = helper->getFetchContext();
  auto requestedAttributes = EntryAttributeFlags::raw(
      static_cast<std::underlying_type_t<FileAttributes>>(
          *params->requestedAttributes()));

  co_await co_waitForPendingWrites(mountHandle.getEdenMount(), *params->sync());

  std::vector<folly::coro::Task<DirListAttributeDataOrError>> tasks;
  tasks.reserve(paths.size());
  for (auto& path : paths) {
    tasks.emplace_back(
        // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
        folly::coro::co_invoke(
            [](EntryAttributeFlags reqAttrs,
               EdenMountHandle handle,
               std::string p,
               ObjectFetchContextPtr ctx)
                -> folly::coro::Task<DirListAttributeDataOrError> {
              co_return co_await co_getAllEntryAttributes(
                  reqAttrs, handle.getEdenMount(), std::move(p), ctx);
            },
            requestedAttributes,
            mountHandle,
            std::move(path),
            fetchContext.copy()));
  }

  auto results = co_await folly::coro::collectAllRange(std::move(tasks));
  // A cancellation that lands while the directories are being read shows up
  // as per-entry errors; fail the request instead of returning them.
  fetchContext->throwIfCanceled();

  auto res = std::make_unique<ReaddirResult>();
  res->dirLists() = std::move(results);
  co_return res;
}

ImmediateFuture<std::vector<folly::Try<EntryAttributes>>>
EdenServiceHandler::getEntryAttributes(
    const EdenMount& edenMount,
    const std::vector<std::string>& paths,
    EntryAttributeFlags reqBitmask,
    AttributesRequestScope reqScope,
    SyncBehavior sync,
    const ObjectFetchContextPtr& fetchContext) {
  if (server_->getServerState()
          ->getEdenConfig()
          ->enableCoroutinesPhase10.getValue()) {
    return ImmediateFuture{
        // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
        folly::coro::co_invoke(
            [self = shared_from_this()](
                std::shared_ptr<const EdenMount> mountPtr,
                std::vector<std::string> paths,
                EntryAttributeFlags reqBitmask,
                AttributesRequestScope reqScope,
                SyncBehavior sync,
                ObjectFetchContextPtr fetchContext)
                -> folly::coro::Task<std::vector<folly::Try<EntryAttributes>>> {
              co_return co_await self->co_getEntryAttributes(
                  *mountPtr, paths, reqBitmask, reqScope, sync, fetchContext);
            },
            edenMount.shared_from_this(),
            paths,
            reqBitmask,
            reqScope,
            sync,
            fetchContext.copy())
            .semi()};
  }
  return getEntryAttributesImpl(
      edenMount, paths, reqBitmask, reqScope, sync, fetchContext);
}

folly::coro::now_task<std::vector<folly::Try<EntryAttributes>>>
EdenServiceHandler::co_getEntryAttributes(
    const EdenMount& edenMount,
    const std::vector<std::string>& paths,
    EntryAttributeFlags reqBitmask,
    AttributesRequestScope reqScope,
    SyncBehavior sync,
    const ObjectFetchContextPtr& fetchContext) {
  co_await co_waitForPendingWrites(edenMount, sync);

  auto localReqBitmask = reqBitmask;
  if (reqBitmask.contains(ENTRY_ATTRIBUTE_ACLs)) {
    localReqBitmask |= ENTRY_ATTRIBUTE_UNDER_ACL;
  }

  std::vector<folly::coro::Task<EntryAttributes>> tasks;
  tasks.reserve(paths.size());
  for (const auto& path : paths) {
    tasks.emplace_back(
        // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
        folly::coro::co_invoke(
            [self = shared_from_this()](
                std::shared_ptr<const EdenMount> mountPtr,
                EntryAttributeFlags reqBitmask,
                AttributesRequestScope reqScope,
                std::string pathStr,
                ObjectFetchContextPtr fetchContext)
                -> folly::coro::Task<EntryAttributes> {
              co_return co_await self->co_getEntryAttributesForPath(
                  *mountPtr,
                  reqBitmask,
                  reqScope,
                  std::string_view{pathStr},
                  fetchContext);
            },
            edenMount.shared_from_this(),
            localReqBitmask,
            reqScope,
            path,
            fetchContext.copy()));
  }

  auto allRes = co_await folly::coro::collectAllTryRange(std::move(tasks));
  // Each path that was cancelled holds an error in its slot. The request as a
  // whole must fail rather than hand back a response that looks like per-path
  // failures.
  fetchContext->throwIfCanceled();

  if (!reqBitmask.containsAnyOf(kAclPathAttributes)) {
    co_return allRes;
  }

  co_return co_await co_fetchPathAclLookupsForAttributes(
      edenMount.getObjectStore(),
      edenMount.getCheckedOutRootId(),
      fetchContext.copy(),
      std::move(allRes),
      paths,
      reqBitmask);
}

ImmediateFuture<std::vector<folly::Try<EntryAttributes>>>
EdenServiceHandler::getEntryAttributesImpl(
    const EdenMount& edenMount,
    const std::vector<std::string>& paths,
    EntryAttributeFlags reqBitmask,
    AttributesRequestScope reqScope,
    SyncBehavior sync,
    const ObjectFetchContextPtr& fetchContext) {
  auto localReqBitmask = reqBitmask;
  if (reqBitmask.contains(ENTRY_ATTRIBUTE_ACLs)) {
    localReqBitmask |= ENTRY_ATTRIBUTE_UNDER_ACL;
  }
  auto waitFuture = waitForPendingWrites(edenMount, sync);
  return std::move(waitFuture)
      .thenValue([this,
                  &edenMount,
                  paths = paths,
                  fetchContext = fetchContext.copy(),
                  reqBitmask,
                  localReqBitmask,
                  reqScope](auto&&) mutable {
        vector<ImmediateFuture<EntryAttributes>> futures;
        futures.reserve(paths.size());
        for (const auto& path : paths) {
          futures.emplace_back(getEntryAttributesForPath(
              edenMount, localReqBitmask, reqScope, path, fetchContext));
        }

        auto objectStore = edenMount.getObjectStore();
        auto checkedOutRootId = edenMount.getCheckedOutRootId();
        return facebook::eden::collectAll(std::move(futures))
            .thenValue(
                [objectStore = std::move(objectStore),
                 paths = std::move(paths),
                 fetchContext = fetchContext.copy(),
                 reqBitmask,
                 checkedOutRootId = std::move(checkedOutRootId)](
                    std::vector<folly::Try<EntryAttributes>> allRes) mutable
                    -> ImmediateFuture<
                        std::vector<folly::Try<EntryAttributes>>> {
                  // A cancelled request fails as a whole; see
                  // co_getEntryAttributes.
                  fetchContext->throwIfCanceled();
                  if (!reqBitmask.containsAnyOf(kAclPathAttributes)) {
                    return allRes;
                  }

                  return ImmediateFuture<
                      std::vector<folly::Try<EntryAttributes>>>{
                      co_fetchPathAclLookupsForAttributes(
                          objectStore,
                          std::move(checkedOutRootId),
                          fetchContext.copy(),
                          std::move(allRes),
                          std::move(paths),
                          reqBitmask)
                          .semi()};
                });
      });
}

namespace {
bool dtypeMatchesRequestScope(
    const VirtualInode& inode,
    AttributesRequestScope reqScope) {
  if (reqScope == AttributesRequestScope::TREES_AND_FILES) {
    return true;
  }

  if (inode.isDirectory()) {
    return reqScope == AttributesRequestScope::TREES;
  } else {
    return reqScope == AttributesRequestScope::FILES;
  }
}
} // namespace

ImmediateFuture<EntryAttributes> EdenServiceHandler::getEntryAttributesForPath(
    const EdenMount& edenMount,
    EntryAttributeFlags reqBitmask,
    AttributesRequestScope reqScope,
    std::string_view path,
    const ObjectFetchContextPtr& fetchContext) {
  if (server_->getServerState()
          ->getEdenConfig()
          ->enableCoroutinesPhase10.getValue()) {
    return ImmediateFuture{
        // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
        folly::coro::co_invoke(
            [self = shared_from_this()](
                std::shared_ptr<const EdenMount> mountPtr,
                EntryAttributeFlags reqBitmask,
                AttributesRequestScope reqScope,
                std::string pathStr,
                ObjectFetchContextPtr fetchContext)
                -> folly::coro::Task<EntryAttributes> {
              co_return co_await self->co_getEntryAttributesForPath(
                  *mountPtr,
                  reqBitmask,
                  reqScope,
                  std::string_view{pathStr},
                  fetchContext);
            },
            edenMount.shared_from_this(),
            reqBitmask,
            reqScope,
            std::string{path},
            fetchContext.copy())
            .semi()};
  }
  return getEntryAttributesForPathImpl(
      edenMount, reqBitmask, reqScope, path, fetchContext);
}

folly::coro::now_task<EntryAttributes>
EdenServiceHandler::co_getEntryAttributesForPath(
    const EdenMount& edenMount,
    EntryAttributeFlags reqBitmask,
    AttributesRequestScope reqScope,
    std::string_view path,
    const ObjectFetchContextPtr& fetchContext) {
  fetchContext->throwIfCanceled();
  if (path.empty()) {
    co_yield folly::coro::co_error(newEdenError(
        EINVAL,
        EdenErrorType::ARGUMENT_ERROR,
        "path cannot be the empty string"));
  }

  std::optional<RelativePath> relativePath;
  std::string parseError;
  try {
    relativePath = RelativePath{path};
  } catch (const std::exception& e) {
    parseError = e.what();
  }
  if (!relativePath) {
    co_yield folly::coro::co_error(
        newEdenError(EINVAL, EdenErrorType::ARGUMENT_ERROR, parseError));
  }

  auto virtualInode =
      co_await edenMount.co_getVirtualInode(*relativePath, fetchContext);
  if (!dtypeMatchesRequestScope(virtualInode, reqScope)) {
    co_yield folly::coro::co_error(PathError(
        reqScope == AttributesRequestScope::TREES ? ENOTDIR : EISDIR,
        *relativePath));
  }
  co_return co_await virtualInode.co_getEntryAttributes(
      reqBitmask,
      *relativePath,
      edenMount.getObjectStore(),
      edenMount.getLastCheckoutTime().toTimespec(),
      fetchContext);
}

ImmediateFuture<EntryAttributes>
EdenServiceHandler::getEntryAttributesForPathImpl(
    const EdenMount& edenMount,
    EntryAttributeFlags reqBitmask,
    AttributesRequestScope reqScope,
    std::string_view path,
    const ObjectFetchContextPtr& fetchContext) {
  if (path.empty()) {
    return ImmediateFuture<EntryAttributes>(newEdenError(
        EINVAL,
        EdenErrorType::ARGUMENT_ERROR,
        "path cannot be the empty string"));
  }

  try {
    RelativePathPiece relativePath{path};

    return edenMount.getVirtualInode(relativePath, fetchContext)
        .thenValue([&edenMount,
                    reqBitmask,
                    reqScope,
                    relativePath = relativePath.copy(),
                    fetchContext =
                        fetchContext.copy()](const VirtualInode& virtualInode) {
          if (dtypeMatchesRequestScope(virtualInode, reqScope)) {
            return virtualInode.getEntryAttributes(
                reqBitmask,
                relativePath,
                edenMount.getObjectStore(),
                edenMount.getLastCheckoutTime().toTimespec(),
                fetchContext);
          }
          return makeImmediateFuture<EntryAttributes>(PathError(
              reqScope == AttributesRequestScope::TREES ? ENOTDIR : EISDIR,
              relativePath));
        });
  } catch (const std::exception& e) {
    return ImmediateFuture<EntryAttributes>(
        newEdenError(EINVAL, EdenErrorType::ARGUMENT_ERROR, e.what()));
  }
}

folly::SemiFuture<std::unique_ptr<GetAttributesFromFilesResultV2>>
EdenServiceHandler::semifuture_getAttributesFromFilesV2(
    std::unique_ptr<GetAttributesFromFilesParams> params) {
  if (server_->getServerState()
          ->getEdenConfig()
          ->enableCoroutinesPhase10.getValue()) {
    // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
    return folly::coro::co_invoke(
               [self = shared_from_this()](
                   std::unique_ptr<GetAttributesFromFilesParams> p)
                   -> folly::coro::Task<
                       std::unique_ptr<GetAttributesFromFilesResultV2>> {
                 co_return co_await self->co_getAttributesFromFilesV2Impl(
                     std::move(p));
               },
               std::move(params))
        .semi();
  }
  return semifuture_getAttributesFromFilesV2Impl(std::move(params));
}

folly::coro::now_task<std::unique_ptr<GetAttributesFromFilesResultV2>>
EdenServiceHandler::co_getAttributesFromFilesV2Impl(
    std::unique_ptr<GetAttributesFromFilesParams> params) {
  XLOG(DBG6, "Using coroutine path for getAttributesFromFilesV2");
  auto mountHandle = lookupMount(params->mountPoint());
  auto reqScope =
      params->scope().value_or(AttributesRequestScope::TREES_AND_FILES);
  auto reqBitmask = EntryAttributeFlags::raw(
      static_cast<std::underlying_type_t<FileAttributes>>(
          *params->requestedAttributes()));
  std::vector<std::string>& paths = params->paths().value();
  auto requestContext = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      DBG3,
      true,
      requestContext,
      *params->mountPoint(),
      getSyncTimeout(*params->sync()),
      toLogArg(paths));
  auto& fetchContext = helper->getFetchContext();

  auto allRes = co_await co_getEntryAttributes(
      mountHandle.getEdenMount(),
      paths,
      reqBitmask,
      reqScope,
      *params->sync(),
      fetchContext);

  auto res = std::make_unique<GetAttributesFromFilesResultV2>();
  res->res()->reserve(allRes.size());
  size_t index = 0;
  for (const auto& tryAttributes : allRes) {
    res->res()->emplace_back(serializeEntryAttributes(
        mountHandle.getObjectStore(),
        basename(paths.at(index)),
        tryAttributes,
        reqBitmask));
    ++index;
  }
  co_return res;
}

folly::SemiFuture<std::unique_ptr<GetAttributesFromFilesResultV2>>
EdenServiceHandler::semifuture_getAttributesFromFilesV2Impl(
    std::unique_ptr<GetAttributesFromFilesParams> params) {
  auto mountHandle = lookupMount(params->mountPoint());
  auto reqScope =
      params->scope().value_or(AttributesRequestScope::TREES_AND_FILES);
  auto reqBitmask = EntryAttributeFlags::raw(
      static_cast<std::underlying_type_t<FileAttributes>>(
          *params->requestedAttributes()));
  std::vector<std::string>& paths = params->paths().value();
  auto requestContext = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      DBG3,
      true,
      requestContext,
      *params->mountPoint(),
      getSyncTimeout(*params->sync()),
      toLogArg(paths));
  auto& fetchContext = helper->getFetchContext();

  auto entryAttributesFuture = getEntryAttributes(
      mountHandle.getEdenMount(),
      paths,
      reqBitmask,
      reqScope,
      *params->sync(),
      fetchContext);

  return wrapImmediateFuture(
             std::move(helper),
             std::move(entryAttributesFuture)
                 .thenValue(
                     [reqBitmask, mountHandle, &paths](
                         std::vector<folly::Try<EntryAttributes>>&& allRes) {
                       auto res =
                           std::make_unique<GetAttributesFromFilesResultV2>();
                       res->res()->reserve(allRes.size());
                       size_t index = 0;
                       for (const auto& tryAttributes : allRes) {
                         res->res()->emplace_back(serializeEntryAttributes(
                             mountHandle.getObjectStore(),
                             basename(paths.at(index)),
                             tryAttributes,
                             reqBitmask));
                         ++index;
                       }
                       return res;
                     }))
      .ensure([mountHandle, params = std::move(params)]() {})
      .semi();
}

folly::SemiFuture<std::unique_ptr<SetPathObjectIdResult>>
EdenServiceHandler::semifuture_setPathObjectId(
    std::unique_ptr<SetPathObjectIdParams> params) {
#ifndef _WIN32
  auto mountHandle = lookupMount(params->mountPoint());
  std::vector<SetPathObjectIdObjectAndPath> objects;
  std::vector<std::string> object_strings;
  auto objectSize =
      apache::thrift::is_non_optional_field_set_manually_or_by_serializer(
          params->objects())
      ? params->objects()->size() + 1
      : 1;
  objects.reserve(objectSize);
  object_strings.reserve(objectSize);

  // TODO deprecate non-batch fields once all clients moves to the batch
  // fields. Rust clients might set to default and is_set() would return false
  // negative
  if (apache::thrift::is_non_optional_field_set_manually_or_by_serializer(
          params->objectId()) &&
      !params->objectId()->empty()) {
    SetPathObjectIdObjectAndPath objectAndPath;
    objectAndPath.path = RelativePath{*params->path()};
    objectAndPath.id =
        mountHandle.getObjectStore().parseObjectId(*params->objectId());
    objectAndPath.type = *params->type();
    object_strings.emplace_back(objectAndPath.toString());
    objects.emplace_back(std::move(objectAndPath));
  }

  for (auto& object : *params->objects()) {
    SetPathObjectIdObjectAndPath objectAndPath;
    objectAndPath.path = RelativePath{*object.path()};
    objectAndPath.id =
        mountHandle.getObjectStore().parseObjectId(object.objectId().value());
    objectAndPath.type = *object.type();
    object_strings.emplace_back(objectAndPath.toString());
    objects.emplace_back(std::move(objectAndPath));
  }

  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG1, *params->mountPoint(), toLogArg(object_strings));

  if (auto requestInfo = params->requestInfo()) {
    helper->getThriftFetchContext().updateRequestInfo(*requestInfo);
  }
  ObjectFetchContextPtr context = helper->getFetchContext().copy();
  return wrapImmediateFuture(
             std::move(helper),
             mountHandle.getEdenMount()
                 .setPathsToObjectIds(
                     std::move(objects), (*params->mode()), context)
                 .thenValue([](auto&& resultAndTimes) {
                   return std::make_unique<SetPathObjectIdResult>(
                       std::move(resultAndTimes.result));
                 }))
      .ensure([mountHandle] {})
      .semi();
#else
  (void)params;
  NOT_IMPLEMENTED();
#endif
}

folly::SemiFuture<folly::Unit> EdenServiceHandler::semifuture_removeRecursively(
    std::unique_ptr<RemoveRecursivelyParams> params) {
  auto mountPoint = *params->mountPoint();
  auto repoPath = *params->path();

  auto helper = INSTRUMENT_THRIFT_CALL(DBG2, mountPoint, repoPath);
  auto mountHandle = lookupMount(mountPoint);

  auto relativePath = RelativePath{repoPath};
  auto& fetchContext = helper->getFetchContext();

  return wrapImmediateFuture(
             std::move(helper),
             waitForPendingWrites(mountHandle.getEdenMount(), *params->sync())
                 .thenValue([mountHandle,
                             relativePath,
                             fetchContext = fetchContext.copy()](folly::Unit) {
                   return mountHandle.getEdenMount().getInodeSlow(
                       relativePath, fetchContext);
                 })
                 .thenValue([relativePath, fetchContext = fetchContext.copy()](
                                InodePtr inode) {
                   return inode->getParentRacy()->removeRecursively(
                       relativePath.basename(),
                       InvalidationRequired::Yes,
                       fetchContext);
                 }))
      .ensure([mountHandle] {})
      .semi();
}

namespace {
template <typename ReturnType>
ImmediateFuture<std::unique_ptr<ReturnType>> detachIfBackgrounded(
    ImmediateFuture<std::unique_ptr<ReturnType>> future,
    const std::shared_ptr<ServerState>& serverState,
    bool background) {
  if (!background) {
    return future;
  } else {
    folly::futures::detachOn(
        serverState->getThreadPool().get(), std::move(future).semi());
    return ImmediateFuture<std::unique_ptr<ReturnType>>(
        std::make_unique<ReturnType>());
  }
}

template <typename ReturnType>
folly::SemiFuture<std::unique_ptr<ReturnType>> serialDetachIfBackgrounded(
    ImmediateFuture<std::unique_ptr<ReturnType>> future,
    EdenServer* const server,
    bool background) {
  // If we're already using serial execution across the board, just do a
  // normal detachIfBackgrounded
  if (server->usingThriftSerialExecution()) {
    return detachIfBackgrounded(
               std::move(future), server->getServerState(), background)
        .semi();
  }

  folly::Executor::KeepAlive<> serial = folly::SerialExecutor::create(
      server->getServer()->getThreadManager().get());

  if (background) {
    folly::futures::detachOn(serial, std::move(future).semi());
    future = ImmediateFuture<std::unique_ptr<ReturnType>>(
        std::make_unique<ReturnType>());
  }

  if (future.isReady()) {
    return std::move(future).semi();
  }

  return std::move(future).semi().via(serial);
}

void maybeLogExpensiveGlob(
    const std::vector<std::string>& globs,
    const folly::StringPiece searchRoot,
    const ThriftGlobImpl& globber,
    const ObjectFetchContextPtr& context,
    const std::shared_ptr<ServerState>& serverState) {
  bool shouldLogExpensiveGlob = false;

  if (searchRoot.empty()) {
    for (const auto& glob : globs) {
      if (string_view{glob}.starts_with("**")) {
        shouldLogExpensiveGlob = true;
      }
    }
  }

  if (shouldLogExpensiveGlob) {
    auto logString = globber.logString(globs);
    std::string client_cmdline = getClientCmdline(serverState, context);
    XLOGF(
        WARN,
        "EdenFS asked to evaluate expensive glob by caller {} : {}",
        client_cmdline,
        logString);
    serverState->getEdenFsEventsLogger()->logEvent(
        StarGlob{std::move(logString), std::move(client_cmdline)});
  }
}
} // namespace

#ifndef _WIN32
namespace {
ImmediateFuture<folly::Unit> ensureMaterializedImpl(
    std::shared_ptr<EdenMount> edenMount,
    const std::vector<std::string>& repoPaths,
    std::unique_ptr<ThriftRequestScope> helper,
    bool followSymlink) {
  std::vector<ImmediateFuture<folly::Unit>> futures;
  futures.reserve(repoPaths.size());

  auto& fetchContext = helper->getFetchContext();

  for (auto& path : repoPaths) {
    futures.emplace_back(
        makeNotReadyImmediateFuture()
            .thenValue([edenMount = edenMount.get(),
                        path = RelativePath{path},
                        fetchContext = fetchContext.copy()](auto&&) {
              return edenMount->getInodeSlow(path, fetchContext);
            })
            .thenValue([fetchContext = fetchContext.copy(),
                        followSymlink](InodePtr inode) {
              return inode->ensureMaterialized(fetchContext, followSymlink)
                  .ensure([inode]() {});
            }));
  }

  return wrapImmediateFuture(
      std::move(helper), collectAll(std::move(futures)).unit());
}
} // namespace
#endif

folly::SemiFuture<folly::Unit>
EdenServiceHandler::semifuture_ensureMaterialized(
    std::unique_ptr<EnsureMaterializedParams> params) {
#ifndef _WIN32
  auto mountPoint = *params->mountPoint();
  auto helper =
      INSTRUMENT_THRIFT_CALL(DBG4, mountPoint, toLogArg(*params->paths()));

  auto mountHandle = lookupMount(mountPoint);
  // The background mode is not fully running on background, instead, it will
  // start to load inodes in a blocking way, and then collect unready
  // materialization process then throws to the background. This is most
  // efficient way for the local execution of virtualized buck-out as avoid
  // cache exchange by materializing smaller random reads, and not prevent
  // execution starting by read large files on the background.
  bool background = *params->background();
  if (background) {
    helper->markBackground();
  }

  auto waitForPendingWritesFuture =
      waitForPendingWrites(mountHandle.getEdenMount(), *params->sync());
  auto ensureMaterializedFuture =
      std::move(waitForPendingWritesFuture)
          .thenValue([params = std::move(params),
                      mountHandle,
                      helper = std::move(helper)](auto&&) mutable {
            return ensureMaterializedImpl(
                mountHandle.getEdenMountPtr(),
                (*params->paths()),
                std::move(helper),
                (*params->followSymlink()));
          })
          .ensure([mountHandle] {})
          .semi();

  if (background) {
    folly::futures::detachOn(
        server_->getServerState()->getThreadPool().get(),
        std::move(ensureMaterializedFuture));
    return folly::unit;
  } else {
    return ensureMaterializedFuture;
  }
#else
  (void)params;
  NOT_IMPLEMENTED();
#endif
}

folly::SemiFuture<std::unique_ptr<Glob>>
EdenServiceHandler::semifuture_globFiles(std::unique_ptr<GlobParams> params) {
  auto isBackground = *params->background();
  // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
  auto task = folly::coro::co_invoke(
      [self = shared_from_this()](std::unique_ptr<GlobParams> p)
          -> folly::coro::Task<std::unique_ptr<Glob>> {
        co_return co_await self->co_globFilesImpl(std::move(p));
      },
      std::move(params));

  if (server_->usingPrefetchExecutor()) {
    // Pin the entire coroutine to the prefetch executor so all resumptions
    // after co_await points stay on that executor, not the Thrift thread.
    auto pinned = folly::coro::co_withExecutor(
        folly::getKeepAliveToken(server_->getPrefetchFilesV2Executor().get()),
        std::move(task));
    if (isBackground) {
      folly::futures::detachOn(
          server_->getPrefetchFilesV2Executor().get(),
          std::move(pinned).start());
      return ImmediateFuture<std::unique_ptr<Glob>>(std::make_unique<Glob>())
          .semi();
    }
    return std::move(pinned).start();
  }
  return serialDetachIfBackgrounded<Glob>(
      ImmediateFuture{std::move(task).semi()}, server_, isBackground);
}

folly::coro::now_task<std::unique_ptr<Glob>>
EdenServiceHandler::co_globFilesImpl(std::unique_ptr<GlobParams> params) {
  TaskTraceBlock block{"EdenServiceHandler::globFiles"};
  auto mountHandle = lookupMount(params->mountPoint());
  if (!params->revisions().value().empty()) {
    params->revisions() =
        resolveRootsWithLastFilter(params->revisions().value(), mountHandle);
  }
  ThriftGlobImpl globber{*params};
  auto requestContext = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_GLOB_CALL_WITH_CANCELLATION(
      DBG3,
      true,
      requestContext,
      *params->mountPoint(),
      toLogArg(*params->globs()),
      globber.logString());
  if (*params->background()) {
    helper->markBackground();
  }
  auto& context = helper->getFetchContext();

  co_await folly::coro::co_reschedule_on_current_executor;

  maybeLogExpensiveGlob(
      *params->globs(),
      *params->searchRoot(),
      globber,
      context,
      server_->getServerState());

  GlobFilesRequestScope globFilesRequestScope{
      server_->getServerState(), globber.logString(*params->globs()), context};

  co_return co_await globber.glob(
      mountHandle.getEdenMountPtr(),
      server_->getServerState(),
      std::move(*params->globs()),
      context.copy());
}

namespace {

PrefetchStats populatePrefetchStats(
    const PrefetchFetchContext& ctx,
    int64_t durationMs) {
  PrefetchStats stats;

  using ObjectType = ObjectFetchContext::ObjectType;
  using Origin = ObjectFetchContext::Origin;

  // Tree stats (counts)
  stats.treesFromMemoryCache() = ctx.countFetchesOfTypeAndOrigin(
      ObjectType::Tree, Origin::FromMemoryCache);
  stats.treesFromDiskCache() =
      ctx.countFetchesOfTypeAndOrigin(ObjectType::Tree, Origin::FromDiskCache);
  stats.treesFromNetwork() = ctx.countFetchesOfTypeAndOrigin(
      ObjectType::Tree, Origin::FromNetworkFetch);

  // Blob stats (counts)
  stats.blobsFromMemoryCache() = ctx.countFetchesOfTypeAndOrigin(
      ObjectType::Blob, Origin::FromMemoryCache);
  stats.blobsFromDiskCache() =
      ctx.countFetchesOfTypeAndOrigin(ObjectType::Blob, Origin::FromDiskCache);
  stats.blobsFromNetwork() = ctx.countFetchesOfTypeAndOrigin(
      ObjectType::Blob, Origin::FromNetworkFetch);

  // Tree stats (bytes)
  stats.treeBytesFromMemoryCache() = ctx.countBytesFetchedOfTypeAndOrigin(
      ObjectType::Tree, Origin::FromMemoryCache);
  stats.treeBytesFromDiskCache() = ctx.countBytesFetchedOfTypeAndOrigin(
      ObjectType::Tree, Origin::FromDiskCache);
  stats.treeBytesFromNetwork() = ctx.countBytesFetchedOfTypeAndOrigin(
      ObjectType::Tree, Origin::FromNetworkFetch);

  // Blob stats (bytes)
  // First try per-origin byte tracking from the fetch pipeline.
  int64_t blobBytesMemory = ctx.countBytesFetchedOfTypeAndOrigin(
      ObjectType::Blob, Origin::FromMemoryCache);
  int64_t blobBytesDisk = ctx.countBytesFetchedOfTypeAndOrigin(
      ObjectType::Blob, Origin::FromDiskCache);
  int64_t blobBytesNetwork = ctx.countBytesFetchedOfTypeAndOrigin(
      ObjectType::Blob, Origin::FromNetworkFetch);

  // Defense-in-depth: if per-origin blob byte tracking is broken (e.g.
  // IGNORE_RESULT discards content before bytes are recorded), fall back
  // to tree-entry file sizes from the glob walk, distributed proportionally
  // across origins based on blob counts.
  //
  // Note: totalBlobs can be non-zero even when bytes are zero because
  // the backing store can report per-origin blob counts (via
  // didFetchBatch) without corresponding byte amounts.
  if (blobBytesMemory + blobBytesDisk + blobBytesNetwork == 0) {
    uint64_t prefetchedBytes = ctx.getPrefetchedBlobBytes();
    int64_t totalBlobs = *stats.blobsFromMemoryCache() +
        *stats.blobsFromDiskCache() + *stats.blobsFromNetwork();
    if (prefetchedBytes > 0 && totalBlobs > 0) {
      blobBytesMemory = static_cast<int64_t>(
          prefetchedBytes * *stats.blobsFromMemoryCache() / totalBlobs);
      blobBytesDisk = static_cast<int64_t>(
          prefetchedBytes * *stats.blobsFromDiskCache() / totalBlobs);
      blobBytesNetwork = static_cast<int64_t>(
          prefetchedBytes - blobBytesMemory - blobBytesDisk);
    }
  }

  stats.blobBytesFromMemoryCache() = blobBytesMemory;
  stats.blobBytesFromDiskCache() = blobBytesDisk;
  stats.blobBytesFromNetwork() = blobBytesNetwork;

  // Timing
  stats.totalDurationMs() = durationMs;

  // Cache hit rate: (memory + disk) / total
  int64_t totalCacheHits = *stats.treesFromMemoryCache() +
      *stats.treesFromDiskCache() + *stats.blobsFromMemoryCache() +
      *stats.blobsFromDiskCache();
  int64_t totalAccesses = *stats.treesFromMemoryCache() +
      *stats.treesFromDiskCache() + *stats.treesFromNetwork() +
      *stats.blobsFromMemoryCache() + *stats.blobsFromDiskCache() +
      *stats.blobsFromNetwork();
  if (totalAccesses > 0) {
    stats.cacheHitRate() = 100.0 * static_cast<double>(totalCacheHits) /
        static_cast<double>(totalAccesses);
  } else {
    stats.cacheHitRate() = 0.0;
  }

  // Summary info
  stats.filesPrefetched() = *stats.blobsFromMemoryCache() +
      *stats.blobsFromDiskCache() + *stats.blobsFromNetwork();
  stats.filesFailed() = ctx.getFailureCount(ObjectFetchContext::Blob);

  return stats;
}

} // namespace

// The following functions are deliberately given external linkage
// (not left in the anonymous namespace above) so they can be exercised
// directly from EdenServiceHandlerTest.cpp - visible for testing.

CacheUsageState toThriftCacheUsageState(sapling::CacheUsageState state) {
  switch (state) {
    case sapling::CacheUsageState::NotConfigured:
      return CacheUsageState::NOT_CONFIGURED;
    case sapling::CacheUsageState::Unsupported:
      return CacheUsageState::UNSUPPORTED;
    case sapling::CacheUsageState::Available:
      return CacheUsageState::AVAILABLE;
    case sapling::CacheUsageState::Unavailable:
      return CacheUsageState::UNAVAILABLE;
  }
  return CacheUsageState::UNSUPPORTED;
}

/**
 * Convert a plain byte count (e.g. a `*BytesUsed` field) from Rust's u64 to
 * Thrift's signed i64. No "uncapped" sentinel applies here - only
 * `*BytesLimit` fields carry that meaning (see `toThriftByteLimit`) - so a
 * count is only ever clamped defensively against overflow, never mapped to
 * -1. u64::MAX is unrealistic for a real cache byte count, but if it ever
 * occurred it should surface as an implausibly large number, not be
 * silently reinterpreted as "uncapped".
 */
int64_t toThriftByteCount(uint64_t value) {
  if (value > static_cast<uint64_t>(std::numeric_limits<int64_t>::max())) {
    XLOGF(
        WARN,
        "hgcache byte count {} exceeds INT64_MAX; clamping for Thrift",
        value);
    return std::numeric_limits<int64_t>::max();
  }
  return static_cast<int64_t>(value);
}

/**
 * Convert a byte *limit* (e.g. a `*BytesLimit` field) from Rust's u64 to
 * Thrift's signed i64, explicitly mapping the "uncapped" sentinel (u64::MAX,
 * see `to_ffi_cache_usage` in ffi.rs) to -1 rather than relying on
 * `static_cast<int64_t>` silently producing -1 via two's-complement
 * coincidence. Any other value is delegated to `toThriftByteCount`'s
 * overflow clamp.
 */
int64_t toThriftByteLimit(uint64_t value) {
  if (value == std::numeric_limits<uint64_t>::max()) {
    return -1;
  }
  return toThriftByteCount(value);
}

void validateInjectFaultDefinition(const FaultDefinition& fault) {
  if (fault.unblockKeyClass().has_value() !=
      fault.unblockKeyValueRegex().has_value()) {
    throw newEdenError(
        EINVAL,
        EdenErrorType::ARGUMENT_ERROR,
        "unblockKeyClass and unblockKeyValueRegex must be set together");
  }
}

namespace {

/**
 * Attach hgcache disk-usage stats to a prefetch result. Only call when the
 * client asked for stats - the lookup does real work (cache reload check
 * plus per-repo disk usage). A mount that isn't Sapling-backed, or a
 * genuine lookup failure, both leave result.cacheStats() unset - the
 * distinction is only visible in the logs, since PrefetchResult has no
 * separate "stats lookup failed" field.
 */
void populateHgCacheStats(EdenMount& edenMount, PrefetchResult& result) {
  auto backingStore = edenMount.getObjectStore()->getBackingStore();
  auto saplingBackingStore = tryCastToSaplingBackingStore(backingStore);
  if (!saplingBackingStore) {
    // Not a Sapling-backed mount (e.g. Git or RE-CAS) - hgcache stats simply
    // don't apply here. This is an expected, benign case, not an error.
    return;
  }

  auto cacheStats = saplingBackingStore->getCacheStats();
  if (cacheStats.hasError()) {
    XLOGF(
        WARN,
        "Failed to get hgcache stats for {}: {}",
        edenMount.getPath(),
        cacheStats.error());
    return;
  }

  HgCacheStats hgStats;
  hgStats.cachePath() = std::string(cacheStats->cache_path);
  hgStats.cachePathConfigured() = cacheStats->cache_path_configured;
  hgStats.blobState() = toThriftCacheUsageState(cacheStats->blob_state);
  hgStats.blobBytesUsed() = toThriftByteCount(cacheStats->blob_bytes_used);
  hgStats.blobBytesLimit() = toThriftByteLimit(cacheStats->blob_bytes_limit);
  hgStats.treeState() = toThriftCacheUsageState(cacheStats->tree_state);
  hgStats.treeBytesUsed() = toThriftByteCount(cacheStats->tree_bytes_used);
  hgStats.treeBytesLimit() = toThriftByteLimit(cacheStats->tree_bytes_limit);
  hgStats.lfsState() = toThriftCacheUsageState(cacheStats->lfs_state);
  hgStats.lfsBytesUsed() = toThriftByteCount(cacheStats->lfs_bytes_used);
  hgStats.lfsBytesLimit() = toThriftByteLimit(cacheStats->lfs_bytes_limit);
  result.cacheStats() = std::move(hgStats);
}

} // namespace

folly::coro::now_task<std::unique_ptr<PrefetchResult>>
EdenServiceHandler::prefetchFilesV2Impl(
    std::unique_ptr<PrefetchParams> params) {
  TaskTraceBlock block{"EdenServiceHandler::prefetchFilesV2"};
  auto mountHandle = lookupMount(params->mountPoint());
  if (!params->revisions().value().empty()) {
    params->revisions() =
        resolveRootsWithLastFilter(params->revisions().value(), mountHandle);
  }
  ThriftGlobImpl globber{*params};
  auto requestContext = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_PREFETCH_STATS_AND_CANCELLATION(
      DBG2,
      params->returnStats().value(),
      requestContext,
      *params->mountPoint(),
      toLogArg(*params->globs()),
      globber.logString());
  if (*params->background()) {
    helper->markBackground();
  }
  auto& context = helper->getFetchContext();
  auto returnPrefetchedFiles = *params->returnPrefetchedFiles();

  co_await folly::coro::co_reschedule_on_current_executor;

  maybeLogExpensiveGlob(
      *params->globs(),
      *params->searchRoot(),
      globber,
      context,
      server_->getServerState());

  std::optional<std::chrono::steady_clock::time_point> startTime;
  if (helper->getPrefetchFetchContext()->shouldCollectStats()) {
    startTime = std::chrono::steady_clock::now();
  }

  auto prefetchContext = helper->getPrefetchFetchContext().copy();

  auto glob = co_await globber.glob(
      mountHandle.getEdenMountPtr(),
      server_->getServerState(),
      std::move(*params->globs()),
      prefetchContext.copy());

  auto result = std::make_unique<PrefetchResult>();

  if (startTime.has_value()) {
    auto durationMs = std::chrono::duration_cast<std::chrono::milliseconds>(
                          std::chrono::steady_clock::now() - *startTime)
                          .count();
    auto* statsContext = dynamic_cast<PrefetchFetchContext*>(
        helper->getPrefetchFetchContext().get());
    if (statsContext == nullptr) {
      XLOG(ERR, "prefetch stats unavailable: unexpected fetch context");
    } else {
      result->stats() = populatePrefetchStats(*statsContext, durationMs);
    }
    populateHgCacheStats(mountHandle.getEdenMount(), *result);
  }

  if (returnPrefetchedFiles) {
    result->prefetchedFiles() = std::move(*glob);
  }
  co_return result;
}

folly::coro::Task<std::unique_ptr<PrefetchResult>>
EdenServiceHandler::co_prefetchFilesV2(std::unique_ptr<PrefetchParams> params) {
  auto isBackground = *params->background();

  if (isBackground && *params->returnStats()) {
    // The immediate response for a background prefetch is a placeholder
    // returned before the real work finishes - stats computed by the
    // detached coroutine would have nowhere to go, so reject the
    // combination up front rather than silently discarding them.
    throw newEdenError(
        EdenErrorType::ARGUMENT_ERROR,
        "prefetch --stats is not supported with --background: stats require "
        "waiting for the prefetch to complete");
  }

  // Build the work coroutine. Capture self via shared_from_this so the
  // coroutine outlives any background detach.
  // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
  auto task = folly::coro::co_invoke(
      [self = shared_from_this()](std::unique_ptr<PrefetchParams> p)
          -> folly::coro::Task<std::unique_ptr<PrefetchResult>> {
        co_return co_await self->prefetchFilesV2Impl(std::move(p));
      },
      std::move(params));

  // If using a dedicated executor, don't use serial executor since that
  // precludes parallelism. We offload prefetch work to a dedicated thread
  // pool to avoid overloading the Thrift CPU worker pool.
  //
  // TODO: detachOn fire-and-forget loses shutdown cancellation. Consider a
  // handler-scoped CancellableAsyncScope across this file's ~10 detach sites
  // (S532385).
  if (server_->usingPrefetchExecutor()) {
    folly::Executor::KeepAlive<> exec =
        folly::getKeepAliveToken(server_->getPrefetchFilesV2Executor().get());
    auto pinned = folly::coro::co_withExecutor(exec, std::move(task));
    if (isBackground) {
      // Fire-and-forget: start the coroutine on the prefetch executor and
      // detach the resulting SemiFuture there. Return an empty placeholder.
      folly::futures::detachOn(exec, std::move(pinned).start());
      co_return std::make_unique<PrefetchResult>();
    }
    co_return co_await std::move(pinned);
  }

  // No dedicated executor. The glob code has a very large fan-out that can
  // easily overload the Thrift CPU worker pool, so pin coroutine resumptions
  // to a SerialExecutor when the server isn't already globally serialized.
  // Without co_withExecutor, the coroutine would escape the serial executor
  // after the first external co_await, allowing concurrent fan-out from
  // multiple requests.
  if (server_->usingThriftSerialExecution()) {
    if (isBackground) {
      folly::futures::detachOn(
          server_->getServerState()->getThreadPool().get(),
          std::move(task).semi());
      co_return std::make_unique<PrefetchResult>();
    }
    co_return co_await std::move(task);
  }

  folly::Executor::KeepAlive<> serial = folly::SerialExecutor::create(
      server_->getServer()->getThreadManager().get());
  auto pinned = folly::coro::co_withExecutor(serial, std::move(task));
  if (isBackground) {
    folly::futures::detachOn(serial, std::move(pinned).start());
    co_return std::make_unique<PrefetchResult>();
  }
  co_return co_await std::move(pinned);
}

folly::SemiFuture<std::unique_ptr<ChangeOwnershipResponse>>
EdenServiceHandler::semifuture_changeOwnership(
    unique_ptr<ChangeOwnershipRequest> request) {
#ifndef _WIN32
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2, *request->mountPoint());
  auto& fetchContext = helper->getFetchContext();
  auto handle = lookupMount(*request->mountPoint());
  return wrapImmediateFuture(
             std::move(helper),
             handle.getEdenMount()
                 .chown(
                     static_cast<uid_t>(*request->uid()),
                     static_cast<gid_t>(*request->gid()),
                     fetchContext)
                 .ensure([handle] {})
                 .thenValue([](folly::Unit&&) {
                   return std::make_unique<ChangeOwnershipResponse>();
                 }))
      .semi();
#else
  NOT_IMPLEMENTED();
#endif // !_WIN32
}

folly::coro::Task<std::unique_ptr<GetScmStatusResult>>
EdenServiceHandler::co_getScmStatusV2(
    apache::thrift::RequestParams params,
    unique_ptr<GetScmStatusParams> scmParams) {
  auto* context = params.getRequestContext();
  auto rootIdOptions = scmParams->rootIdOptions().ensure();
  static const std::string kNoFidSentinel = "(none)";
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      DBG3,
      true,
      context,
      *scmParams->mountPoint(),
      folly::to<string>("commitHash=", logHash(*scmParams->commit())),
      folly::to<string>("listIgnored=", *scmParams->listIgnored()),
      folly::to<string>(
          "fid=",
          rootIdOptions.fid().has_value() ? *rootIdOptions.fid()
                                          : kNoFidSentinel));
  helper->getThriftFetchContext().fillClientRequestInfo(scmParams->cri());

  auto& fetchContext = helper->getFetchContext();
  auto edenConfig =
      server_->getServerState()->getReloadableConfig()->getEdenConfig();

  auto mountHandle = lookupMount(scmParams->mountPoint());

  // If we were passed a FilterID, create a RootID that contains the filter
  // and a varint that indicates the length of the original id.
  std::string parsedCommit = resolveRootId(
      std::move(*scmParams->commit()), rootIdOptions, mountHandle);
  auto rootId = mountHandle.getObjectStore().parseRootId(parsedCommit);

  const auto& enforceParents = edenConfig->enforceParents.getValue();

  // The connection token fires when the client goes away, the request token
  // when EdenFS cancels the request, for example for a forced restart.
  auto requestToken = fetchContext->getCancellationToken();

  // Owns its arguments, so the Task can run on another executor.
  auto diffBody = [mountHandle,
                   rootId,
                   cancellation = folly::cancellation_token_merge(
                       context->getConnectionContext()->getCancellationToken(),
                       requestToken),
                   fetchContext = fetchContext.copy(),
                   listIgnored = *scmParams->listIgnored(),
                   enforceParents = enforceParents]() mutable
      -> folly::coro::Task<std::unique_ptr<ScmStatus>> {
    co_return co_await mountHandle.getEdenMount().co_diff(
        mountHandle.getRootInode(),
        rootId,
        cancellation,
        fetchContext,
        listIgnored,
        enforceParents);
  };

  std::unique_ptr<ScmStatus> status;
  folly::Executor* executor = edenConfig->thriftUseStatusExecutor.getValue()
      ? server_->getDiffWorkExecutor()
      : nullptr;
  if (executor) {
    // The diff's coroutines run inline until they suspend, and with the
    // default inline detached executor nothing in the walk suspends: the
    // overlay reads of materialized directories complete before the children
    // are visited and hashing is synchronous, so the whole diff runs on one
    // thread. The detached executor makes each directory load asynchronous
    // work on the diff work executor, so a child suspends on its load and
    // resumes on that pool; that is what spreads the walk, and the hashing in
    // it, across the pool. The pin puts those resumptions on the pool rather
    // than on this Thrift worker.
    fetchContext->setDetachedExecutor(executor);
    status = co_await folly::coro::co_withExecutor(executor, diffBody());
  } else {
    status = co_await diffBody();
  }
  // A cancelled diff returns what it had walked so far. A client that is
  // still connected must not take that for the full status.
  if (requestToken.isCancellationRequested()) {
    throw folly::OperationCancelled{};
  }

  auto result = std::make_unique<GetScmStatusResult>();
  result->status() = std::move(*status);
  result->version() = server_->getVersion();
  co_return result;
}

folly::SemiFuture<unique_ptr<ScmStatus>>
EdenServiceHandler::semifuture_getScmStatusBetweenRevisions(
    unique_ptr<string> mountPoint,
    unique_ptr<string> oldHash,
    unique_ptr<string> newHash) {
  auto* context = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      DBG2,
      true,
      context,
      *mountPoint,
      folly::to<string>("oldHash=", logHash(*oldHash)),
      folly::to<string>("newHash=", logHash(*newHash)));
  auto mountHandle = lookupMount(mountPoint);
  auto& fetchContext = helper->getFetchContext();

  // parseRootId assumes that the passed in id will contain information
  // about the active filter. This legacy code path does not respect filters,
  // so the last active filter will always be passed in if it exists. For
  // non-FFS repos, the last filterID will be std::nullopt.
  std::string resolvedOldId =
      resolveRootIdWithLastFilter(std::move(*oldHash), mountHandle);
  std::string resolvedNewId =
      resolveRootIdWithLastFilter(std::move(*newHash), mountHandle);

  auto callback = std::make_unique<ScmStatusDiffCallback>();
  auto requestToken = fetchContext->getCancellationToken();
  auto diffFuture = diffBetweenRoots(
      mountHandle.getObjectStore().parseRootId(resolvedOldId),
      mountHandle.getObjectStore().parseRootId(resolvedNewId),
      *mountHandle.getEdenMount().getCheckoutConfig(),
      mountHandle.getObjectStorePtr(),
      folly::cancellation_token_merge(
          context->getConnectionContext()->getCancellationToken(),
          requestToken),
      fetchContext,
      callback.get());
  return wrapImmediateFuture(
             std::move(helper),
             std::move(diffFuture)
                 .thenValue([callback = std::move(callback),
                             requestToken = std::move(requestToken)](auto&&) {
                   // A cancelled diff holds a partial status.
                   if (requestToken.isCancellationRequested()) {
                     throw folly::OperationCancelled{};
                   }
                   return std::make_unique<ScmStatus>(
                       callback->extractStatus());
                 }))
      .semi();
}

folly::SemiFuture<std::unique_ptr<MatchFileSystemResponse>>
EdenServiceHandler::semifuture_matchFilesystem(
    std::unique_ptr<MatchFileSystemRequest> params) {
  auto helper =
      INSTRUMENT_THRIFT_CALL(DBG2, *params->mountPoint(), *params->paths());
#ifdef _WIN32
  auto mountHandle = lookupMount(params->mountPoint()->mountPoint());
  if (auto* prjfsChannel = mountHandle.getEdenMount().getPrjfsChannel()) {
    std::vector<ImmediateFuture<folly::Unit>> results;
    results.reserve(params->paths()->size());
    for (auto& path : *params->paths()) {
      results.push_back(prjfsChannel->matchEdenViewOfFileToFS(
          relpathFromUserPath(path), helper->getFetchContext()));
    }
    return wrapImmediateFuture(
               std::move(helper),
               ImmediateFuture{
                   collectAll(std::move(results))
                       .ensure([mountHandle]() {})
                       .thenValue([](std::vector<folly::Try<folly::Unit>>
                                         raw_results) {
                         std::vector<MatchFilesystemPathResult> results;
                         results.reserve(raw_results.size());
                         for (auto& raw_result : raw_results) {
                           MatchFilesystemPathResult result{};
                           if (raw_result.hasException()) {
                             result.error() =
                                 newEdenError(raw_result.exception());
                           }
                           results.push_back(std::move(result));
                         }
                         auto final_result =
                             std::make_unique<MatchFileSystemResponse>();
                         final_result->results() = std::move(results);
                         return final_result;
                       })})
        .semi();
  }
#endif
  throw newEdenError(
      ENOTSUP,
      EdenErrorType::POSIX_ERROR,
      "matchFilesystemStat only supported for PrjFs repos which {} is not",
      *params->mountPoint());
}

folly::SemiFuture<std::unique_ptr<DebugGetScmBlobResponse>>
EdenServiceHandler::semifuture_debugGetBlob(
    std::unique_ptr<DebugGetScmBlobRequest> request) {
  auto config = server_->getServerState()->getEdenConfig();
  bool use_coroutines = config->enableCoroutinesInDebugGetBlob.getValue();

  if (use_coroutines) {
    auto requestContext = getRequestContext();
    return co_debugGetBlobImpl(requestContext, std::move(request))
        .semi()
        .deferError(
            [](folly::exception_wrapper&& ex)
                -> std::unique_ptr<DebugGetScmBlobResponse> {
              throw newEdenError(ex);
            });
  } else {
    return debugGetBlobImpl(std::move(request));
  }
}

folly::coro::Task<std::unique_ptr<DebugGetScmBlobResponse>>
EdenServiceHandler::co_debugGetBlobImpl(
    apache::thrift::Cpp2RequestContext* requestContext,
    std::unique_ptr<DebugGetScmBlobRequest> request) {
  auto mountid = request->mountId();
  auto idStr = request->id();
  auto origins = request->origins();

  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      DBG2, true, requestContext, *mountid, logHash(*idStr), *origins);
  auto cancellationToken = getCancellationToken(helper->getRequestId());

  auto& faultInjector = server_->getServerState()->getFaultInjector();
  if (faultInjector.hasBlockWithCancelFault("debugGetBlob", idStr->c_str())) {
    co_await faultInjector
        .waitForCancelOrTimeout(
            "debugGetBlob", *cancellationToken, idStr->c_str())
        .semi(); // Throws
  }

  auto mountHandle = lookupMount(*mountid);
  auto edenMount = mountHandle.getEdenMountPtr();
  auto id = edenMount->getObjectStore()->parseObjectId(*idStr);
  auto originFlags = DataFetchOriginFlags::raw(*origins);
  auto store = edenMount->getObjectStore();
  std::vector<folly::coro::Task<ScmBlobWithOrigin>> blobTasks;

  // We use co_invoke to have the same collectAll pattern from the
  // Futures-based getBlob. co_invoke is not recommend for 'hot
  // paths' due to overhead. From benchmarking, no performance
  // regressions were found.
  if (originFlags.contains(FROMWHERE_MEMORY_CACHE)) {
    blobTasks.push_back(
        folly::coro::co_invoke(
            [edenMount, id]() -> folly::coro::Task<ScmBlobWithOrigin> {
              co_return transformToBlobFromOrigin(
                  edenMount,
                  id,
                  folly::Try<std::shared_ptr<const Blob>>{
                      edenMount->getBlobCache()->get(id).object},
                  DataFetchOrigin::MEMORY_CACHE);
            }));
  }
  auto& context = helper->getFetchContext();
  if (originFlags.contains(FROMWHERE_LOCAL_BACKING_STORE)) {
    auto slOid = SlOid{id};
    auto backingStore = edenMount->getObjectStore()->getBackingStore();
    std::shared_ptr<SaplingBackingStore> saplingBackingStore =
        castToSaplingBackingStore(backingStore, edenMount->getPath());
    blobTasks.push_back(
        folly::coro::co_invoke(
            [edenMount, id, saplingBackingStore, slOid, &context]()
                -> folly::coro::Task<ScmBlobWithOrigin> {
              auto blob = saplingBackingStore->getBlobLocal(slOid, context);
              co_return transformToBlobFromOrigin(
                  edenMount,
                  id,
                  std::move(blob),
                  DataFetchOrigin::LOCAL_BACKING_STORE);
            }));
  }
  if (originFlags.contains(FROMWHERE_REMOTE_BACKING_STORE)) {
    auto slOid = SlOid{id};
    auto backingStore = edenMount->getObjectStore()->getBackingStore();
    std::shared_ptr<SaplingBackingStore> saplingBackingStore =
        castToSaplingBackingStore(backingStore, edenMount->getPath());
    blobTasks.push_back(
        folly::coro::co_invoke(
            [edenMount, id, saplingBackingStore, slOid, &context]()
                -> folly::coro::Task<ScmBlobWithOrigin> {
              auto blob = saplingBackingStore->getBlobRemote(slOid, context);
              co_return transformToBlobFromOrigin(
                  edenMount,
                  id,
                  std::move(blob),
                  DataFetchOrigin::REMOTE_BACKING_STORE);
            }));
  }
  if (originFlags.contains(FROMWHERE_ANYWHERE)) {
    blobTasks.push_back(
        folly::coro::co_invoke(
            [edenMount, id, store, &context]()
                -> folly::coro::Task<ScmBlobWithOrigin> {
              try {
                auto blob = co_await store->co_getBlob(id, context);
                co_return transformToBlobFromOrigin(
                    edenMount,
                    id,
                    folly::Try<std::shared_ptr<const Blob>>{blob},
                    DataFetchOrigin::ANYWHERE);
              } catch (const std::exception&) {
                folly::Try<std::shared_ptr<const Blob>> blobTry;
                blobTry.emplaceException(std::current_exception());
                co_return transformToBlobFromOrigin(
                    edenMount,
                    id,
                    std::move(blobTry),
                    DataFetchOrigin::ANYWHERE);
              }
            }));
  }
  auto blobs = co_await folly::coro::collectAllRange(std::move(blobTasks));
  auto response = std::make_unique<DebugGetScmBlobResponse>();
  response->blobs() = std::move(blobs);
  co_return response;
}

folly::coro::Task<std::unique_ptr<::facebook::eden::CancelRequestsResponse>>
EdenServiceHandler::co_cancelRequests(
    apache::thrift::RequestParams params,
    std::unique_ptr<::facebook::eden::CancelRequestsParams> p_params) {
  auto requestContext = params.getRequestContext();
  auto helper =
      INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(DBG3, false, requestContext);
  auto response = std::make_unique<CancelRequestsResponse>();

  if (!p_params || p_params->requestIds()->size() == 0) {
    co_return response;
  }

  const auto& requestIds = *p_params->requestIds();
  response->results()->reserve(requestIds.size());
  XLOGF(DBG3, "cancelRequests called for {} request IDs", requestIds.size());

  for (const auto requestId : requestIds) {
    XLOGF(DBG3, "Processing cancellation for requestId: {}", requestId);

    CancellationStatusOrError result;

    // Attempt to cancel the request
    if (requestCancellation(requestId)) {
      CancellationStatus success;
      success.requestId() = requestId;
      result.success() = std::move(success);
      XLOGF(DBG3, "Successfully cancelled request {}", requestId);
    } else {
      EdenError error;
      error.errorType() = EdenErrorType::CANCELLATION_ERROR;

      // Check if the request is known but not cancelable
      if (auto status = getRequestStatus(requestId)) {
        switch (*status) {
          case RequestStatus::UNCANCELABLE:
            error.errorCode() = EINVAL; // Not cancellable
            error.message() =
                fmt::format("Request {} is not cancellable", requestId);
            XLOGF(DBG3, "Request {} is not cancellable", requestId);
            break;
          case RequestStatus::REQUESTED:
            error.errorCode() = EALREADY; // Already cancelled
            error.message() =
                fmt::format("Request {} was already cancelled", requestId);
            XLOGF(DBG3, "Request {} was already cancelled", requestId);
            break;
          case RequestStatus::ACTIVE:
            error.errorCode() = EBUSY; // Failed to cancel active request
            error.message() =
                fmt::format("Request {} failed to cancel", requestId);
            XLOGF(DBG3, "Request {} failed to cancel", requestId);
            break;
        }
      } else {
        error.errorCode() = ENOENT; // Request not found
        error.message() = fmt::format("Request {} not found", requestId);
        XLOGF(DBG3, "Request {} not found", requestId);
      }

      result.error() = std::move(error);
    }

    response->results()->push_back(std::move(result));
  }

  XLOGF(DBG3, "Processed {} cancellation requests", requestIds.size());
  co_return response;
}

folly::SemiFuture<std::unique_ptr<DebugGetScmBlobResponse>>
EdenServiceHandler::debugGetBlobImpl(
    std::unique_ptr<DebugGetScmBlobRequest> request) {
  auto mountid = request->mountId();
  auto idStr = request->id();
  auto origins = request->origins();
  auto helper =
      INSTRUMENT_THRIFT_CALL(DBG2, *mountid, logHash(*idStr), *origins);

  auto mountHandle = lookupMount(*mountid);
  auto edenMount = mountHandle.getEdenMountPtr();
  auto id = edenMount->getObjectStore()->parseObjectId(*idStr);
  auto originFlags = DataFetchOriginFlags::raw(*origins);
  auto store = edenMount->getObjectStore();

  std::vector<ImmediateFuture<ScmBlobWithOrigin>> blobFutures;

  if (originFlags.contains(FROMWHERE_MEMORY_CACHE)) {
    blobFutures.emplace_back(transformToBlobFromOrigin(
        edenMount,
        id,
        folly::Try<std::shared_ptr<const Blob>>{
            edenMount->getBlobCache()->get(id).object},
        DataFetchOrigin::MEMORY_CACHE));
  }

  auto& context = helper->getFetchContext();

  if (originFlags.contains(FROMWHERE_LOCAL_BACKING_STORE)) {
    auto slOid = SlOid{id};
    auto backingStore = edenMount->getObjectStore()->getBackingStore();
    std::shared_ptr<SaplingBackingStore> saplingBackingStore =
        castToSaplingBackingStore(backingStore, edenMount->getPath());

    blobFutures.emplace_back(transformToBlobFromOrigin(
        edenMount,
        id,
        saplingBackingStore->getBlobLocal(slOid, context),
        DataFetchOrigin::LOCAL_BACKING_STORE));
  }
  if (originFlags.contains(FROMWHERE_REMOTE_BACKING_STORE)) {
    auto slOid = SlOid{id};
    auto backingStore = edenMount->getObjectStore()->getBackingStore();
    std::shared_ptr<SaplingBackingStore> saplingBackingStore =
        castToSaplingBackingStore(backingStore, edenMount->getPath());
    blobFutures.emplace_back(transformToBlobFromOrigin(
        edenMount,
        id,
        saplingBackingStore->getBlobRemote(slOid, context),
        DataFetchOrigin::REMOTE_BACKING_STORE));
  }
  if (originFlags.contains(FROMWHERE_ANYWHERE)) {
    blobFutures.emplace_back(
        store->getBlob(id, context).thenTry([edenMount, id](auto&& blob) {
          return transformToBlobFromOrigin(
              edenMount, id, std::move(blob), DataFetchOrigin::ANYWHERE);
        }));
  }

  return wrapImmediateFuture(
             std::move(helper),
             collectAllSafe(std::move(blobFutures))
                 .thenValue([](std::vector<ScmBlobWithOrigin> blobs) {
                   auto response = std::make_unique<DebugGetScmBlobResponse>();
                   response->blobs() = std::move(blobs);
                   return response;
                 }))
      .semi();
}

folly::SemiFuture<std::unique_ptr<DebugGetBlobMetadataResponse>>
EdenServiceHandler::semifuture_debugGetBlobMetadata(
    std::unique_ptr<DebugGetBlobMetadataRequest> request) {
  auto mountid = request->mountId();
  auto idStr = request->id();
  auto origins = request->origins();
  auto helper =
      INSTRUMENT_THRIFT_CALL(DBG2, *mountid, logHash(*idStr), *origins);

  auto mountHandle = lookupMount(*mountid);
  auto edenMount = mountHandle.getEdenMountPtr();
  auto id = edenMount->getObjectStore()->parseObjectId(*idStr);
  auto originFlags = DataFetchOriginFlags::raw(*origins);
  auto store = edenMount->getObjectStore();

  auto& fetchContext = helper->getFetchContext();

  std::vector<ImmediateFuture<BlobMetadataWithOrigin>> blobFutures;

  if (originFlags.contains(FROMWHERE_MEMORY_CACHE)) {
    auto auxData = store->getBlobAuxDataFromInMemoryCache(id, fetchContext);
    blobFutures.emplace_back(transformToBlobMetadataFromOrigin(
        edenMount, id, auxData, DataFetchOrigin::MEMORY_CACHE));
  }
  if (originFlags.contains(FROMWHERE_LOCAL_BACKING_STORE)) {
    auto slOid = SlOid{id};
    auto backingStore = edenMount->getObjectStore()->getBackingStore();
    std::shared_ptr<SaplingBackingStore> saplingBackingStore =
        castToSaplingBackingStore(backingStore, edenMount->getPath());

    auto auxData =
        saplingBackingStore->getLocalBlobAuxData(slOid).value_or(nullptr);

    blobFutures.emplace_back(transformToBlobMetadataFromOrigin(
        edenMount,
        id,
        std::move(auxData),
        DataFetchOrigin::LOCAL_BACKING_STORE));
  }
  if (originFlags.contains(FROMWHERE_REMOTE_BACKING_STORE)) {
    auto slOid = SlOid{id};
    auto backingStore = edenMount->getObjectStore()->getBackingStore();
    std::shared_ptr<SaplingBackingStore> saplingBackingStore =
        castToSaplingBackingStore(backingStore, edenMount->getPath());

    blobFutures.emplace_back(
        ImmediateFuture{
            folly::coro::co_invoke(
                [](std::shared_ptr<SaplingBackingStore> backingStore,
                   SlOid oid,
                   ObjectFetchContextPtr context)
                    -> folly::coro::Task<BackingStore::GetBlobAuxResult> {
                  co_return co_await backingStore->co_getBlobAuxDataEnqueue(
                      oid, context);
                },
                std::move(saplingBackingStore),
                std::move(slOid),
                fetchContext.copy())
                .semi()}
            .thenValue([edenMount, id](BackingStore::GetBlobAuxResult result) {
              return transformToBlobMetadataFromOrigin(
                  edenMount,
                  id,
                  std::move(result.blobAux),
                  DataFetchOrigin::REMOTE_BACKING_STORE);
            }));
  }
  if (originFlags.contains(FROMWHERE_ANYWHERE)) {
    blobFutures.emplace_back(store->getBlobAuxData(id, fetchContext)
                                 .thenTry([edenMount, id](auto&& auxData) {
                                   return transformToBlobMetadataFromOrigin(
                                       std::move(auxData),
                                       DataFetchOrigin::ANYWHERE);
                                 }));
  }

  return wrapImmediateFuture(
             std::move(helper),
             collectAllSafe(std::move(blobFutures))
                 .thenValue([](std::vector<BlobMetadataWithOrigin> blobs) {
                   auto response =
                       std::make_unique<DebugGetBlobMetadataResponse>();
                   response->metadatas() = std::move(blobs);
                   return response;
                 }))
      .semi();
}

folly::SemiFuture<std::unique_ptr<DebugGetScmTreeResponse>>
EdenServiceHandler::semifuture_debugGetTree(
    std::unique_ptr<DebugGetScmTreeRequest> request) {
  auto mountid = request->mountId();
  auto idStr = request->id();
  auto origins = request->origins();
  auto helper =
      INSTRUMENT_THRIFT_CALL(DBG2, *mountid, logHash(*idStr), *origins);

  auto mountHandle = lookupMount(*mountid);
  auto edenMount = mountHandle.getEdenMountPtr();
  auto id = edenMount->getObjectStore()->parseObjectId(*idStr);
  auto originFlags = DataFetchOriginFlags::raw(*origins);
  auto store = edenMount->getObjectStore();

  std::vector<ImmediateFuture<ScmTreeWithOrigin>> treeFutures;

  if (originFlags.contains(FROMWHERE_MEMORY_CACHE)) {
    treeFutures.emplace_back(transformToTreeFromOrigin(
        edenMount,
        id,
        folly::Try<std::shared_ptr<const Tree>>{store->getTreeCache()->get(id)},
        DataFetchOrigin::MEMORY_CACHE));
  }

  auto& context = helper->getFetchContext();

  if (originFlags.contains(FROMWHERE_LOCAL_BACKING_STORE)) {
    auto slOid = SlOidView{id};

    auto backingStore = edenMount->getObjectStore()->getBackingStore();
    std::shared_ptr<SaplingBackingStore> saplingBackingStore =
        castToSaplingBackingStore(backingStore, edenMount->getPath());

    treeFutures.emplace_back(transformToTreeFromOrigin(
        edenMount,
        id,
        folly::Try<std::shared_ptr<const Tree>>{
            saplingBackingStore->getTreeLocal(slOid, context)},
        DataFetchOrigin::LOCAL_BACKING_STORE));
  }

  if (originFlags.contains(FROMWHERE_REMOTE_BACKING_STORE)) {
    auto slOid = SlOidView{id};
    auto backingStore = edenMount->getObjectStore()->getBackingStore();
    std::shared_ptr<SaplingBackingStore> saplingBackingStore =
        castToSaplingBackingStore(backingStore, edenMount->getPath());
    treeFutures.emplace_back(transformToTreeFromOrigin(
        edenMount,
        id,
        saplingBackingStore->getTreeRemote(slOid, context),
        DataFetchOrigin::REMOTE_BACKING_STORE));
  }

  if (originFlags.contains(FROMWHERE_ANYWHERE)) {
    treeFutures.emplace_back(store->getTree(id, helper->getFetchContext())
                                 .thenTry([edenMount, id](auto&& tree) mutable {
                                   return transformToTreeFromOrigin(
                                       std::move(edenMount),
                                       id,
                                       std::move(tree),
                                       DataFetchOrigin::ANYWHERE);
                                 }));
  }

  return wrapImmediateFuture(
             std::move(helper),
             collectAllSafe(std::move(treeFutures))
                 .thenValue([](std::vector<ScmTreeWithOrigin> trees) {
                   auto response = std::make_unique<DebugGetScmTreeResponse>();
                   response->trees() = std::move(trees);
                   return response;
                 }))
      .ensure([mountHandle] {})
      .semi();
}

namespace {
class InodeStatusCallbacks : public TraversalCallbacks {
 public:
  explicit InodeStatusCallbacks(
      EdenMount* mount,
      int64_t flags,
      std::vector<TreeInodeDebugInfo>& results)
      : mount_{mount}, flags_{flags}, results_{results} {}

  void visitTreeInode(
      RelativePathPiece path,
      InodeNumber ino,
      const std::optional<ObjectId>& id,
      uint64_t fsRefcount,
      const std::vector<ChildEntry>& entries) override {
#ifndef _WIN32
    auto* inodeMetadataTable = mount_->getInodeMetadataTable();
#endif

    TreeInodeDebugInfo info;
    info.inodeNumber() = ino.get();
    info.path() = path.asString();
    info.materialized() = !id.has_value();
    if (id.has_value()) {
      info.treeHash() = mount_->getObjectStore()->renderObjectId(id.value());
    }
    info.refcount() = fsRefcount;

    info.entries()->reserve(entries.size());

    for (auto& entry : entries) {
      TreeInodeEntryDebugInfo entryInfo;
      entryInfo.name() = entry.name.asString();
      entryInfo.inodeNumber() = entry.ino.get();

      // This could be enabled on Windows if InodeMetadataTable was removed.
#ifndef _WIN32
      if (auto metadata = (flags_ & eden_constants::DIS_COMPUTE_ACCURATE_MODE_)
              ? inodeMetadataTable->getOptional(entry.ino)
              : std::nullopt) {
        entryInfo.mode() = metadata->mode;
      } else {
        entryInfo.mode() = dtype_to_mode(entry.dtype);
      }
#else
      entryInfo.mode_ref() = dtype_to_mode(entry.dtype);
#endif

      entryInfo.loaded() = entry.loadedChild != nullptr;
      entryInfo.materialized() = !entry.id.has_value();
      if (entry.id.has_value()) {
        entryInfo.hash() =
            mount_->getObjectStore()->renderObjectId(entry.id.value());
      }

      if ((flags_ & eden_constants::DIS_COMPUTE_BLOB_SIZES_) &&
          dtype_t::Dir != entry.dtype) {
        if (entry.id.has_value()) {
          // schedule fetching size from ObjectStore::getBlobSize
          requestedSizes_.push_back(
              RequestedSize{
                  results_.size(), info.entries()->size(), entry.id.value()});
        } else {
#ifndef _WIN32
          entryInfo.fileSize() = mount_->getOverlayFileAccess()->getFileSize(
              entry.ino, entry.loadedChild.get());
#else
          // This following code ends up doing a stat in the working
          // directory. This is safe to do as Windows works very differently
          // from Linux/macOS when dealing with materialized files. In this
          // code, we know that the file is materialized because we do not
          // have a id for it, and every materialized file is present on
          // disk and reading/stating it is guaranteed to be done without
          // EdenFS involvement. If somehow EdenFS is wrong, and this ends up
          // triggering a recursive call into EdenFS, we are detecting this
          // and simply bailing out very early in the callback.
          auto filePath = mount_->getPath() + path + entry.name;
          struct stat fileStat;
          if (::stat(filePath.c_str(), &fileStat) == 0) {
            entryInfo.fileSize_ref() = fileStat.st_size;
          } else {
            // Couldn't read the file, let's pretend it has a size of 0.
            entryInfo.fileSize_ref() = 0;
          }
#endif
        }
      }

      info.entries()->push_back(entryInfo);
    }

    results_.push_back(std::move(info));
  }

  bool shouldRecurse(const ChildEntry& entry) override {
    if (flags_ & eden_constants::DIS_NOT_RECURSIVE_) {
      return false;
    }

    if ((flags_ & eden_constants::DIS_REQUIRE_LOADED_) && !entry.loadedChild) {
      return false;
    }
    if ((flags_ & eden_constants::DIS_REQUIRE_MATERIALIZED_) &&
        entry.id.has_value()) {
      return false;
    }
    return true;
  }

  void fillBlobSizes(const ObjectFetchContextPtr& fetchContext) {
    std::vector<ImmediateFuture<folly::Unit>> futures;
    futures.reserve(requestedSizes_.size());
    for (auto& request : requestedSizes_) {
      futures.push_back(mount_->getObjectStore()
                            ->getBlobSize(request.id, fetchContext)
                            .thenValue([this, request](uint64_t blobSize) {
                              results_.at(request.resultIndex)
                                  .entries()
                                  ->at(request.entryIndex)
                                  .fileSize() = blobSize;
                            }));
    }
    collectAll(std::move(futures)).get();
  }

 private:
  struct RequestedSize {
    size_t resultIndex;
    size_t entryIndex;
    ObjectId id;
  };

  EdenMount* mount_;
  int64_t flags_;
  std::vector<TreeInodeDebugInfo>& results_;
  std::vector<RequestedSize> requestedSizes_;
};

} // namespace

void EdenServiceHandler::debugInodeStatus(
    vector<TreeInodeDebugInfo>& inodeInfo,
    unique_ptr<string> mountPoint,
    unique_ptr<std::string> path,
    int64_t flags,
    std::unique_ptr<SyncBehavior> sync) {
  if (0 == flags) {
    flags = eden_constants::DIS_REQUIRE_LOADED_ |
        eden_constants::DIS_COMPUTE_BLOB_SIZES_;
  }

  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG2, *mountPoint, *path, flags, getSyncTimeout(*sync));
  auto mountHandle = lookupMount(mountPoint);

  waitForPendingWrites(mountHandle.getEdenMount(), *sync)
      .thenValue([mountHandle,
                  &inodeInfo,
                  path = std::move(path),
                  flags,
                  helper = std::move(helper)](auto&&) mutable {
        auto inode =
            inodeFromUserPath(
                mountHandle.getEdenMount(), *path, helper->getFetchContext())
                .asTreePtr();
        auto inodePath = inode->getPath().value();

        InodeStatusCallbacks callbacks{
            &mountHandle.getEdenMount(), flags, inodeInfo};
        traverseObservedInodes(*inode, inodePath, callbacks);
        callbacks.fillBlobSizes(helper->getFetchContext());
      })
      .ensure([mountHandle] {})
      .get();
}

void EdenServiceHandler::debugOutstandingFuseCalls(
    [[maybe_unused]] std::vector<FuseCall>& outstandingCalls,
    [[maybe_unused]] std::unique_ptr<std::string> mountPoint) {
#ifndef _WIN32
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2);

  auto mountHandle = lookupMount(mountPoint);

  if (auto* fuseChannel = mountHandle.getEdenMount().getFuseChannel()) {
    for (const auto& call : fuseChannel->getOutstandingRequests()) {
      outstandingCalls.push_back(populateFuseCall(
          call.unique,
          call.request,
          *server_->getServerState()->getProcessInfoCache()));
    }
  }
#else
  NOT_IMPLEMENTED();
#endif // !_WIN32
}

void EdenServiceHandler::debugOutstandingNfsCalls(
    std::vector<NfsCall>& outstandingCalls,
    std::unique_ptr<std::string> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2);

  auto mountHandle = lookupMount(mountPoint);

  if (auto* nfsdChannel = mountHandle.getEdenMount().getNfsdChannel()) {
    for (const auto& call : nfsdChannel->getOutstandingRequests()) {
      NfsCall nfsCall;
      nfsCall.xid() = call.xid;
      outstandingCalls.push_back(nfsCall);
    }
  }
}

void EdenServiceHandler::debugOutstandingPrjfsCalls(
    [[maybe_unused]] std::vector<PrjfsCall>& outstandingCalls,
    [[maybe_unused]] std::unique_ptr<std::string> mountPoint) {
#ifdef _WIN32
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2);

  auto mountHandle = lookupMount(mountPoint);

  if (auto* prjfsChannel = mountHandle.getEdenMount().getPrjfsChannel()) {
    for (const auto& call :
         prjfsChannel->getInner()->getOutstandingRequests()) {
      outstandingCalls.push_back(populatePrjfsCall(call.type, call.data));
    }
  }
#else
  NOT_IMPLEMENTED();
#endif // _WIN32
}

bool EdenServiceHandler::debugLogError() {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2);
  auto& logger = server_->getServerState()->getErrorLogger();
  if (!logger.isEnabled()) {
    return false;
  }
  try {
    throw std::runtime_error("debugLogError: test error for e2e validation");
  } catch (const std::exception& ex) {
    logger.log(EdenErrorInfo::thrift(ex, "debugLogError"));
  }
  return true;
}

void EdenServiceHandler::debugOutstandingThriftRequests(
    std::vector<ThriftRequestMetadata>& outstandingRequests) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2);

  const auto requestsLockedPtr = outstandingThriftRequests_.rlock();
  for (const auto& item : *requestsLockedPtr) {
    outstandingRequests.emplace_back(
        populateThriftRequestMetadata(item.second));
  }
}

void EdenServiceHandler::debugOutstandingHgEvents(
    std::vector<HgEvent>& outstandingEvents,
    std::unique_ptr<std::string> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2);

  auto mountHandle = lookupMount(mountPoint);

  auto backingStore = mountHandle.getObjectStore().getBackingStore();
  std::shared_ptr<SaplingBackingStore> saplingBackingStore =
      castToSaplingBackingStore(
          backingStore, mountHandle.getEdenMount().getPath());
  const auto hgEvents = saplingBackingStore->getOutstandingHgEvents();

  auto processInfoCache =
      mountHandle.getEdenMount().getServerState()->getProcessInfoCache();
  for (const auto& event : hgEvents) {
    HgEvent thriftEvent;
    convertHgImportTraceEventToHgEvent(event, *processInfoCache, thriftEvent);
    outstandingEvents.emplace_back(thriftEvent);
  }
}

void EdenServiceHandler::debugStartRecordingActivity(
    ActivityRecorderResult& result,
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::string> outputDir) {
  AbsolutePathPiece path;
  try {
    path = absolutePathFromThrift(*outputDir);
  } catch (const std::exception&) {
    throw newEdenError(
        EINVAL,
        EdenErrorType::ARGUMENT_ERROR,
        "path for output directory is invalid");
  }

  auto mountHandle = lookupMount(mountPoint);
  auto lockedPtr = mountHandle.getEdenMount().getActivityRecorder().wlock();
  // bool check on the wrapped pointer as lockedPtr is truthy as long
  // as we have the lock
  if (!lockedPtr->get()) {
    auto recorder =
        server_->makeActivityRecorder(mountHandle.getEdenMountPtr());
    lockedPtr->swap(recorder);
  }
  uint64_t unique = lockedPtr->get()->addSubscriber(path);
  // unique_ref is signed but overflow is very unlikely because unique is UNIX
  // timestamp in seconds.
  result.unique() = unique;
}

void EdenServiceHandler::debugStopRecordingActivity(
    ActivityRecorderResult& result,
    std::unique_ptr<std::string> mountPoint,
    int64_t unique) {
  auto mountHandle = lookupMount(mountPoint);
  auto lockedPtr = mountHandle.getEdenMount().getActivityRecorder().wlock();
  auto* activityRecorder = lockedPtr->get();
  if (!activityRecorder) {
    return;
  }

  auto outputPath = activityRecorder->removeSubscriber(unique);
  if (outputPath.has_value()) {
    result.unique() = unique;
    result.path() = outputPath.value();
  }

  if (activityRecorder->getSubscribers().empty()) {
    lockedPtr->reset();
  }
}

void EdenServiceHandler::debugListActivityRecordings(
    ListActivityRecordingsResult& result,
    std::unique_ptr<std::string> mountPoint) {
  auto mountHandle = lookupMount(mountPoint);
  auto lockedPtr = mountHandle.getEdenMount().getActivityRecorder().rlock();
  auto* activityRecorder = lockedPtr->get();
  if (!activityRecorder) {
    return;
  }

  std::vector<ActivityRecorderResult> recordings;
  auto subscribers = activityRecorder->getSubscribers();
  recordings.reserve(subscribers.size());
  for (auto const& subscriber : subscribers) {
    ActivityRecorderResult recording;
    recording.unique() = std::get<0>(subscriber);
    recording.path() = std::get<1>(subscriber);
    recordings.push_back(std::move(recording));
  }
  result.recordings() = recordings;
}

void EdenServiceHandler::debugGetInodePath(
    InodePathDebugInfo& info,
    std::unique_ptr<std::string> mountPoint,
    int64_t inodeNumber) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);
  auto inodeNum = static_cast<InodeNumber>(inodeNumber);
  auto mountHandle = lookupMount(mountPoint);
  auto inodeMap = mountHandle.getEdenMount().getInodeMap();

  auto relativePath = inodeMap->getPathForInode(inodeNum);
  // Check if the inode is loaded
  info.loaded() = inodeMap->lookupLoadedInode(inodeNum) != nullptr;
  // If getPathForInode returned none then the inode is unlinked
  info.linked() = relativePath != std::nullopt;
  if (relativePath) {
    info.path() = relativePath->asString();
  } else {
    info.path() = "";
  }
}

void EdenServiceHandler::clearFetchCounts() {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);

  for (auto& handle : server_->getMountPoints()) {
    handle.getObjectStore().clearFetchCounts();
  }
}

void EdenServiceHandler::clearFetchCountsByMount(
    std::unique_ptr<std::string> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);
  auto mount = lookupMount(mountPoint);
  mount.getObjectStore().clearFetchCounts();
}

void EdenServiceHandler::startRecordingBackingStoreFetch() {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);
  for (auto& backingStore : server_->getBackingStores()) {
    backingStore->startRecordingFetch();
  }
}

void EdenServiceHandler::stopRecordingBackingStoreFetch(
    GetFetchedFilesResult& results) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);
  for (const auto& backingStore : server_->getBackingStores()) {
    auto filePaths = backingStore->stopRecordingFetch();

    std::shared_ptr<SaplingBackingStore> saplingBackingStore{nullptr};

    // If FilteredFS is enabled, we'll see a FilteredBackingStore first
    auto filteredBackingStore =
        std::dynamic_pointer_cast<FilteredBackingStore>(backingStore);
    if (filteredBackingStore) {
      // FilteredBackingStore -> SaplingBackingStore
      saplingBackingStore = std::dynamic_pointer_cast<SaplingBackingStore>(
          filteredBackingStore->getBackingStore());
    } else {
      // BackingStore -> SaplingBackingStore
      saplingBackingStore =
          std::dynamic_pointer_cast<SaplingBackingStore>(backingStore);
    }

    // recording is only implemented for SaplingBackingStore at the moment
    if (saplingBackingStore) {
      (*results.fetchedFilePaths())["SaplingBackingStore"].insert(
          filePaths.begin(), filePaths.end());
    }
  }
} // namespace eden

void EdenServiceHandler::getAccessCounts(
    GetAccessCountsResult& result,
    int64_t duration) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);

  result.cmdsByPid() =
      server_->getServerState()->getProcessInfoCache()->getAllProcessNames();

  auto seconds = std::chrono::seconds{duration};

  for (auto& handle : server_->getMountPoints()) {
    auto& mount = handle.getEdenMount();
    auto& mountStr = mount.getPath().value();
    auto& pal = mount.getProcessAccessLog();

    auto& pidFetches = mount.getObjectStore()->getPidFetches();

    MountAccesses& ma = result.accessesByMount()[mountStr];
    for (auto& [pid, accessCounts] : pal.getAccessCounts(seconds)) {
      ma.accessCountsByPid()[pid] = accessCounts;
    }

    auto pidFetchesLockedPtr = pidFetches.rlock();
    for (auto& [pid, fetchCount] : *pidFetchesLockedPtr) {
      ma.fetchCountsByPid()[pid.get()] = fetchCount;
    }
  }
}

// TODO(T119221752): add more BackingStore subclasses to this command. We
// currently only support SaplingBackingStores
int64_t EdenServiceHandler::debugDropAllPendingRequests() {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG1);
  auto stores = server_->getSaplingBackingStores();
  int64_t numDropped = 0;
  for (auto& store : stores) {
    numDropped += store->dropAllPendingRequestsFromQueue();
  }
  return numDropped;
}

int64_t EdenServiceHandler::unloadInodeForPath(
    [[maybe_unused]] unique_ptr<string> mountPoint,
    [[maybe_unused]] std::unique_ptr<std::string> path,
    [[maybe_unused]] std::unique_ptr<TimeSpec> age) {
#ifndef _WIN32
  auto helper = INSTRUMENT_THRIFT_CALL(DBG1, *mountPoint, *path);
  auto mountHandle = lookupMount(mountPoint);

  TreeInodePtr inode =
      inodeFromUserPath(
          mountHandle.getEdenMount(), *path, helper->getFetchContext())
          .asTreePtr();
  auto cutoff = std::chrono::system_clock::now() -
      std::chrono::seconds(*age->seconds()) -
      std::chrono::nanoseconds(*age->nanoSeconds());
  auto cutoff_ts = folly::to<timespec>(cutoff);
  return inode->unloadChildrenLastAccessedBefore(cutoff_ts);
#else
  NOT_IMPLEMENTED();
#endif
}

folly::SemiFuture<std::unique_ptr<DebugInvalidateResponse>>
EdenServiceHandler::semifuture_debugInvalidateNonMaterialized(
    std::unique_ptr<DebugInvalidateRequest> params) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG1, *params->mount()->mountPoint());
  auto mountHandle = lookupMount(params->mount()->mountPoint());
  auto& fetchContext = helper->getFetchContext();

  // TODO: We may need to restrict 0s age as that can lead to
  // weird behavior where files are invalidated while being read causing the
  // read to fail.

  auto cutoff = std::chrono::system_clock::time_point::max();
  if (*params->age()->seconds() != 0) {
    cutoff = std::chrono::system_clock::now() -
        std::chrono::seconds(*params->age()->seconds());
  }

  ImmediateFuture<folly::Unit> backgroundFuture{std::in_place};
  if (*params->background()) {
    helper->markBackground();
    backgroundFuture = makeNotReadyImmediateFuture();
  }

  auto invalFut =
      std::move(backgroundFuture)
          .thenValue([mountHandle, sync = *params->sync()](auto&&) {
            return waitForPendingWrites(mountHandle.getEdenMount(), sync);
          })
          .thenValue([mountHandle,
                      path = *params->path(),
                      fetchContext = fetchContext.copy()](auto&&) {
            return inodeFromUserPath(
                       mountHandle.getEdenMount(), path, fetchContext)
                .asTreePtr();
          })
          .thenValue([this, mountHandle, cutoff](TreeInodePtr inode) {
            auto pressureBased = mountHandle.getEdenMount()
                                     .getEdenConfig()
                                     ->enablePressureBasedGc.getValue();
            return server_->garbageCollectInodes(
                mountHandle.getEdenMount(),
                inode,
                cutoff,
                ObjectFetchContext::getNullInodeGcContext(),
                pressureBased);
          })
          .thenValue([](uint64_t numInvalidated) {
            auto ret = std::make_unique<DebugInvalidateResponse>();
            ret->numInvalidated() = numInvalidated;
            return ret;
          })
          .ensure([helper = std::move(helper), mountHandle] {});

  if (!*params->background()) {
    return std::move(invalFut).semi();
  } else {
    folly::futures::detachOn(
        server_->getServerState()->getThreadPool().get(),
        std::move(invalFut).semi());
    return std::make_unique<DebugInvalidateResponse>();
  }
}

folly::coro::Task<std::unique_ptr<GetFileContentResponse>>
EdenServiceHandler::co_getFileContent(
    apache::thrift::RequestParams params,
    std::unique_ptr<GetFileContentRequest> request) {
  // Read from request
  auto sync = request->sync();
  auto mountPoint = request->mount()->mountPoint();
  auto filePath = request->filePath();

  // Set up log helper
  auto requestContext = params.getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(
      DBG3,
      false,
      requestContext,
      *mountPoint,
      getSyncTimeout(*sync),
      *filePath);

  // Prepare params for querying
  auto mountHandle = lookupMount(mountPoint);
  auto path = RelativePathPiece(*filePath);
  auto& fetchContext = helper->getFetchContext();

  ScmBlobOrError blobOrError;
  // Ensure Eden has its internal state updated.
  // See SyncBehavior struct in eden.thrift for details.
  auto waitTry = co_await co_awaitTry(
      co_waitForPendingWrites(mountHandle.getEdenMount(), *request->sync()));
  if (waitTry.hasException()) {
    blobOrError.error() = newEdenError(waitTry.exception());
  } else {
    auto& edenMount = mountHandle.getEdenMount();
    auto inodeTry =
        co_await co_awaitTry(edenMount.co_getVirtualInode(path, fetchContext));
    if (inodeTry.hasException()) {
      blobOrError.error() = newEdenError(inodeTry.exception());
    } else {
      auto& objectStore = mountHandle.getObjectStorePtr();
      auto blobTry = co_await co_awaitTry(
          inodeTry.value().co_getBlob(objectStore, fetchContext));
      if (blobTry.hasException()) {
        blobOrError.error() = newEdenError(blobTry.exception());
      } else {
        // Return error if the binary size exceeds 2GB limit.
        // Enforced by CompactProtocolWriter in the Thrift
        // https://github.com/facebook/fbthrift/blob/main/thrift/lib/cpp2/protocol/CompactProtocol-inl.h
        const auto blobSize = blobTry.value().size();
        if (blobSize > std::numeric_limits<int32_t>::max()) {
          blobOrError.error() = newEdenError(
              EFBIG,
              EdenErrorType::POSIX_ERROR,
              "Thrift size limit (2GB) exceeded by file: ",
              path);
        } else {
          blobOrError.blob() = std::move(blobTry.value());
        }
      }
    }
  }
  auto response = std::make_unique<GetFileContentResponse>();
  response->blob() = std::move(blobOrError);
  co_return response;
}

folly::coro::Task<std::unique_ptr<::facebook::eden::GetActiveRequestsResponse>>
EdenServiceHandler::co_getActiveRequests(apache::thrift::RequestParams params) {
  auto requestContext = params.getRequestContext();
  auto helper =
      INSTRUMENT_THRIFT_CALL_WITH_CANCELLATION(DBG3, false, requestContext);

  XLOG(DBG3) << "getActiveRequests called";

  auto response = std::make_unique<GetActiveRequestsResponse>();
  std::vector<ActiveRequestInfo> activeRequests;

  auto currentRequestId = helper->getRequestId();
  auto lockedStore = requestCancellationStore_.rlock();
  for (const auto& [requestId, cancellationInfo] : *lockedStore) {
    // Skip our own request
    if (requestId == currentRequestId) {
      continue;
    }

    ActiveRequestInfo requestInfo;
    requestInfo.requestId() = requestId;
    requestInfo.cancelable() = cancellationInfo.isCancelable();

    std::optional<std::string_view> method;
    std::optional<pid_t> reqClientPid;
    std::chrono::system_clock::time_point systemTime;
    {
      auto lockedOutstanding = outstandingThriftRequests_.rlock();
      auto it = lockedOutstanding->find(requestId);
      if (it != lockedOutstanding->end()) {
        method = it->second.method;
        reqClientPid = it->second.clientPid
            ? std::optional<pid_t>(it->second.clientPid.value().get())
            : std::nullopt;
        systemTime = it->second.systemTime;
      }
    }

    if (method.has_value()) {
      requestInfo.method() = std::string(method.value());
      requestInfo.clientPid() = reqClientPid.value_or(0);

      requestInfo.startTimeNs() =
          std::chrono::duration_cast<std::chrono::nanoseconds>(
              systemTime.time_since_epoch())
              .count();

      if (reqClientPid) {
        auto processInfoCache =
            server_->getServerState()->getProcessInfoCache();
        auto processName =
            processInfoCache->getProcessName(reqClientPid.value());
        if (processName.has_value() && !processName->empty()) {
          requestInfo.processName() = *processName;
        }
      }
    } else {
      requestInfo.method() = "unknown";
      requestInfo.clientPid() = 0;
    }

    activeRequests.push_back(std::move(requestInfo));
  }

  response->requests() = std::move(activeRequests);
  XLOG(DBG3) << "getActiveRequests returning " << response->requests()->size()
             << " active requests";

  co_return response;
}

void EdenServiceHandler::listRedirections(
    ListRedirectionsResponse& response,
    std::unique_ptr<ListRedirectionsRequest> request) {
  auto mountId = request->mount();
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountId);

  const auto& configDir = server_->getEdenDir();
  const auto& edenEtcDir =
      server_->getServerState()->getEdenConfig()->getSystemConfigDir();

  auto redirsFFI = list_redirections(
      absolutePathFromThrift(*mountId->mountPoint()).stringWithoutUNC(),
      configDir.stringWithoutUNC(),
      edenEtcDir.stringWithoutUNC());

  std::vector<Redirection> redirs(redirsFFI.size());
  std::transform(
      redirsFFI.begin(), redirsFFI.end(), redirs.begin(), [](auto&& redirFFI) {
        return redirectionFromFFI(std::move(redirFFI));
      });

  response.redirections() = std::move(redirs);
}

void EdenServiceHandler::getStatInfo(
    InternalStats& result,
    std::unique_ptr<GetStatInfoParams> params) {
  int64_t statsMask = *params->statsMask();
  // return all stats when mask not provided
  // TODO: remove when no old clients exists
  if (0 == statsMask) {
    statsMask = ~0;
  }

  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);

  if (statsMask & eden_constants::STATS_MOUNTS_STATS_) {
    auto mountList = server_->getMountPoints();
    std::map<PathString, MountInodeInfo> mountPointInfo = {};
    std::map<PathString, JournalInfo> mountPointJournalInfo = {};
    for (auto& handle : mountList) {
      auto& mount = handle.getEdenMount();
      auto inodeMap = mount.getInodeMap();
      // Set LoadedInde Count and unloaded Inode count for the mountPoint.
      MountInodeInfo mountInodeInfo;
      auto counts = inodeMap->getInodeCounts();
      mountInodeInfo.unloadedInodeCount() = counts.unloadedInodeCount;
      mountInodeInfo.loadedFileCount() = counts.fileCount;
      mountInodeInfo.loadedTreeCount() = counts.treeCount;

      JournalInfo journalThrift;
      if (auto journalStats = mount.getJournal().getStats()) {
        journalThrift.entryCount() = journalStats->entryCount;
        journalThrift.durationSeconds() = journalStats->getDurationInSeconds();
      } else {
        journalThrift.entryCount() = 0;
        journalThrift.durationSeconds() = 0;
      }
      journalThrift.memoryUsage() = mount.getJournal().estimateMemoryUsage();

      auto mountPath = absolutePathToThrift(mount.getPath());
      mountPointJournalInfo[mountPath] = journalThrift;

      mountPointInfo[mountPath] = mountInodeInfo;
    }
    result.mountPointInfo() = mountPointInfo;
    result.mountPointJournalInfo() = mountPointJournalInfo;
  }

  fb303::ThreadCachedServiceData::get()->publishStats();
  auto counters = fb303::ServiceData::get()->getCounters();
  if (statsMask & eden_constants::STATS_COUNTERS_) {
    // Get the counters and set number of inodes unloaded by periodic unload
    // job.
    result.counters() = counters;
    size_t periodicUnloadCount{0};
    for (auto& handle : server_->getMountPoints()) {
      auto& mount = handle.getEdenMount();
      periodicUnloadCount +=
          counters[mount.getCounterName(CounterName::PERIODIC_INODE_UNLOAD)];
    }

    result.periodicUnloadCount() = periodicUnloadCount;
  }

  if (statsMask & eden_constants::STATS_PRIVATE_BYTES_) {
    auto privateDirtyBytes = facebook::eden::proc_util::calculatePrivateBytes();
    if (privateDirtyBytes) {
      result.privateBytes() = privateDirtyBytes.value();
    }
  }

  if (statsMask & eden_constants::STATS_RSS_BYTES_) {
    auto memoryStats = facebook::eden::proc_util::readMemoryStats();
    if (memoryStats) {
      result.vmRSSBytes() = memoryStats->resident;
    }
  }

  if (statsMask & eden_constants::STATS_SMAPS_) {
    // Note: this will be removed in a subsequent commit.
    // We now report periodically via ServiceData
    std::string smaps;
    if (folly::readFile("/proc/self/smaps", smaps)) {
      result.smaps() = std::move(smaps);
    }
  }

  if (statsMask & eden_constants::STATS_CACHE_STATS_) {
    const auto blobCacheStats = server_->getBlobCache()->getStats(counters);
    result.blobCacheStats() = CacheStats{};
    result.blobCacheStats()->entryCount() = blobCacheStats.objectCount;
    result.blobCacheStats()->totalSizeInBytes() =
        blobCacheStats.totalSizeInBytes;
    result.blobCacheStats()->hitCount() = blobCacheStats.hitCount;
    result.blobCacheStats()->missCount() = blobCacheStats.missCount;
    result.blobCacheStats()->evictionCount() = blobCacheStats.evictionCount;
    result.blobCacheStats()->dropCount() = blobCacheStats.dropCount;

    const auto treeCacheStats = server_->getTreeCache()->getStats(counters);
    result.treeCacheStats() = CacheStats{};
    result.treeCacheStats()->entryCount() = treeCacheStats.objectCount;
    result.treeCacheStats()->totalSizeInBytes() =
        treeCacheStats.totalSizeInBytes;
    result.treeCacheStats()->hitCount() = treeCacheStats.hitCount;
    result.treeCacheStats()->missCount() = treeCacheStats.missCount;
    result.treeCacheStats()->evictionCount() = treeCacheStats.evictionCount;
    result.treeCacheStats()->dropCount() = treeCacheStats.dropCount;
  }
}

void EdenServiceHandler::flushStatsNow() {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);
  server_->flushStatsNow();
}

void EdenServiceHandler::getCounters(std::map<std::string, int64_t>& result) {
  fb303::ThreadCachedServiceData::get()->publishStats();
  fb303::BaseService::getCounters(result);
}

void EdenServiceHandler::getRegexCounters(
    std::map<std::string, int64_t>& result,
    std::unique_ptr<std::string> regex) {
  fb303::ThreadCachedServiceData::get()->publishStats();
  fb303::BaseService::getRegexCounters(result, std::move(regex));
}

void EdenServiceHandler::getSelectedCounters(
    std::map<std::string, int64_t>& result,
    std::unique_ptr<std::vector<std::string>> keys) {
  fb303::ThreadCachedServiceData::get()->publishStats();
  fb303::BaseService::getSelectedCounters(result, std::move(keys));
}

int64_t EdenServiceHandler::getCounter(std::unique_ptr<std::string> key) {
  fb303::ThreadCachedServiceData::get()->publishStats();
  return fb303::BaseService::getCounter(std::move(key));
}

folly::SemiFuture<Unit>
EdenServiceHandler::semifuture_invalidateKernelInodeCache(
    [[maybe_unused]] std::unique_ptr<std::string> mountPoint,
    [[maybe_unused]] std::unique_ptr<std::string> path) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2, *mountPoint, *path);
  auto mountHandle = lookupMount(mountPoint);
#ifndef _WIN32
  InodePtr inode = inodeFromUserPath(
      mountHandle.getEdenMount(), *path, helper->getFetchContext());

  if (auto* fuseChannel = mountHandle.getEdenMount().getFuseChannel()) {
    // Invalidate cached pages and attributes
    fuseChannel->invalidateInode(inode->getNodeId(), 0, 0);

    const auto treePtr = inode.asTreePtrOrNull();

    // Invalidate all parent/child relationships potentially cached.
    if (treePtr != nullptr) {
      auto contents = treePtr->lockContentsRead();
      for (const auto& entry : contents->entries.all()) {
        fuseChannel->invalidateEntry(inode->getNodeId(), entry.first);
      }
    }

    // Wait for all of the invalidations to complete
    return fuseChannel->completeInvalidations().semi();
  }

  if (auto* nfsChannel = mountHandle.getEdenMount().getNfsdChannel()) {
    inode->forceMetadataUpdate();
    auto& fetchContext = helper->getFetchContext();
    auto rawInodePtr = inode.get();
    return wrapImmediateFuture(
               std::move(helper),
               rawInodePtr->stat(fetchContext)
                   .thenValue(
                       [nfsChannel,
                        canonicalMountPoint =
                            absolutePathFromThrift(*mountPoint),
                        inode = std::move(inode),
                        path = std::move(path),
                        mountHandle,
                        fetchContext =
                            fetchContext.copy()](struct stat&& stat) mutable
                           -> ImmediateFuture<folly::Unit> {
                         nfsChannel->invalidate(
                             canonicalMountPoint + RelativePath{*path},
                             stat.st_mode);
                         const auto treePtr = inode.asTreePtrOrNull();
                         // Invalidate all children as well. There isn't
                         // really a way to invalidate the entry cache for nfs
                         // so we settle for invalidating the children
                         // themselves.
                         if (treePtr != nullptr) {
                           // Collect the child names and release the contents
                           // lock before loading any children: loading an
                           // unloaded child acquires the write lock on the
                           // same contents_ lock, so holding the read lock
                           // across the loads self-deadlocks and permanently
                           // wedges the directory.
                           std::vector<PathComponent> childNames;
                           {
                             auto contents = treePtr->lockContentsRead();
                             childNames.reserve(contents->entries.size());
                             for (const auto& entry : contents->entries.all()) {
                               childNames.push_back(entry.first);
                             }
                           }
                           std::vector<ImmediateFuture<folly::Unit>>
                               childInvalidations{};
                           for (const auto& name : childNames) {
                             auto childPath = RelativePath{*path} + name;
                             // The entries may change once the lock is
                             // released; a child that fails to load is simply
                             // not invalidated, matching how per-child stat
                             // errors are already discarded below.
                             childInvalidations.push_back(
                                 makeImmediateFutureWith(
                                     [&]() -> ImmediateFuture<folly::Unit> {
                                       auto childInode = inodeFromUserPath(
                                           mountHandle.getEdenMount(),
                                           childPath.asString(),
                                           fetchContext);
                                       childInode->forceMetadataUpdate();
                                       return childInode->stat(fetchContext)
                                           .thenValue(
                                               [nfsChannel,
                                                canonicalMountPoint,
                                                childPath](struct stat&& stat) {
                                                 nfsChannel->invalidate(
                                                     canonicalMountPoint +
                                                         childPath,
                                                     stat.st_mode);
                                                 return folly::Unit();
                                               });
                                     }));
                           }
                           return collectAll(std::move(childInvalidations))
                               .unit();
                         }
                         return folly::unit;
                       })
                   .thenTry([nfsChannel](folly::Try<folly::Unit> res) {
                     return nfsChannel->completeInvalidations().thenTry(
                         [res = std::move(res)](auto&&) mutable {
                           return res;
                         });
                   }))
        .semi();
  }
#else
  auto toInvalidate = relpathFromUserPath(*path);

  XLOGF(
      WARN,
      "Manually invalidating \"{}\". This is unsupported and may lead to strange behavior.",
      toInvalidate);
  if (auto* prjfsChannel = mountHandle.getEdenMount().getPrjfsChannel()) {
    return makeImmediateFutureWith(
               [&] { return prjfsChannel->removeCachedFile(toInvalidate); })
        .semi();
  }
#endif // !_WIN32

  return EDEN_BUG_FUTURE(folly::Unit) << "Unsupported Channel type.";
}

void EdenServiceHandler::enableTracing() {
  XLOG(INFO, "Enabling tracing");
  eden::enableTracing();
}
void EdenServiceHandler::disableTracing() {
  XLOG(INFO, "Disabling tracing");
  eden::disableTracing();
}

void EdenServiceHandler::getTracePoints(std::vector<TracePoint>& result) {
  auto compactTracePoints = getAllTracepoints();
  for (auto& point : compactTracePoints) {
    TracePoint tp;
    tp.timestamp() = point.timestamp.count();
    tp.traceId() = point.traceId;
    tp.blockId() = point.blockId;
    tp.parentBlockId() = point.parentBlockId;
    if (point.name) {
      tp.name() = std::string(point.name);
    }
    if (point.start) {
      tp.event() = TracePointEvent::START;
    } else if (point.stop) {
      tp.event() = TracePointEvent::STOP;
    }
    result.emplace_back(std::move(tp));
  }
}

void EdenServiceHandler::getRetroactiveThriftRequestEvents(
    GetRetroactiveThriftRequestEventsResult& result) {
  if (!thriftRequestActivityBuffer_.has_value()) {
    throw newEdenError(
        ENOTSUP,
        EdenErrorType::POSIX_ERROR,
        "ActivityBuffer not initialized in thrift server.");
  }

  std::vector<ThriftRequestEvent> thriftEvents;
  auto bufferEvents = thriftRequestActivityBuffer_->getAllEvents();
  thriftEvents.reserve(bufferEvents.size());
  for (auto const& event : bufferEvents) {
    ThriftRequestEvent thriftEvent;
    convertThriftRequestTraceEventToThriftRequestEvent(event, thriftEvent);
    thriftEvents.emplace_back(std::move(thriftEvent));
  }

  result.events() = std::move(thriftEvents);
}

void EdenServiceHandler::getRetroactiveHgEvents(
    GetRetroactiveHgEventsResult& result,
    std::unique_ptr<GetRetroactiveHgEventsParams> params) {
  auto mountHandle = lookupMount(params->mountPoint());
  auto backingStore = mountHandle.getObjectStore().getBackingStore();
  std::shared_ptr<SaplingBackingStore> saplingBackingStore =
      castToSaplingBackingStore(
          backingStore, mountHandle.getEdenMount().getPath());

  std::vector<HgEvent> thriftEvents;
  auto bufferEvents = saplingBackingStore->getActivityBuffer().getAllEvents();
  thriftEvents.reserve(bufferEvents.size());
  for (auto const& event : bufferEvents) {
    HgEvent thriftEvent{};
    convertHgImportTraceEventToHgEvent(
        event, *server_->getServerState()->getProcessInfoCache(), thriftEvent);
    thriftEvents.emplace_back(std::move(thriftEvent));
  }

  result.events() = std::move(thriftEvents);
}

void EdenServiceHandler::getRetroactiveInodeEvents(
    GetRetroactiveInodeEventsResult& result,
    std::unique_ptr<GetRetroactiveInodeEventsParams> params) {
  auto mountHandle = lookupMount(params->mountPoint());

  if (!mountHandle.getEdenMount().getActivityBuffer().has_value()) {
    throw newEdenError(
        ENOTSUP,
        EdenErrorType::POSIX_ERROR,
        "ActivityBuffer not initialized in EdenFS mount.");
  }

  std::vector<InodeEvent> thriftEvents;
  auto bufferEvents =
      mountHandle.getEdenMount().getActivityBuffer()->getAllEvents();
  thriftEvents.reserve(bufferEvents.size());
  for (auto const& event : bufferEvents) {
    InodeEvent thriftEvent{};
    ConvertInodeTraceEventToThriftInodeEvent(event, thriftEvent);
    thriftEvent.path() = event.getPath();
    thriftEvents.emplace_back(std::move(thriftEvent));
  }

  result.events() = std::move(thriftEvents);
}

namespace {
std::optional<folly::exception_wrapper> getFaultError(
    apache::thrift::optional_field_ref<std::string&> errorType,
    apache::thrift::optional_field_ref<std::string&> errorMessage) {
  if (!errorType.has_value() && !errorMessage.has_value()) {
    return std::nullopt;
  }

  auto createException =
      [](StringPiece type, const std::string& msg) -> folly::exception_wrapper {
    if (type == "runtime_error") {
      return std::runtime_error(msg);
    } else if (type.startsWith("errno:")) {
      auto errnum = folly::to<int>(type.subpiece(6));
      return std::system_error(errnum, std::generic_category(), msg);
    } else if (type == "quiet") {
      return QuietFault(msg);
    }
    // If we want to support other error types in the future they should
    // be added here.
    throw newEdenError(
        EdenErrorType::GENERIC_ERROR, "unknown error type ", type);
  };

  return createException(
      errorType.value_or("runtime_error"),
      errorMessage.value_or("injected error"));
}
} // namespace

void EdenServiceHandler::injectFault(unique_ptr<FaultDefinition> fault) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2);
  validateInjectFaultDefinition(*fault);
  auto& injector = server_->getServerState()->getFaultInjector();
  if (*fault->block()) {
    injector.injectBlock(
        *fault->keyClass(), *fault->keyValueRegex(), *fault->count());
    return;
  }
  if (*fault->blockWithCancel()) {
    // At the time of injection we do not have the actual request's cancellation
    // token. This fault needs to call updateBlockWithCancelToken to update the
    // token.
    folly::CancellationSource placeholderSource;
    std::string blockWithCancelKeyClass = *fault->keyClass();
    injector.injectBlockWithCancel(
        blockWithCancelKeyClass,
        *fault->keyValueRegex(),
        placeholderSource.getToken(),
        std::chrono::milliseconds{5000}, // 5 second timeout
        *fault->count());
    return;
  }
  if (*fault->kill()) {
    injector.injectKill(
        *fault->keyClass(), *fault->keyValueRegex(), *fault->count());
    return;
  }
  if (fault->unblockKeyClass().has_value() &&
      fault->unblockKeyValueRegex().has_value()) {
    injector.injectUnblock(
        *fault->keyClass(),
        *fault->keyValueRegex(),
        *fault->unblockKeyClass(),
        *fault->unblockKeyValueRegex(),
        *fault->count());
    return;
  }

  auto error = getFaultError(fault->errorType(), fault->errorMessage());
  std::chrono::milliseconds delay(*fault->delayMilliseconds());
  if (error.has_value()) {
    if (delay.count() > 0) {
      injector.injectDelayedError(
          *fault->keyClass(),
          *fault->keyValueRegex(),
          delay,
          error.value(),
          *fault->count());
    } else {
      injector.injectError(
          *fault->keyClass(),
          *fault->keyValueRegex(),
          error.value(),
          *fault->count());
    }
  } else {
    if (delay.count() > 0) {
      injector.injectDelay(
          *fault->keyClass(), *fault->keyValueRegex(), delay, *fault->count());
    } else {
      injector.injectNoop(
          *fault->keyClass(), *fault->keyValueRegex(), *fault->count());
    }
  }
}

bool EdenServiceHandler::removeFault(unique_ptr<RemoveFaultArg> fault) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2);
  auto& injector = server_->getServerState()->getFaultInjector();
  return injector.removeFault(*fault->keyClass(), *fault->keyValueRegex());
}

int64_t EdenServiceHandler::unblockFault(unique_ptr<UnblockFaultArg> info) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2);
  auto& injector = server_->getServerState()->getFaultInjector();
  auto error = getFaultError(info->errorType(), info->errorMessage());

  if (!info->keyClass().has_value()) {
    if (info->keyValueRegex().has_value()) {
      throw newEdenError(
          EINVAL,
          EdenErrorType::ARGUMENT_ERROR,
          "cannot specify a key value regex without a key class");
    }
    if (error.has_value()) {
      return injector.unblockAllWithError(error.value());
    } else {
      return injector.unblockAll();
    }
  }

  const auto& keyClass = info->keyClass().value();
  std::string keyValueRegex = info->keyValueRegex().value_or(".*");
  if (error.has_value()) {
    return injector.unblockWithError(keyClass, keyValueRegex, error.value());
  } else {
    return injector.unblock(keyClass, keyValueRegex);
  }
}

void EdenServiceHandler::getBlockedFaults(
    GetBlockedFaultsResponse& out,
    std::unique_ptr<GetBlockedFaultsRequest> request) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2);
  auto& injector = server_->getServerState()->getFaultInjector();
  auto result = injector.getBlockedFaults(*request->keyclass());

  out.keyValues() = std::move(result);
}

void EdenServiceHandler::reloadConfig() {
  auto helper = INSTRUMENT_THRIFT_CALL(INFO);
  server_->reloadConfig();
}

void EdenServiceHandler::fillDaemonInfo(DaemonInfo& info) {
  fb303::cpp2::fb303_status status = [&] {
    switch (server_->getStatus()) {
      case EdenServer::RunState::STARTING:
        return facebook::fb303::cpp2::fb303_status::STARTING;
      case EdenServer::RunState::RUNNING:
        return facebook::fb303::cpp2::fb303_status::ALIVE;
      case EdenServer::RunState::SHUTTING_DOWN:
        return facebook::fb303::cpp2::fb303_status::STOPPING;
    }
    EDEN_BUG() << "unexpected EdenServer status "
               << enumValue(server_->getStatus());
  }();

  info.pid() = ProcessId::current().get();
  info.commandLine() = originalCommandLine_;
  info.status() = status;

  auto now = std::chrono::steady_clock::now();
  std::chrono::duration<float> uptime = now - server_->getStartTime();
  info.uptime() = uptime.count();
}

void EdenServiceHandler::getDaemonInfo(DaemonInfo& result) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG4);
  fillDaemonInfo(result);
}

apache::thrift::ResponseAndServerStream<DaemonInfo, std::string>
EdenServiceHandler::streamStartStatus() {
  DaemonInfo result;
  fillDaemonInfo(result);

  if (result.status() != facebook::fb303::cpp2::fb303_status::STARTING) {
    return {
        result,
        apache::thrift::ServerStream<EdenStartStatusUpdate>::createEmpty()};
  }
  try {
    auto serverStream = server_->createStartupStatusThriftStream();
    return {std::move(result), std::move(serverStream)};
  } catch (EdenError& error) {
    if (error.errorType() == EdenErrorType::POSIX_ERROR &&
        error.errorCode() == EALREADY) {
      // We raced with eden start completing. Let's re-collect the status and
      // return as if EdenFS has completed. The EdenFS status should be set
      // before the startup logger completes, so at this point the status
      // should be something other than starting. Client should not
      // necessarily rely on this though.
      fillDaemonInfo(result);
      return {
          result,
          apache::thrift::ServerStream<EdenStartStatusUpdate>::createEmpty()};
    }
    throw;
  }
}

void EdenServiceHandler::checkPrivHelper(PrivHelperInfo& result) {
  auto privhelper = server_->getServerState()->getPrivHelper();
  result.connected() = privhelper->checkConnection();
  result.pid() = privhelper->getPid();
}

int64_t EdenServiceHandler::getPid() {
  return ProcessId::current().get();
}

void EdenServiceHandler::getCheckoutProgressInfo(
    CheckoutProgressInfoResponse& ret,
    unique_ptr<CheckoutProgressInfoRequest> params) {
  auto mountPath = absolutePathFromThrift(*params->mountPoint());
  auto mountHandle = server_->getMount(mountPath);
  auto& mount = mountHandle.getEdenMount();
  auto checkoutProgress = mount.getCheckoutProgress();
  if (checkoutProgress.has_value()) {
    CheckoutProgressInfo progressInfoRet;
    auto counts = mount.getInodeMap()->getInodeCounts();
    auto totalInodes =
        counts.unloadedInodeCount + counts.fileCount + counts.treeCount;

    progressInfoRet.totalInodes() = totalInodes;

    progressInfoRet.updatedInodes() = std::move(checkoutProgress.value());
    ret.checkoutProgressInfo() = std::move(progressInfoRet);
  } else {
    ret.set_noProgress();
  }
}

void EdenServiceHandler::initiateShutdown(std::unique_ptr<std::string> reason) {
  auto helper = INSTRUMENT_THRIFT_CALL(INFO);
  XLOGF(INFO, "initiateShutdown requested, reason: {}", *reason);
  server_->stop();
}

void EdenServiceHandler::getConfig(
    EdenConfigData& result,
    unique_ptr<GetConfigParams> params) {
  auto state = server_->getServerState();

  if (params->reload() == ConfigReloadBehavior::ForceReload) {
    state->getReloadableConfig()->maybeReload();
  }

  auto config = state->getEdenConfig();

  result = config->toThriftConfigData();
}

OptionalProcessId EdenServiceHandler::getAndRegisterClientPid(
    apache::thrift::Cpp2RequestContext* requestContext) {
#ifndef _WIN32
  // The Cpp2RequestContext for a thrift request is kept in a thread local
  // on the thread which the request originates. This means this must be run
  // on the Thread in which a thrift request originates.
  //
  // For coroutines, thread-local storage is unreliable due to potential thread
  // switching. Use the explicit request context if supplied.
  auto connectionContext =
      requestContext ? requestContext : getRequestContext();
  // if connectionContext will be a null pointer in an async method, so we
  // need to check for this
  if (connectionContext) {
    if (auto peerCreds = connectionContext->getConnectionContext()
                             ->getPeerEffectiveCreds()) {
      pid_t clientPid = peerCreds->pid;
      server_->getServerState()->getProcessInfoCache()->add(clientPid);
      return ProcessId(clientPid);
    }
  }
  return std::nullopt;
#else
  // Unix domain sockets on Windows don't support peer credentials.
  return std::nullopt;
#endif
}

void EdenServiceHandler::insertCancellationSource(
    uint64_t requestId,
    folly::CancellationSource cancellationSource,
    std::string_view endpoint,
    OptionalProcessId clientPid) {
  auto lockedStore = requestCancellationStore_.wlock();
  lockedStore->emplace(
      requestId,
      RequestCancellationInfo(
          std::move(cancellationSource), endpoint, clientPid));
  XLOGF(
      DBG6,
      "Inserted cancellation source for request ID: {}. Total active: {}",
      requestId,
      lockedStore->size());
}

std::optional<folly::CancellationSource>
EdenServiceHandler::getCancellationSource(uint64_t requestId) {
  auto lockedStore = requestCancellationStore_.rlock();
  auto it = lockedStore->find(requestId);
  if (it != lockedStore->end() && it->second.isCancelable()) {
    return it->second.cancellationSource;
  }
  return std::nullopt;
}

bool EdenServiceHandler::removeCancellationSource(uint64_t requestId) {
  auto lockedStore = requestCancellationStore_.wlock();
  auto it = lockedStore->find(requestId);

  if (it != lockedStore->end()) {
    // Check if cancellation was requested and how long it took to remove
    if (it->second.cancellationRequestedAt.has_value()) {
      auto now = std::chrono::steady_clock::now();
      auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
          now - it->second.cancellationRequestedAt.value());

      bool success = elapsed <=
          std::chrono::seconds(CANCELLATION_VERIFICATION_THRESHOLD_SECONDS);

      if (success) {
        // Request cancelled within threshold - success
        XLOGF(
            DBG3,
            "Request {} ({}) cancelled after {} ms (<= {} second)",
            requestId,
            it->second.endpoint,
            elapsed.count(),
            CANCELLATION_VERIFICATION_THRESHOLD_SECONDS);
        server_->getStats()->increment(&ThriftStats::cancelRequestSuccess);
      } else {
        // Request took too long to cancel (>= threshold) - long running
        XLOGF(
            DBG3,
            "Request {} ({}) cancelled after {} ms (> {} second)",
            requestId,
            it->second.endpoint,
            elapsed.count(),
            CANCELLATION_VERIFICATION_THRESHOLD_SECONDS);
        server_->getStats()->increment(&ThriftStats::cancelRequestLongRunning);
      }

      server_->getServerState()->getEdenFsEventsLogger()->logEvent(
          ThriftCancellation{
              std::string{it->second.endpoint},
              success,
              static_cast<double>(elapsed.count())});
    }

    lockedStore->erase(it);
    XLOGF(
        DBG6,
        "Removed cancellation source for request ID: {}. Remaining active: {}",
        requestId,
        lockedStore->size());
    return true;
  }

  return false;
}

bool EdenServiceHandler::requestCancellation(uint64_t requestId) {
  auto lockedStore = requestCancellationStore_.wlock();
  auto it = lockedStore->find(requestId);
  if (it != lockedStore->end()) {
    bool success = it->second.requestCancellation();
    if (success) {
      XLOGF(DBG3, "Requested cancellation for request ID: {}", requestId);
    } else {
      XLOGF(DBG4, "Request ID {} is not cancelable", requestId);
    }
    return success;
  }
  XLOGF(DBG4, "No cancellation source found for request ID: {}", requestId);
  return false;
}

size_t EdenServiceHandler::getActiveCancellationSourceCount() const {
  auto lockedStore = requestCancellationStore_.rlock();
  return lockedStore->size();
}

void EdenServiceHandler::insertUncancelableRequest(
    uint64_t requestId,
    std::string_view endpoint,
    OptionalProcessId clientPid) {
  auto lockedStore = requestCancellationStore_.wlock();
  lockedStore->emplace(
      requestId,
      RequestCancellationInfo::createUncancelable(endpoint, clientPid));
  XLOGF(
      DBG6,
      "Inserted uncancelable request for request ID: {}. Total active: {}",
      requestId,
      lockedStore->size());
}

void EdenServiceHandler::markRequestAsStream(uint64_t requestId) {
  auto lockedStore = requestCancellationStore_.wlock();
  auto it = lockedStore->find(requestId);
  if (it != lockedStore->end()) {
    it->second.stream = true;
  }
}

void EdenServiceHandler::markRequestAsBackground(uint64_t requestId) {
  auto lockedStore = requestCancellationStore_.wlock();
  auto it = lockedStore->find(requestId);
  if (it != lockedStore->end()) {
    it->second.background = true;
  }
}

void EdenServiceHandler::closeTraceStreams() {
  size_t closed = 0;
  {
    auto lockedStore = requestCancellationStore_.wlock();
    for (auto& [requestId, cancellationInfo] : *lockedStore) {
      if (cancellationInfo.stream && cancellationInfo.requestCancellation()) {
        ++closed;
      }
    }
  }
  XLOGF(DBG3, "Closed {} trace streams", closed);
}

std::vector<EdenServiceHandler::LongRunningRequest>
EdenServiceHandler::getRequestsRunningLongerThan(
    std::chrono::steady_clock::duration minAge) const {
  std::vector<LongRunningRequest> requests;
  const auto now = std::chrono::steady_clock::now();
  {
    auto lockedStore = requestCancellationStore_.rlock();
    for (const auto& [requestId, cancellationInfo] : *lockedStore) {
      auto age = now - cancellationInfo.startedAt;
      if (!cancellationInfo.stream && !cancellationInfo.background &&
          age >= minAge) {
        requests.push_back(
            {requestId,
             std::string{cancellationInfo.endpoint},
             cancellationInfo.clientPid,
             age,
             cancellationInfo.isCancelable()});
      }
    }
  }
  std::sort(requests.begin(), requests.end(), [](auto& a, auto& b) {
    return a.age > b.age;
  });
  return requests;
}

std::optional<RequestStatus> EdenServiceHandler::getRequestStatus(
    uint64_t requestId) {
  auto lockedStore = requestCancellationStore_.rlock();
  auto it = lockedStore->find(requestId);
  if (it != lockedStore->end()) {
    return it->second.status;
  }
  return std::nullopt;
}

std::optional<folly::CancellationToken>
EdenServiceHandler::getCancellationToken(uint64_t requestId) {
  auto lockedStore = requestCancellationStore_.rlock();
  auto it = lockedStore->find(requestId);
  if (it != lockedStore->end() && it->second.isCancelable()) {
    return it->second.cancellationSource->getToken();
  }
  return std::nullopt;
}

void EdenServiceHandler::cancelActiveRequestsAfter(
    std::chrono::nanoseconds delay,
    std::string reason) {
  auto pending = delayedCancellation_.wlock();
  if (pending->timer) {
    pending->timer->cancel();
  }
  const auto generation = ++pending->generation;
  // Runs on the timekeeper thread; cancelling the requests only takes the
  // cancellation store lock and runs their cancellation callbacks. It holds
  // the delayed cancellation lock meanwhile, so once a disarm has returned no
  // cancellation from this timer can still be running.
  pending->timer =
      folly::futures::sleep(
          std::chrono::duration_cast<folly::HighResDuration>(delay))
          .toUnsafeFuture()
          .thenTry(
              [weak = weak_from_this(), generation, reason = std::move(reason)](
                  folly::Try<folly::Unit>&& slept) {
                if (!slept.hasValue()) {
                  return;
                }
                auto self = weak.lock();
                if (!self) {
                  return;
                }
                auto pending = self->delayedCancellation_.wlock();
                if (pending->generation != generation) {
                  return;
                }
                self->cancelAllActiveRequests(reason);
              });
}

void EdenServiceHandler::disarmDelayedCancellation() {
  auto pending = delayedCancellation_.wlock();
  ++pending->generation;
  if (pending->timer) {
    pending->timer->cancel();
    pending->timer.reset();
  }
}

void EdenServiceHandler::cancelAllActiveRequests(std::string_view reason) {
  XLOGF(DBG3, "Cancelling all active Thrift requests: {}", reason);

  size_t requestCount = 0;
  size_t cancelledCount = 0;

  {
    auto lockedStore = requestCancellationStore_.wlock();
    requestCount = lockedStore->size();

    for (auto& [requestId, cancellationInfo] : *lockedStore) {
      if (cancellationInfo.requestCancellation()) {
        ++cancelledCount;
      }
    }
  }

  XLOGF(
      DBG3,
      "Requested cancellation for {} of {} active Thrift requests",
      cancelledCount,
      requestCount);
}

void EdenServiceHandler::logOutstandingThriftRequests(std::string_view reason) {
  // Nothing new is accepted after a stop, so a request that later holds up
  // the Thrift server's drain is one of these. Name them now while the
  // daemon can still write a log.
  constexpr size_t kMaxLogged = 50;
  const auto now = std::chrono::steady_clock::now();
  std::vector<std::string> lines;
  size_t total = 0;
  {
    const auto requests = outstandingThriftRequests_.rlock();
    total = requests->size();
    const auto cancellable = requestCancellationStore_.rlock();
    for (const auto& [requestId, event] : *requests) {
      if (lines.size() >= kMaxLogged) {
        break;
      }
      lines.push_back(
          fmt::format(
              "{}: {} ({}, {}cancellable){}",
              requestId,
              event.method,
              durationStr(now - event.monotonicTime),
              cancellable->contains(requestId) ? "" : "not ",
              event.clientPid
                  ? fmt::format(" from pid {}", event.clientPid.value().get())
                  : ""));
    }
  }
  if (total == 0) {
    XLOGF(INFO, "{}: no Thrift requests in flight", reason);
    return;
  }
  XLOGF(
      INFO,
      "{}: {} Thrift request(s) in flight{}:\n  {}",
      reason,
      total,
      total > lines.size() ? fmt::format(" (first {})", lines.size()) : "",
      fmt::join(lines, "\n  "));
}

} // namespace facebook::eden
