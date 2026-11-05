/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

// Implementation of the mount protocol as described in:
// https://tools.ietf.org/html/rfc1813#page-106

#include <folly/File.h>
#include <folly/SocketAddress.h>
#include <folly/futures/Future.h>
#include "eden/common/utils/PathFuncs.h"
#include "eden/fs/inodes/InodeNumber.h"

namespace folly {
class Executor;
class EventBase;
} // namespace folly

namespace facebook::eden {

class MountdServerProcessor;
class RpcServer;
class EdenFsEventsLogger;

class Mountd {
 public:
  /**
   * Create a new RPC mountd program.
   *
   * All the socket processing will be run on the EventBase passed in. This
   * also must be called on that EventBase thread.
   *
   * Note: at mount time, EdenFS will manually call mount.nfs with -o mountport
   * to manually specify the port on which this server is bound, so registering
   * is not necessary for a properly behaving EdenFS.
   */
  Mountd(
      folly::EventBase* evb,
      std::shared_ptr<folly::Executor> threadPool,
      const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
      size_t maximumInFlightRequests,
      std::chrono::nanoseconds highNfsRequestsLogInterval);

  /**
   * Bind the RPC mountd program to the passed in address.
   */
  void initialize(folly::SocketAddress addr);
  void initialize(folly::File socket);
  void resumeAccepting();

  /**
   * Register a path as the root of a mount point.
   *
   * Once registered, the mount RPC request for that specific path will answer
   * positively with the passed in InodeNumber.
   */
  void registerMount(AbsolutePathPiece path, InodeNumber rootIno);

  /**
   * Unregister the mount point matching the path. Throws if path unregistration
   * fails for any reason.
   */
  void unregisterMount(AbsolutePathPiece path);

  /**
   * Best effort attempt to unregister the mount point matching the path.
   *
   * For NFS mounts, we register the mount prior to finishing initialization.
   * Failure after registration (but before initialization) causes the
   * uninitialized mount to get stuck in the Mountd's map of registered mounts
   * and causes crashes when remount attempts occur. To avoid this, we must
   * always unregister upon initialization failure. This method is a best
   * effort attempt to unregister the mount point that failed to mount.
   */
  void tryUnregisterMount(AbsolutePathPiece path);

  /**
   * Return true if the path is registered as a mount point.
   */
  bool isMountRegistered(AbsolutePathPiece path);

  /**
   * Obtain the address that this mountd program is listening on.
   */
  folly::SocketAddress getAddr() const;

  /**
   * Must be called on the Mountd's EventBase.
   */
  folly::SemiFuture<folly::File> takeoverStop();

  Mountd(const Mountd&) = delete;
  Mountd(Mountd&&) = delete;
  Mountd& operator=(const Mountd&) = delete;
  Mountd& operator=(Mountd&&) = delete;

 private:
  std::shared_ptr<MountdServerProcessor> proc_;
  std::shared_ptr<RpcServer> server_;
};

} // namespace facebook::eden
