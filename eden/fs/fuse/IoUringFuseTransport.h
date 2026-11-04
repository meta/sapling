/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#ifndef _WIN32

#include <array>
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <memory>
#include <mutex>
#include <optional>
#include <shared_mutex>
#include <string>
#include <thread>
#include <unordered_map>
#include <vector>

#include <folly/File.h>
#include <folly/SharedMutex.h>
#include <folly/Synchronized.h>
#include <folly/hash/Hash.h>
#include <folly/lang/Align.h>
#include <folly/synchronization/CallOnce.h>

#include <folly/portability/GTestProd.h>

#include "eden/fs/fuse/DevFuseTransport.h"
#include "eden/fs/fuse/FuseFeatures.h"
#include "eden/fs/telemetry/EdenStats.h"

#if EDEN_HAVE_FUSE_IO_URING
#include <liburing.h>
#endif

#include "eden/fs/fuse/FuseTransport.h"

namespace facebook::eden {

class IoUringFuseTransport final : public FuseTransport {
 public:
  explicit IoUringFuseTransport(
      uint32_t queueDepth,
      bool disableIoWait = false,
      bool skipSelfWakeup = true);
  ~IoUringFuseTransport() override;
  IoUringFuseTransport(const IoUringFuseTransport&) = delete;
  IoUringFuseTransport& operator=(const IoUringFuseTransport&) = delete;
  IoUringFuseTransport(IoUringFuseTransport&&) = delete;
  IoUringFuseTransport& operator=(IoUringFuseTransport&&) = delete;

  const char* getName() const override;
  size_t getWorkerThreadCount(size_t defaultThreadCount) const override;
  void requestStopWakeup() override;
  ssize_t readInitPacket(int fd, void* buf, size_t size) const override;
  void processSession(FuseChannel& channel) override;
  void replyError(
      FuseChannel& channel,
      const fuse_in_header& request,
      int errorCode) const override;
  void replyNone(FuseChannel& channel, const fuse_in_header& request)
      const override;
  void sendRawReply(FuseChannel& channel, const iovec iov[], size_t count)
      const override;

#if EDEN_HAVE_FUSE_IO_URING
  // Returns an error string if this process cannot set up the FUSE io_uring
  // queue. Eden uses this before FUSE_INIT so it can fall back to devfuse when
  // the current runtime blocks io_uring setup.
  static std::optional<std::string> getMaybeSetupError(
      uint32_t queueDepth,
      int fuseFd);

  // Creates every queue's ring and allocates its buffers up front, keeping
  // them, and returns an error description if any queue could not be prepared.
  //
  // getMaybeSetupError() only proves that a *single* ring fits. This proves the
  // whole set fits, which is what actually gets created: one queue per logical
  // CPU, each charging RLIMIT_MEMLOCK against a counter shared by every process
  // running as this uid. A co-tenant can leave room for one ring but not for
  // all of them.
  //
  // The queues are kept rather than probed-and-released so nothing can consume
  // the headroom between the check and the real bring-up. The caller must run
  // this before answering FUSE_INIT: once the reply advertises
  // FUSE_OVER_IO_URING the kernel blocks request allocation until the ring is
  // ready, so declining io_uring is no longer possible.
  //
  // On failure the ring pool is torn down, so the caller can simply discard
  // this transport and continue with devfuse.
  std::optional<std::string> prepareAllQueues(FuseChannel& channel);
#endif

 private:
#if EDEN_HAVE_FUSE_IO_URING
  FOLLY_GTEST_FRIEND_TEST(FuseChannelTest, ioUringSubmitAndWaitErrorPolicy);
  FOLLY_GTEST_FRIEND_TEST(FuseChannelTest, ioUringCqeErrorPolicy);
  FOLLY_GTEST_FRIEND_TEST(FuseChannelTest, ioUringDisableIoWaitAppliesNoIoWait);
  FOLLY_GTEST_FRIEND_TEST(FuseChannelTest, ioUringDefaultDoesNotDisableIoWait);
  FOLLY_GTEST_FRIEND_TEST(
      FuseChannelTest,
      ioUringStopWakeupBeforeEventFdPublished);
  FOLLY_GTEST_FRIEND_TEST(
      FuseChannelTest,
      ioUringRequestStopWakeupBeforeRingPoolPublished);

  struct RingPool;

  struct RingEntry {
    using Buffer = std::unique_ptr<void, decltype(&free)>;
    enum class Phase {
      Idle,
      RegisterFetchInFlight,
      RequestOutstanding,
      CommitAndFetchInFlight,
    };

