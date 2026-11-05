/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <tuple>

#include "eden/common/utils/CaseSensitivity.h"
#include "eden/fs/inodes/FsChannel.h"
#include "eden/fs/nfs/Mountd.h"
#include "eden/fs/nfs/rpc/RpcServer.h"

namespace folly {
class Executor;
}

namespace facebook::eden {

class EdenFsEventsLogger;
class ErrorLogger;
class Notifier;
class NfsDispatcher;
class ProcessInfoCache;
class PrivHelper;
class ReloadableConfig;
class Rpcbindd;
class Nfsd3;
class FaultInjector;

class NfsServer {
 public:
  /**
   * Create a new NFS server.
   *
   * This will handle the lifetime of the various programs involved in the NFS
   * protocol including mountd and nfsd. The requests will be serviced by the
   * passed in threadPool.
   *
   * One mountd program will be created per NfsServer, while one nfsd program
   * will be created per-mount point, this allows nfsd program to be only aware
   * of its own mount point which greatly simplifies it.
   */
  NfsServer(
      PrivHelper* privHelper,
      folly::EventBase* evb,
      std::shared_ptr<folly::Executor> threadPool,
      bool shouldRunOurOwnRpcbindServer,
      const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
      size_t maximumInFlightRequests,
      std::chrono::nanoseconds highNfsRequestsLogInterval,
      std::chrono::nanoseconds longRunningFSRequestThreshold);

  /**
   * Bind the NfsServer to the passed in socket.
   *
   * See Mountd::initialize for the meaning of registerMountdWithRpcbind.
   */
  void initialize(folly::SocketAddress addr, bool registerMountdWithRpcbind);
  void initialize(folly::File socket);
  void resumeMountdAccepting();
  folly::SocketAddress getMountdAddr() const;

  /**
   * Return value of registerMount.
   */
  struct NfsMountInfo {
    std::unique_ptr<Nfsd3, FsChannelDeleter> nfsd;
    folly::SocketAddress mountdAddr;
  };

  /**
   * Register a path as the root of a mount point.
   *
   * This will create an nfs program for that mount point and register it with
   * the mountd program.
   *
   * @return: the created nfsd program as well as a tuple that holds the TCP
   * port number that mountd and nfsd are listening to.
   */
  NfsServer::NfsMountInfo registerMount(
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
      FaultInjector& faultInjector);

  /**
   * Registers an RPC service running a certain protocol version on port.
   */
  void recordPortNumber(uint32_t protocol, uint32_t version, uint32_t port);

  /**
   * Unregister the mount point matching the path.
   *
   * The nfs program will also be destroyed, and thus it is expected that
   * EdenFS has unmounted this mount point before calling this function.
   */
  void unregisterMount(AbsolutePathPiece path);

  /**
   * Try to unregister the mount point matching the path.
   *
   * The nfs program will also be destroyed, and thus it is expected that
   * EdenFS has unmounted this mount point before calling this function. Does
   * not throw if the mount point is already unregistered.
   */
  void tryUnregisterMount(AbsolutePathPiece path);

  /**
   * Return true if the path is registered as a mount point.
   */
  bool isMountRegistered(AbsolutePathPiece path);

  /**
   * Return the EventBase that the various NFS programs are running on.
   */
  folly::EventBase* getEventBase() const {
    return evb_;
  }

  /**
   * Must be called on the NfsServer's EventBase.
   */
  folly::SemiFuture<folly::File> takeoverStop();

  NfsServer(const NfsServer&) = delete;
  NfsServer(NfsServer&&) = delete;
  NfsServer& operator=(const NfsServer&) = delete;
  NfsServer& operator=(NfsServer&&) = delete;

 private:
  PrivHelper* const privHelper_;
  folly::EventBase* evb_;
  std::shared_ptr<folly::Executor> threadPool_;
  std::shared_ptr<Rpcbindd> rpcbindd_;
  Mountd mountd_;
  size_t maximumInFlightRequests_;
  std::chrono::nanoseconds highNfsRequestsLogInterval_;
  std::chrono::nanoseconds longRunningFSRequestThreshold_;
};

} // namespace facebook::eden
