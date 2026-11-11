/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <optional>

#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/Pipe.h"
#include "eden/fs/takeover/TakeoverData.h"

namespace facebook::eden {

/**
 * Keeps a socket in flight, queued with SCM_RIGHTS on a private socketpair,
 * while this object is alive.
 *
 * Once the old process closes its copies of the FDs it sent, the only
 * references to them are in the receive queue of our takeover socket. macOS's
 * garbage collector for in-flight FDs (unp_gc) only scans the receive queues
 * of sockets that are themselves in flight, so if it runs before we read the
 * message, it flushes those FDs and every mount reads EOF. Closing any Unix
 * socket schedules it, and the old process closes its takeover socket right
 * after sending. Pinning the takeover socket makes unp_gc scan its queue.
 */
class InFlightSocketPin {
 public:
  explicit InFlightSocketPin(int fd);

 private:
  std::optional<SocketPair> pair_;
};

folly::Future<UnixSocket::Message> receiveTakeoverDataMessage(
    FutureUnixSocket& socket,
    UnixSocket::Message&& msg,
    const std::chrono::seconds& takeoverReceiveTimeout);

/**
 * Request to take over mount points from an existing edenfs process.
 *
 * Returns a TakeoverData object on success, or throws an exception on error.
 */
TakeoverData takeoverMounts(
    AbsolutePathPiece socketPath,
    const std::chrono::seconds& takeoverReceiveTimeout,
    // Ask the server to cancel the Thrift requests it is serving instead of
    // refusing the takeover while one of them has run for long.
    bool force,
    // Let the server refuse the takeover while it has served a checkout or a
    // mount recently, as well as while it is busy.
    bool onlyIfIdle,
    // the following parameters are present for testing purposes and should not
    // normally be used in the production build.
    bool shouldThrowDuringTakeover = false,
    bool shouldPing = true,
    const std::set<int32_t>& supportedTakeoverVersions =
        kSupportedTakeoverVersions,
    const uint64_t supportedTakeoverCapabilities = kSupportedCapabilities);

} // namespace facebook::eden