    RingPool* pool{nullptr};
    size_t queueId{0};
    Buffer requestHeaderStorage{nullptr, &free};
    Buffer payloadStorage{nullptr, &free};
    fuse_uring_req_header* requestHeader{nullptr};
    void* payload{nullptr};
    size_t payloadSize{0};
    uint64_t requestCommitId{0};
    Phase phase{Phase::Idle};
    std::array<iovec, 2> iov{};
  };

  struct RingQueue {
    RingQueue();
    ~RingQueue() noexcept;
    RingQueue(const RingQueue&) = delete;
    RingQueue& operator=(const RingQueue&) = delete;
    RingQueue(RingQueue&& other) noexcept;
    RingQueue& operator=(RingQueue&& other) noexcept;

    void reset() noexcept;
    void resetMovedFrom() noexcept;

    RingPool* pool{nullptr};
    size_t queueId{0};
    // -1 until the owning worker publishes a real fd. May transiently hold
    // IoUringFuseTransport::kStopRequestedBeforeReady if requestStopWakeup()
    // ran before publication; see requestQueueStopWakeup().
    std::atomic<int> eventFd{-1};
    size_t requestHeaderSize{sizeof(fuse_uring_req_header)};
    std::thread::id ownerThreadId;
    io_uring ring{};
    bool ringInitialized{false};
    // Tracks the entry buffers separately from the ring: the two halves of
    // bring-up can run at different times, so each needs its own guard.
    bool buffersAllocated{false};
    std::vector<RingEntry> entries;
    std::unique_ptr<folly::Synchronized<std::vector<RingEntry*>>>
        pendingCommits;
  };

  struct RingPool {
    RingPool() = default;
    RingPool(const RingPool&) = delete;
    RingPool& operator=(const RingPool&) = delete;
    RingPool(RingPool&&) = delete;
    RingPool& operator=(RingPool&&) = delete;
    ~RingPool() = default;

    size_t queueDepth{0};
    size_t maxRequestPayloadSize{0};
    folly::File devFuseStopFd;
    std::vector<RingQueue> queues;
  };

  struct DecodedRequest {
    RingEntry* entry{nullptr};
    fuse_in_header header{};
    std::vector<uint8_t> arguments;
  };

  struct CqeResult {
    enum class Action {
      DispatchRequest,
      Ignored,
      StopRequested,
    };

    Action action{Action::Ignored};
    std::optional<DecodedRequest> request;
  };

  // Guards ringPool_ only across the boundary between the worker thread that
  // constructs/owns it and requestStopWakeup(), which may run on a thread
  // that never synchronizes with that worker via folly::call_once (see
  // initializeSession()). processSession() and its helpers read ringPool_
  // without this lock: they always run after folly::call_once has completed
  // initializeRingPool() for the calling thread, which already establishes
  // the necessary happens-before edge.
  //
  // Those unlocked reads also depend on ringPool_ outliving every worker, so
  // destroyRingPool() must never run while a worker could still be using the
  // pool. Both of its callers satisfy that: ~IoUringFuseTransport() runs
  // after the worker threads have been joined, and prepareAllQueues() runs
  // before FUSE_INIT is answered, which is before any worker has started. A
  // third caller, or either of these moving to a point where workers can
  // still be running, means processSession() and its helpers must take the
  // shared lock too.
  mutable folly::SharedMutex ringPoolMutex_;
  std::unique_ptr<RingPool> ringPool_;
  // Request contexts retain their source transport after the reader exits.
  DevFuseTransport devFuseTransport_;

  // Sentinel stored in RingQueue::eventFd to record that requestStopWakeup()
  // observed the queue before its eventfd was published. See
  // requestQueueStopWakeup().
  static constexpr int kStopRequestedBeforeReady = -2;

  // io_uring error handelings are aligned with the libfuse error handling
  static bool isTransientSubmitAndWaitError(int result);
  static bool shouldRetrySubmitAndWaitError(int result, bool stopRequested);
  static bool shouldIgnoreSubmitAndWaitError(int result, bool stopRequested);
  static bool shouldIgnoreCqeError(int result);
  static bool shouldIgnoreCqeErrorDuringShutdown(int result);
  static bool shouldIgnoreCqeError(int result, bool stopRequested);
  static bool isWakeEventCqe(
      const RingQueue& queue,
      const io_uring_cqe& cqe,
      void* userData);

