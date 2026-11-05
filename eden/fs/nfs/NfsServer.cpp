/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/nfs/NfsServer.h"

#include "eden/fs/nfs/Nfsd3.h"
#include "eden/fs/nfs/portmap/Rpcbindd.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"

namespace facebook::eden {

NfsServer::NfsServer(
    PrivHelper* privHelper,
    folly::EventBase* evb,
    std::shared_ptr<folly::Executor> threadPool,
    bool shouldRunOurOwnRpcbindServer,
    const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
    size_t maximumInFlightRequests,
    std::chrono::nanoseconds highNfsRequestsLogInterval,
    std::chrono::nanoseconds longRunningFSRequestThreshold)
    : privHelper_{privHelper},
      evb_(evb),
      threadPool_{std::move(threadPool)},
      rpcbindd_(
          shouldRunOurOwnRpcbindServer ? std::make_shared<Rpcbindd>(
                                             evb_,
                                             threadPool_,
                                             edenFsEventsLogger,
                                             maximumInFlightRequests,
                                             highNfsRequestsLogInterval)
                                       : nullptr),
      mountd_(
          evb_,
          threadPool_,
          edenFsEventsLogger,
          maximumInFlightRequests,
          highNfsRequestsLogInterval),
      maximumInFlightRequests_(maximumInFlightRequests),
      highNfsRequestsLogInterval_(highNfsRequestsLogInterval),
      longRunningFSRequestThreshold_(longRunningFSRequestThreshold) {}

void NfsServer::initialize(
    folly::SocketAddress addr,
    bool registerMountdWithRpcbind) {
  mountd_.initialize(addr, registerMountdWithRpcbind);
  if (rpcbindd_) {
    rpcbindd_->initialize();
  }
  auto registeredAddr = mountd_.getAddr();
  // we can't register uds sockets with our portmapper (portmapper v2
  // does not support those).
  if (registeredAddr.isFamilyInet()) {
    recordPortNumber(
        mountd_.getProgramNumber(),
        mountd_.getProgramVersion(),
        registeredAddr.getPort());
  }
}

void NfsServer::initialize(folly::File socket) {
  mountd_.initialize(std::move(socket));
  if (rpcbindd_) {
    rpcbindd_->initialize();
  }
  // TODO: we should register the mountd server on takeover too. but
  // we only transfer the connected socket not the listening socket.
  // the listening one is the one we wanna register. So we need to
  // transfer that socket to be able to register it.
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

/**
 * Registers an RPC service running a certain protocol version on port.
 */
void NfsServer::recordPortNumber(
    uint32_t protocol,
    uint32_t version,
    uint32_t port) {
  if (rpcbindd_) {
    rpcbindd_->recordPortNumber(protocol, version, port);
  }
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
