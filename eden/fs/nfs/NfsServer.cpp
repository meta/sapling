/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/nfs/NfsServer.h"

#include "eden/fs/nfs/Nfsd3.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"

namespace facebook::eden {

NfsServer::NfsServer(
    PrivHelper* privHelper,
    folly::EventBase* evb,
    std::shared_ptr<folly::Executor> threadPool,
    const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
    size_t maximumInFlightRequests,
    std::chrono::nanoseconds highNfsRequestsLogInterval,
    std::chrono::nanoseconds longRunningFSRequestThreshold)
    : privHelper_{privHelper},
      evb_(evb),
      threadPool_{std::move(threadPool)},
      mountd_(
          evb_,
          threadPool_,
          edenFsEventsLogger,
          maximumInFlightRequests,
          highNfsRequestsLogInterval),
      maximumInFlightRequests_(maximumInFlightRequests),
      highNfsRequestsLogInterval_(highNfsRequestsLogInterval),
      longRunningFSRequestThreshold_(longRunningFSRequestThreshold) {}

void NfsServer::initialize(folly::SocketAddress addr) {
  mountd_.initialize(std::move(addr));
}

void NfsServer::initialize(folly::File socket) {
  mountd_.initialize(std::move(socket));
}

void NfsServer::resumeMountdAccepting() {
  mountd_.resumeAccepting();
}

folly::SocketAddress NfsServer::getMountdAddr() const {
  return mountd_.getAddr();
}

NfsServer::NfsMountInfo NfsServer::registerMount(
    AbsolutePathPiece path,
    InodeNumber rootIno,
    std::unique_ptr<NfsDispatcher> dispatcher,
    const folly::Logger* straceLogger,
    std::shared_ptr<ProcessInfoCache> processInfoCache,
    const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
    ErrorLogger& errorLogger,
    folly::Duration requestTimeout,
    std::shared_ptr<Notifier> notifier,
    CaseSensitivity caseSensitive,
    uint32_t readIoSize,
    uint32_t writeIoSize,
    size_t traceBusCapacity,
    bool fastPathRPCs,
    std::shared_ptr<ReloadableConfig> config,
    FaultInjector& faultInjector) {
  auto nfsd = std::unique_ptr<Nfsd3, FsChannelDeleter>{new Nfsd3{
      privHelper_,
      AbsolutePath{path},
      evb_,
      threadPool_,
      std::move(dispatcher),
      straceLogger,
      std::move(processInfoCache),
      edenFsEventsLogger,
      errorLogger,
      requestTimeout,
      std::move(notifier),
      caseSensitive,
      readIoSize,
      writeIoSize,
      maximumInFlightRequests_,
      highNfsRequestsLogInterval_,
      longRunningFSRequestThreshold_,
      traceBusCapacity,
      fastPathRPCs,
      std::move(config),
      faultInjector}};
  mountd_.registerMount(path, rootIno);

  return {std::move(nfsd), mountd_.getAddr()};
}

void NfsServer::unregisterMount(AbsolutePathPiece path) {
  mountd_.unregisterMount(path);
}

void NfsServer::tryUnregisterMount(AbsolutePathPiece path) {
  mountd_.tryUnregisterMount(path);
}

bool NfsServer::isMountRegistered(AbsolutePathPiece path) {
  return mountd_.isMountRegistered(path);
}

folly::SemiFuture<folly::File> NfsServer::takeoverStop() {
  return mountd_.takeoverStop();
}

} // namespace facebook::eden