  void markRegisterFetchSubmission(RingEntry& entry) const;
  void markDecodedRequest(RingEntry& entry) const;
  void markCommitAndFetchSubmission(RingEntry& entry) const;
  bool shouldRecoverCanceledCommitAndFetchCqe(
      const RingQueue& queue,
      void* userData,
      bool stopRequested) const;
  void recoverCanceledCommitAndFetchCqe(RingQueue& queue, RingEntry& entry)
      const;
  bool isEntryOutstanding(const RingEntry& entry) const;
  void logCanceledCqe(
      const RingQueue& queue,
      const io_uring_cqe& cqe,
      bool stopRequested,
      void* userData) const;
  void queueCommitAndFetch(RingEntry& entry, const EdenStatsPtr& stats) const;
  void processPendingCommits(RingQueue& queue) const;
  bool hasPendingCommits(const RingQueue& queue) const;
  bool shouldExitWorkerLoop(const FuseChannel& channel, const RingQueue& queue)
      const;
  void notifyWorker(const RingQueue& queue) const;
  void requestQueueStopWakeup(RingQueue& queue) const;
  static size_t getConfiguredQueueCount(size_t defaultThreadCount);
  void initializeRingPool(size_t queueCount, size_t maxRequestPayloadSize);
  void initializeSession(FuseChannel& channel);
  // Bring-up is split into three phases so they can run at different times.
  // Ring creation and buffer allocation are pure io_uring/userspace work and
  // can run before FUSE_INIT is answered; only registration requires
  // FUSE_OVER_IO_URING to have been negotiated. Keeping every allocation in
  // the first two phases means a shortfall is detectable while declining
  // io_uring is still possible.
  void createQueueRing(RingQueue& queue, int fuseFd) const;
  void allocateQueueBuffers(RingQueue& queue) const;
  void registerQueueWithFuse(RingQueue& queue) const;
  void initializeQueueForWorker(RingQueue& queue, int fuseFd) const;
  void initializeEntryBuffers(RingQueue& queue, RingEntry& entry) const;
  void prepareWakePollSqe(RingQueue& queue) const;
  void prepareFetchRequest(RingQueue& queue, RingEntry& entry) const;
  void prepareFetchRequests(RingQueue& queue) const;
  CqeResult handleCqe(
      RingQueue& queue,
      const io_uring_cqe& cqe,
      bool stopRequested) const;
  CqeResult handleWakeEventCqe(RingQueue& queue) const;
  void rejectDecodedRequestAfterStop(
      const DecodedRequest& request,
      const EdenStatsPtr& stats) const;
  void registerOutstandingEntry(uint64_t unique, RingEntry& entry) const;
  RingEntry& takeOutstandingEntry(uint64_t unique) const;
  void prepareCommitAndFetchSqe(RingQueue& queue, RingEntry& entry) const;
  static fuse_out_header& getReplyHeader(RingEntry& entry);
  static fuse_uring_ent_in_out& getRingEntryInOut(RingEntry& entry);
  void destroyRingPool() noexcept;
  static void* allocatePageAlignedBuffer(size_t size);

  // Every ring worker registers and takes an entry per request, so the map is
  // sharded by a hash of the request's `unique` to keep workers off each
  // other's locks.
  struct alignas(folly::hardware_destructive_interference_size)
      OutstandingShard {
    folly::Synchronized<std::unordered_map<uint64_t, RingEntry*>> entries;
  };
  mutable std::array<OutstandingShard, 64> outstandingEntries_;
  folly::Synchronized<std::unordered_map<uint64_t, RingEntry*>>&
  outstandingShard(uint64_t unique) const {
    return outstandingEntries_
        [folly::hash::twang_mix64(unique) % outstandingEntries_.size()]
            .entries;
  }
  mutable folly::once_flag sessionInitFlag_;
  mutable std::atomic<size_t> nextWorkerId_{0};
  // When true, pass IORING_ENTER_NO_IOWAIT on io_uring_enter so that a worker
  // parked waiting for completions is not charged as iowait, which otherwise
  // inflates /proc/stat iowait and cgroup io.pressure (PSI) even when no real
  // FUSE I/O is outstanding. Only honored on kernels with
  // IORING_FEAT_NO_IOWAIT.
  bool disableIoWait_{false};
  // Skip notifyWorker() for replies queued by the queue's own worker.
  // Gated by experimental:fuse-io-uring-skip-self-wakeup.
  bool skipSelfWakeup_{false};
#endif
  uint32_t queueDepth_;
};

} // namespace facebook::eden

#endif
