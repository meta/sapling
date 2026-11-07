/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <chrono>
#include <optional>
#include <string_view>

#include <folly/coro/safe/NowTask.h>
#include <memory>
#include "eden/common/utils/ImmediateFuture.h"
#include "eden/fs/privhelper/PrivHelper.h"
#include "eden/fs/utils/FsChannelTypes.h"
#include "eden/fs/utils/RequestPermitVendor.h"

namespace facebook::eden {

class ProcessAccessLog;

/**
 * Why a channel stopped.
 */
struct FsStopReason {
  std::string_view name;
  // The mount went away without EdenFS stopping it (unmounted from outside, or
  // the connection to the kernel broke), so it may need to be remounted.
  bool unexpected{false};
};

class FsStopData {
 public:
  virtual ~FsStopData() = default;

  /**
   * If true, the mount has been stopped and should be considered unmounted.
   *
   * If false, this mount is intended to be taken over by a new EdenFS daemon.
   */
  virtual bool isUnmounted() = 0;

  virtual FsChannelInfo extractTakeoverInfo() = 0;

  /**
   * Why the channel stopped, for telemetry and recovery.
   */
  virtual FsStopReason getStopReason() const {
    return {"unknown", false};
  }

  /**
   * When the channel was asked to stop. It stops only once its outstanding
   * requests finish, which can take much longer.
   */
  std::optional<std::chrono::steady_clock::time_point> stopRequestedAt;
};

using FsStopDataPtr = std::unique_ptr<FsStopData>;

/**
 * A connection to a userspace filesystem driver.
 *
 * In practice, this is FuseChannel, Nfsd3, or PrjfsChannel.
 */
class FsChannel {
 public:
  using StopFuture = folly::SemiFuture<FsStopDataPtr>;

 protected:
  virtual ~FsChannel() = default;

  /**
   * A semaphore-based rate limiter used to limit the number of outstanding
   * requests to the FsChannel. This is initialized in the constructors of the
   * derived classes. The size of the semaphore is controlled by
   * fschannel:max-inflight-requests. If the config is set to zero, rate
   * limiting is disabled and this will be nullptr.
   */
  std::unique_ptr<RequestPermitVendor> requestRateLimiter_{nullptr};

  /**
   * Initialize the rate limiter with the given maximum number of concurrent
   * requests. This should be called by concrete implementations in their
   * constructor. If zero is passed, rate limiting is disabled and the permit
   * methods will be no-ops.
   */
  void initializeInflightRequestsRateLimiter(size_t maximumInFlightRequests) {
    if (maximumInFlightRequests > 0) {
      requestRateLimiter_ =
          std::make_unique<RequestPermitVendor>(maximumInFlightRequests);
    }
  }

 public:
  /**
   * Neither FuseChannel and Nfsd3 can be deleted from arbitrary threads.
   *
   * destroy() initiates the destruction process, but the delete will occur on
   * another thread.
   *
   * The FsChannel may not be accessed after destroy() is called.
   */
  virtual void destroy() = 0;

  /**
   * Returns a short, human-readable (or at least loggable) name for this
   * FsChannel type.
   *
   * e.g. "fuse", "nfs3", "prjfs"
   */
  virtual const char* getName() const = 0;

  /**
   * An FsChannel must be initialized after construction. This process begins
   * the handshake with the filesystem driver.
   *
   * Returns a SemiFuture that is completed when the initialized mount has shut
   * down. This future should be used to detect when the mount has been stopped
   * for an error or any other reason. For example, in FUSE and NFS, the unmount
   * process is initiated by the kernel and not by FuseChannel.
   */
  [[nodiscard]] virtual folly::Future<StopFuture> initialize() = 0;

  /**
   * Ask this FsChannel to remove itself from the filesystem.
   */
  [[nodiscard]] virtual folly::SemiFuture<folly::Unit> unmount(
      UnmountOptions options) = 0;

  /**
   * Ask this FsChannel to stop for a takeover request.
   *
   * Returns true if takeover is supported and a takeover attempt has begun.
   */
  virtual bool takeoverStop() = 0;

  /**
   * Returns the ProcessAccessLog used to track this channel's filesystem
   * accesses. Requests hold a copy, so a request that finishes after its
   * channel is destroyed can still record into the log.
   */
  virtual const std::shared_ptr<ProcessAccessLog>& getProcessAccessLog() = 0;

  /**
   * Some user-space filesystem implementations (notably Projected FS, but also
   * FUSE in writeback-cache mode) receive write notifications asynchronously.
   *
   * In situations like Thrift requests where EdenFS must guarantee previous
   * writes have been observed, call waitForPendingWrites. The returned future
   * will complete when all pending write operations have been observed.
   */
  [[nodiscard]] virtual ImmediateFuture<folly::Unit> waitForPendingWrites() = 0;

  [[nodiscard]] virtual folly::coro::now_task<folly::Unit>
  co_waitForPendingWrites() = 0;

  /**
   * During checkout or other Thrift calls that modify the filesystem, those
   * modifications may be invisible to the filesystem's own caches. Therefore,
   * we send fine-grained invalidation messages to the FsChannel. Those
   * invalidations may be asynchronous, but we need to ensure that they have
   * been observed by the time the Thrift call completes.
   *
   * You may think of completeInvalidations() as a fence; after
   * completeInvalidations() completes, invalidations of inode attributes, inode
   * content, and name lookups are guaranteed to be observable.
   */
  [[nodiscard]] virtual ImmediateFuture<folly::Unit>
  completeInvalidations() = 0;

  [[nodiscard]] virtual folly::coro::now_task<folly::Unit>
  co_completeInvalidations() = 0;

  /**
   * Helper function to acquire a permit from the rate limiter. This will block
   * until a permit is available. This is a no-op if rate limiting is disabled.
   */
  std::unique_ptr<RequestPermit> acquireFsRequestPermit() {
    if (requestRateLimiter_) {
      return requestRateLimiter_->acquirePermit();
    }
    return nullptr;
  }

  /**
   * Returns true if request rate limiting is enabled.
   */
  bool isRateLimitingEnabled() const {
    return requestRateLimiter_ != nullptr;
  }

  /**
   * Non-blocking variant of acquireFsRequestPermit(). Returns nullptr if
   * rate limiting is disabled or no capacity is available.
   */
  std::unique_ptr<RequestPermit> tryAcquireFsRequestPermit() {
    if (requestRateLimiter_) {
      return requestRateLimiter_->tryAcquirePermit();
    }
    return nullptr;
  }
};

/**
 * FsChannelDeleter acts as a deleter argument for std::shared_ptr or
 * std::unique_ptr.
 */
class FsChannelDeleter {
 public:
  void operator()(FsChannel* channel) {
    channel->destroy();
  }
};

using FsChannelPtr = std::unique_ptr<FsChannel, FsChannelDeleter>;

} // namespace facebook::eden
