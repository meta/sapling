/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/fuse/IoUringFuseTransport.h"
#include "eden/fs/fuse/FuseChannel.h"

#if EDEN_HAVE_FUSE_IO_URING
#include <poll.h>
#include <sched.h>
#include <sys/eventfd.h>
#include <sys/sysinfo.h>
#endif

#include <unistd.h>
#include <algorithm>
#include <cerrno>
#include <cstring>
#include <limits>
#include <optional>
#include <stdexcept>
#include <string>
#include <system_error>

#include <fmt/core.h>
#include <folly/logging/xlog.h>

#if EDEN_HAVE_FUSE_IO_URING
#ifndef IORING_SETUP_SQE128
#error \
    "FUSE io_uring transport requires liburing support for IORING_SETUP_SQE128"
#endif
#endif

namespace facebook::eden {

namespace {

[[noreturn]] void throwIoUringNotImplemented(
    const char* method,
    uint32_t queueDepth) {
  throw std::runtime_error(
      fmt::format(
          "IoUringFuseTransport::{} not implemented (queueDepth={})",
          method,
          queueDepth));
}

#if EDEN_HAVE_FUSE_IO_URING
enum class QueueSetupStep {
  EventFd,
  Setup,
  RegisterFiles,
};

struct QueueSetupError {
  QueueSetupStep step;
  int errnum;
};

const char* getQueueSetupStepName(QueueSetupStep step) {
  switch (step) {
    case QueueSetupStep::EventFd:
      return "eventfd";
    case QueueSetupStep::Setup:
      return "setup";
    case QueueSetupStep::RegisterFiles:
      return "register_files";
  }
  return "unknown";
}

const char* getQueueSetupFailureReason(int errnum) {
  switch (errnum) {
    case EPERM:
    case EACCES:
      return "blocked or disabled";
    case ENOSYS:
      return "unavailable or hidden by seccomp";
    default:
      return "failed availability probe";
  }
}

std::string describeQueueSetupError(const QueueSetupError& error) {
  return fmt::format(
      "io_uring {} is {}: {} ({})",
      getQueueSetupStepName(error.step),
      getQueueSetupFailureReason(error.errnum),
      std::generic_category().message(error.errnum),
      error.errnum);
}

// Takes a pointer rather than a reference: with IORING_SETUP_SQE128 each SQE
// is 128 bytes, larger than `io_uring_sqe`, and `cmd` extends past the end of
// the struct. Through a reference, clang sizes `cmd` from the declared struct
// (16 bytes) and rejects larger copies into it with -Wfortify-source.
void prepareUringCmdSqe(
    io_uring_sqe* sqe,
    uint32_t cmdOp,
    uint16_t queueId,
    uint64_t commitId,
    void* userData) {
  std::memset(sqe, 0, sizeof(*sqe));
  sqe->opcode = IORING_OP_URING_CMD;
  sqe->flags = IOSQE_FIXED_FILE;
  sqe->ioprio = 0;
  sqe->fd = 0;
  sqe->off = 0;
  sqe->rw_flags = 0;
  io_uring_sqe_set_data(sqe, userData);
  sqe->cmd_op = cmdOp;
  sqe->__pad1 = 0;

  fuse_uring_cmd_req cmd = {};
  cmd.flags = 0;
  cmd.commit_id = commitId;
  cmd.qid = queueId;
  std::memcpy(sqe->cmd, &cmd, sizeof(cmd));
}

size_t roundUpToPageSize(size_t size) {
  const auto pageSize = static_cast<size_t>(getpagesize());
  return ((size + pageSize - 1) / pageSize) * pageSize;
}

// Returns false if the thread was not pinned, in which case its affinity is
// whatever it was before the call -- callers that pin repeatedly need to know,
// since a stale mask is worse than none.
bool pinThreadToCpu(size_t cpu, size_t cpuCount) {
  if (cpuCount == 0) {
    return false;
  }

  // cpu_set_t has a fixed capacity, so clamp the affinity domain to the CPUs
  // that CPU_SET() can actually encode before normalizing the queue id.
  const auto usableCpuCount =
      std::min(cpuCount, static_cast<size_t>(CPU_SETSIZE));
  cpu %= usableCpuCount;
  cpu_set_t cpuset;
  CPU_ZERO(&cpuset);
  CPU_SET(cpu, &cpuset);
  if (sched_setaffinity(0, sizeof(cpuset), &cpuset) != 0) {
    const auto savedErrno = errno;
    XLOGF(
        WARN,
        "failed to pin io_uring thread to cpu {}: {}",
        cpu,
        std::generic_category().message(savedErrno));
    return false;
  }
  return true;
}

// Restores the calling thread's CPU affinity on scope exit, so a function that
// pins itself temporarily does not leave the thread bound to whatever CPU it
// touched last.
class ScopedThreadAffinity {
 public:
  ScopedThreadAffinity() {
    saved_ = sched_getaffinity(0, sizeof(original_), &original_) == 0;
    if (!saved_) {
      const auto savedErrno = errno;
      XLOGF(
          WARN,
          "failed to read thread CPU affinity: {}",
          std::generic_category().message(savedErrno));
    }
  }

  ~ScopedThreadAffinity() {
    restore();
  }

  // Puts the captured mask back. Idempotent, and a no-op when the original
  // mask was never captured, so a caller can also use it to undo a partial pin
  // mid-scope.
  void restore() {
    if (!saved_) {
      return;
    }
    if (sched_setaffinity(0, sizeof(original_), &original_) != 0) {
      const auto savedErrno = errno;
      // Worth a warning rather than a silent return: threads spawned after
      // this point inherit whatever mask is left behind, and not all of them
      // pin themselves afterwards.
      XLOGF(
          WARN,
          "failed to restore thread CPU affinity: {}",
          std::generic_category().message(savedErrno));
    }
  }

  // Strictly scope-bound: a movable guard could restore twice, or not at all.
  ScopedThreadAffinity(const ScopedThreadAffinity&) = delete;
  ScopedThreadAffinity& operator=(const ScopedThreadAffinity&) = delete;
  ScopedThreadAffinity(ScopedThreadAffinity&&) = delete;
  ScopedThreadAffinity& operator=(ScopedThreadAffinity&&) = delete;

  // False when the original mask could not be captured, in which case the
  // caller must not pin: there would be no way to undo it.
  bool canRestore() const {
    return saved_;
  }

 private:
  cpu_set_t original_{};
  bool saved_{false};
};

std::optional<QueueSetupError> setupQueue(
    uint32_t queueDepth,
    int fuseFd,
    int& eventFd,
    io_uring& ring,
    bool& ringInitialized) noexcept {
  if (fuseFd < 0) {
    return QueueSetupError{QueueSetupStep::RegisterFiles, EBADF};
  }

  eventFd = eventfd(0, EFD_CLOEXEC);
  if (eventFd < 0) {
    return QueueSetupError{QueueSetupStep::EventFd, errno};
  }

  io_uring_params params = {};
  auto depth = static_cast<unsigned>(queueDepth + 1);
  params.flags = IORING_SETUP_CQSIZE | IORING_SETUP_SQE128;
  params.cq_entries = depth * 2;

  auto rc = io_uring_queue_init_params(depth, &ring, &params);
  if (rc != 0) {
    return QueueSetupError{QueueSetupStep::Setup, -rc};
  }
  ringInitialized = true;

  int files[1] = {fuseFd};
  rc = io_uring_register_files(&ring, files, 1);
  if (rc != 0) {
    return QueueSetupError{QueueSetupStep::RegisterFiles, -rc};
  }

  return std::nullopt;
}

[[noreturn]] void throwIoUringSetupError(
    const QueueSetupError& error,
    size_t queueId) {
  switch (error.step) {
    case QueueSetupStep::EventFd:
      throw std::system_error(
          error.errnum,
          std::generic_category(),
          fmt::format(
              "failed to create io_uring eventfd for queue {}", queueId));
    case QueueSetupStep::Setup:
      throw std::system_error(
          error.errnum,
          std::generic_category(),
          fmt::format("failed to initialize io_uring queue {}", queueId));
    case QueueSetupStep::RegisterFiles:
      throw std::system_error(
          error.errnum,
          std::generic_category(),
          fmt::format(
              "failed to register /dev/fuse with io_uring queue {}", queueId));
  }

  XCHECK(false) << "unknown io_uring setup step";
}
#endif

} // namespace

IoUringFuseTransport::IoUringFuseTransport(
    uint32_t queueDepth,
    bool disableIoWait,
    bool skipSelfWakeup)
    : queueDepth_{queueDepth} {
#if EDEN_HAVE_FUSE_IO_URING
  disableIoWait_ = disableIoWait;
  skipSelfWakeup_ = skipSelfWakeup;
#else
  (void)disableIoWait;
  (void)skipSelfWakeup;
#endif
}

IoUringFuseTransport::~IoUringFuseTransport() {
#if EDEN_HAVE_FUSE_IO_URING
  destroyRingPool();
#endif
}

#if EDEN_HAVE_FUSE_IO_URING
IoUringFuseTransport::RingQueue::RingQueue() {
  ring.ring_fd = -1;
  requestHeaderSize = roundUpToPageSize(sizeof(fuse_uring_req_header));
  pendingCommits =
      std::make_unique<folly::Synchronized<std::vector<RingEntry*>>>();
}

IoUringFuseTransport::RingQueue::~RingQueue() noexcept {
  reset();
}

IoUringFuseTransport::RingQueue::RingQueue(RingQueue&& other) noexcept
    : pool{other.pool},
      queueId{other.queueId},
      eventFd{other.eventFd.exchange(-1, std::memory_order_acq_rel)},
      requestHeaderSize{other.requestHeaderSize},
      ownerThreadId{other.ownerThreadId},
      ring{other.ring},
      ringInitialized{other.ringInitialized},
      buffersAllocated{other.buffersAllocated},
      entries{std::move(other.entries)},
      pendingCommits{std::move(other.pendingCommits)} {
  other.resetMovedFrom();
}

IoUringFuseTransport::RingQueue& IoUringFuseTransport::RingQueue::operator=(
    RingQueue&& other) noexcept {
  if (this == &other) {
    return *this;
  }

  reset();

  pool = other.pool;
  queueId = other.queueId;
  eventFd.store(
      other.eventFd.exchange(-1, std::memory_order_acq_rel),
      std::memory_order_release);
  requestHeaderSize = other.requestHeaderSize;
  ownerThreadId = other.ownerThreadId;
  ring = other.ring;
  ringInitialized = other.ringInitialized;
  buffersAllocated = other.buffersAllocated;
  entries = std::move(other.entries);
  pendingCommits = std::move(other.pendingCommits);

  other.resetMovedFrom();

  return *this;
}

void IoUringFuseTransport::RingQueue::resetMovedFrom() noexcept {
  pool = nullptr;
  queueId = 0;
  eventFd.store(-1, std::memory_order_relaxed);
  requestHeaderSize = sizeof(fuse_uring_req_header);
  ownerThreadId = {};
  ring = {};
  ring.ring_fd = -1;
  ringInitialized = false;
  // entries (and the buffers they own) were moved out along with the vector.
  buffersAllocated = false;
}

void IoUringFuseTransport::RingQueue::reset() noexcept {
  if (ringInitialized) {
    io_uring_queue_exit(&ring);
    ringInitialized = false;
    ring = {};
    ring.ring_fd = -1;
  }

  const auto fd = eventFd.exchange(-1, std::memory_order_acq_rel);
  if (fd >= 0) {
    if (close(fd) != 0) {
      const auto savedErrno = errno;
      XLOGF(
          WARN,
          "failed to close io_uring eventfd for queue {}: errno={}",
          queueId,
          savedErrno);
    }
  }
}

std::optional<std::string> IoUringFuseTransport::getMaybeSetupError(
    uint32_t queueDepth,
    int fuseFd) {
  int eventFd = -1;
  io_uring ring = {};
  bool ringInitialized = false;

  auto maybeError =
      setupQueue(queueDepth, fuseFd, eventFd, ring, ringInitialized);

  // The preflight uses a temporary ring and fd; tear them down immediately.
  if (ringInitialized) {
    io_uring_queue_exit(&ring);
  }
  if (eventFd >= 0) {
    (void)close(eventFd);
  }

  if (maybeError.has_value()) {
    return describeQueueSetupError(*maybeError);
  }
  return std::nullopt;
}

void IoUringFuseTransport::initializeRingPool(
    size_t queueCount,
    size_t maxRequestPayloadSize) {
  auto ringPool = std::make_unique<RingPool>();
  const auto stopFd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
  if (stopFd < 0) {
    throw std::system_error(
        errno,
        std::generic_category(),
        "failed to create companion FUSE reader wakeup fd");
  }
  ringPool->devFuseStopFd = folly::File{stopFd, /*ownsFd=*/true};
  ringPool->queueDepth = queueDepth_;
  ringPool->maxRequestPayloadSize = maxRequestPayloadSize;
  ringPool->queues.resize(queueCount);

  for (size_t queueId = 0; queueId < queueCount; ++queueId) {
    auto& queue = ringPool->queues[queueId];
    queue.pool = ringPool.get();
    queue.queueId = queueId;
    queue.entries.resize(queueDepth_);
  }

  std::unique_lock lock{ringPoolMutex_};
  ringPool_ = std::move(ringPool);
}

void IoUringFuseTransport::initializeSession(FuseChannel& channel) {
  folly::call_once(sessionInitFlag_, [&] {
    const auto bufferSize = channel.getTransportBufferSize();
    const auto maxRequestPayloadSize =
        bufferSize > 4096 ? bufferSize - 4096 : bufferSize;
    const auto queueCount =
        getConfiguredQueueCount(channel.getTransportWorkerThreadCount());
    initializeRingPool(queueCount, maxRequestPayloadSize);
  });
}

std::optional<std::string> IoUringFuseTransport::prepareAllQueues(
    FuseChannel& channel) {
  try {
    initializeSession(channel);
  } catch (const std::exception& ex) {
    destroyRingPool();
    return fmt::format("failed to prepare io_uring ring pool: {}", ex.what());
  }
  if (!ringPool_) {
    return "io_uring ring pool was not created";
  }

  // This all runs on one thread, so each queue is pinned to the CPU whose
  // worker will later claim it before its ring is created. io_uring_setup()
  // allocates the ring on the calling thread's NUMA node -- it passes
  // numa_node_id() as the preferred node, with no __GFP_THISNODE, so it is a
  // preference rather than a hard bind -- and without this the entire pool
  // would land on whichever node happens to run FUSE_INIT.
  //
  // The mapping must stay identical to the one processSession() uses, clamp
  // included, or queues end up deliberately placed on the wrong node instead
  // of merely unpinned. Reusing pinThreadToCpu() is what keeps them in sync.
  //
  // Only the ring is affected. The per-entry buffers dominate the footprint
  // (~1 MiB per queue against ~8 KiB for the ring) but are never written here,
  // so first-touch still places them on the worker that eventually uses them.
  //
  // Unpins on the way out. This thread goes on to call startWorkerThreads(),
  // and spawned threads inherit the creator's affinity mask -- the io_uring
  // workers re-pin themselves, but the invalidation threads never do and would
  // stay stuck on one CPU for the life of the mount.
  ScopedThreadAffinity restoreAffinity;
  const auto configuredCpuCount = get_nprocs_conf();
  // Skip pinning outright if the mask could not be captured. Losing NUMA
  // locality is a far better outcome than pinning with no way back.
  const bool pinToQueueCpu =
      restoreAffinity.canRestore() && configuredCpuCount > 0;

  const auto queueCount = ringPool_->queues.size();
  const auto fuseFd = channel.getFuseDeviceFd();
  for (auto& queue : ringPool_->queues) {
    if (pinToQueueCpu &&
        !pinThreadToCpu(
            queue.queueId, static_cast<size_t>(configuredCpuCount))) {
      // A failed pin leaves the *previous* queue's mask in place, which would
      // put this ring on a deliberately wrong node rather than an arbitrary
      // one. Fall back to the original mask so the placement degrades to
      // unpinned.
      restoreAffinity.restore();
    }
    try {
      if (!queue.ringInitialized) {
        createQueueRing(queue, fuseFd);
      }
      if (!queue.buffersAllocated) {
        allocateQueueBuffers(queue);
      }
    } catch (const std::exception& ex) {
      auto error = fmt::format(
          "failed to prepare io_uring queue {} of {}: {}",
          queue.queueId,
          queueCount,
          ex.what());
      // Release the queues prepared so far rather than holding a partial pool
      // for the lifetime of the transport. sessionInitFlag_ stays set, so the
      // pool is not rebuilt later: this transport is spent and the caller is
      // expected to drop it.
      destroyRingPool();
      return error;
    }
  }

  XLOGF(DBG3, "prepared {} io_uring queues before FUSE_INIT", queueCount);
  return std::nullopt;
}

void IoUringFuseTransport::createQueueRing(RingQueue& queue, int fuseFd) const {
  int eventFd = -1;
  auto maybeSetupError = setupQueue(
      queueDepth_, fuseFd, eventFd, queue.ring, queue.ringInitialized);
  const auto previousEventFdState =
      queue.eventFd.exchange(eventFd, std::memory_order_acq_rel);
  if (previousEventFdState == kStopRequestedBeforeReady && eventFd >= 0) {
    // requestStopWakeup() ran before this queue's eventfd was published, so
    // that wakeup would otherwise have been silently dropped. Self-notify now
    // that the fd is known -- before throwing below on setup failure -- so
    // the worker doesn't block indefinitely in io_uring_submit_and_wait() if
    // it somehow reached that loop despite the error, and so the pending
    // wakeup isn't discarded by the exchange() above without ever being
    // acted on.
    notifyWorker(queue);
  }
  if (maybeSetupError.has_value()) {
    throwIoUringSetupError(*maybeSetupError, queue.queueId);
  }

  if (disableIoWait_) {
    // io_uring can't tell an idle-parked wait from one blocked on real I/O, so
    // this opts the whole ring out of iowait accounting. -EOPNOTSUPP means the
    // kernel lacks IORING_FEAT_NO_IOWAIT; leave iowait charging on in that
    // case.
    auto rc = ::io_uring_set_iowait(&queue.ring, false);
    if (rc && rc != -EOPNOTSUPP) {
      XLOGF(
          ERR,
          "io_uring_set_iowait failed for queue {}: {}",
          queue.queueId,
          folly::errnoStr(-rc));
    }
  }
}

void IoUringFuseTransport::allocateQueueBuffers(RingQueue& queue) const {
  for (auto& entry : queue.entries) {
    initializeEntryBuffers(queue, entry);
  }
  queue.buffersAllocated = true;
}

void IoUringFuseTransport::registerQueueWithFuse(RingQueue& queue) const {
  prepareFetchRequests(queue);
  auto rc = io_uring_submit(&queue.ring);
  if (rc < 0) {
    throw std::system_error(
        -rc,
        std::generic_category(),
        fmt::format(
            "failed to submit initial io_uring fetch SQEs for queue {}",
            queue.queueId));
  }

  XLOGF(
      DBG6,
      "io_uring worker ready queueId={} queueDepth={} submitted={} eventFd={} ringFd={}",
      queue.queueId,
      queue.entries.size(),
      rc,
      queue.eventFd.load(std::memory_order_acquire),
      queue.ring.ring_fd);
}

void IoUringFuseTransport::initializeQueueForWorker(
    RingQueue& queue,
    int fuseFd) const {
  XLOGF(
      DBG6,
      "io_uring worker initializing queueId={} queueDepth={} payloadSize={}",
      queue.queueId,
      queue.entries.size(),
      queue.pool->maxRequestPayloadSize);

  // The ring and buffers may already exist if they were set up ahead of
  // FUSE_INIT; only do the work here that the worker is first to reach.
  if (!queue.ringInitialized) {
    createQueueRing(queue, fuseFd);
  }
  if (!queue.buffersAllocated) {
    allocateQueueBuffers(queue);
  }
  registerQueueWithFuse(queue);
}

void IoUringFuseTransport::initializeEntryBuffers(
    RingQueue& queue,
    RingEntry& entry) const {
  entry.pool = queue.pool;
  entry.queueId = queue.queueId;
  entry.requestHeaderStorage.reset(
      allocatePageAlignedBuffer(queue.requestHeaderSize));
  entry.payloadStorage.reset(
      allocatePageAlignedBuffer(queue.pool->maxRequestPayloadSize));

  entry.requestHeader =
      static_cast<fuse_uring_req_header*>(entry.requestHeaderStorage.get());
  entry.payload = entry.payloadStorage.get();
  entry.payloadSize = queue.pool->maxRequestPayloadSize;
  entry.requestCommitId = 0;
  entry.phase = RingEntry::Phase::Idle;

  entry.iov[0].iov_base = entry.requestHeader;
  entry.iov[0].iov_len = queue.requestHeaderSize;
  entry.iov[1].iov_base = entry.payload;
  entry.iov[1].iov_len = entry.payloadSize;
}

void IoUringFuseTransport::prepareFetchRequests(RingQueue& queue) const {
  for (auto& entry : queue.entries) {
    prepareFetchRequest(queue, entry);
  }

  const auto sqReady = io_uring_sq_ready(&queue.ring);
  if (sqReady != queue.entries.size()) {
    throw std::runtime_error(
        fmt::format(
            "io_uring queue {} prepared {} SQEs for {} fetch requests",
            queue.queueId,
            sqReady,
            queue.entries.size()));
  }

  prepareWakePollSqe(queue);
}

void IoUringFuseTransport::prepareWakePollSqe(RingQueue& queue) const {
  auto* sqe = io_uring_get_sqe(&queue.ring);
  if (!sqe) {
    throw std::runtime_error(
        fmt::format(
            "failed to get io_uring SQE for queue {} eventfd poll",
            queue.queueId));
  }

  io_uring_prep_poll_add(sqe, queue.eventFd, POLLIN);
  io_uring_sqe_set_data(sqe, &queue);
}

void IoUringFuseTransport::prepareFetchRequest(
    RingQueue& queue,
    RingEntry& entry) const {
  auto* sqe = io_uring_get_sqe(&queue.ring);
  if (!sqe) {
    throw std::runtime_error(
        fmt::format(
            "failed to get io_uring SQE for queue {} fetch registration",
            queue.queueId));
  }

  prepareUringCmdSqe(
      sqe,
      FUSE_IO_URING_CMD_REGISTER,
      static_cast<uint16_t>(queue.queueId),
      /* commitId */ 0,
      &entry);
  markRegisterFetchSubmission(entry);
  sqe->addr = reinterpret_cast<uint64_t>(entry.iov.data());
  sqe->len = static_cast<__u32>(entry.iov.size());
}

IoUringFuseTransport::CqeResult IoUringFuseTransport::handleCqe(
    RingQueue& queue,
    const io_uring_cqe& cqe,
    bool stopRequested) const {
  auto* userData = io_uring_cqe_get_data(&cqe);
  if (isWakeEventCqe(queue, cqe, userData)) {
    return handleWakeEventCqe(queue);
  }
  if (cqe.res != 0) {
    if (cqe.res == -ECANCELED) {
      logCanceledCqe(queue, cqe, stopRequested, userData);
      if (shouldRecoverCanceledCommitAndFetchCqe(
              queue, userData, stopRequested)) {
        auto& entry = *static_cast<RingEntry*>(userData);
        recoverCanceledCommitAndFetchCqe(queue, entry);
        return {.action = CqeResult::Action::Ignored, .request = std::nullopt};
      }
    }

    if (cqe.res == -ENOTCONN && !stopRequested) {
      return {
          .action = CqeResult::Action::StopRequested, .request = std::nullopt};
    }

    if (shouldIgnoreCqeError(cqe.res, stopRequested)) {
      return {.action = CqeResult::Action::Ignored, .request = std::nullopt};
    }

    throw std::system_error(
        -cqe.res,
        std::generic_category(),
        fmt::format(
            "io_uring CQE failed on queue {} with result {}",
            queue.queueId,
            cqe.res));
  }

  if (!userData) {
    throw std::runtime_error(
        fmt::format(
            "io_uring CQE on queue {} had no request data", queue.queueId));
  }

  auto& entry = *static_cast<RingEntry*>(userData);
  auto& in = *reinterpret_cast<fuse_in_header*>(&entry.requestHeader->in_out);
  auto& ringInOut = entry.requestHeader->ring_ent_in_out;
  entry.requestCommitId = ringInOut.commit_id;
  if (entry.requestCommitId == 0) {
    throw std::runtime_error(
        fmt::format(
            "io_uring request on queue {} returned commit_id=0",
            queue.queueId));
  }

  if (in.len < sizeof(fuse_in_header)) {
    throw std::runtime_error(
        fmt::format(
            "io_uring request on queue {} was truncated: len={}",
            queue.queueId,
            in.len));
  }

  const auto argumentSize =
      static_cast<size_t>(in.len) - sizeof(fuse_in_header);
  const auto payloadSize = static_cast<size_t>(ringInOut.payload_sz);
  if (payloadSize > entry.payloadSize) {
    throw std::runtime_error(
        fmt::format(
            "io_uring request on queue {} has payload {} larger than buffer {}",
            queue.queueId,
            payloadSize,
            entry.payloadSize));
  }
  if (payloadSize > argumentSize) {
    throw std::runtime_error(
        fmt::format(
            "io_uring request on queue {} has payload {} larger than arg size {}",
            queue.queueId,
            payloadSize,
            argumentSize));
  }

  const auto opHeaderSize = argumentSize - payloadSize;
  if (opHeaderSize > sizeof(entry.requestHeader->op_in)) {
    throw std::runtime_error(
        fmt::format(
            "io_uring request on queue {} has op header {} larger than buffer {}",
            queue.queueId,
            opHeaderSize,
            sizeof(entry.requestHeader->op_in)));
  }

  DecodedRequest request;
  request.entry = &entry;
  request.header = in;
  request.arguments.resize(argumentSize);
  if (opHeaderSize > 0) {
    std::memcpy(
        request.arguments.data(), entry.requestHeader->op_in, opHeaderSize);
  }
  if (payloadSize > 0) {
    std::memcpy(
        request.arguments.data() + opHeaderSize, entry.payload, payloadSize);
  }

  markDecodedRequest(entry);
  return {
      .action = CqeResult::Action::DispatchRequest,
      .request = std::move(request)};
}

IoUringFuseTransport::CqeResult IoUringFuseTransport::handleWakeEventCqe(
    RingQueue& queue) const {
  eventfd_t value = 0;
  if (eventfd_read(queue.eventFd, &value) != 0) {
    throw std::system_error(
        errno,
        std::generic_category(),
        fmt::format(
            "failed to drain io_uring wake eventfd for queue {}",
            queue.queueId));
  }

  prepareWakePollSqe(queue);
  return {.action = CqeResult::Action::Ignored, .request = std::nullopt};
}

void IoUringFuseTransport::rejectDecodedRequestAfterStop(
    const DecodedRequest& request,
    const EdenStatsPtr& stats) const {
  auto& entry = *request.entry;
  auto& out = getReplyHeader(entry);
  auto& ringInOut = getRingEntryInOut(entry);

  out.error = -EIO;
  out.unique = request.header.unique;
  out.len = 0;
  ringInOut.payload_sz = 0;

  queueCommitAndFetch(entry, stats);
}

void IoUringFuseTransport::registerOutstandingEntry(
    uint64_t unique,
    RingEntry& entry) const {
  auto outstandingEntries = outstandingShard(unique).wlock();
  auto [it, inserted] = outstandingEntries->emplace(unique, &entry);
  if (!inserted) {
    throw std::runtime_error(
        fmt::format(
            "duplicate io_uring outstanding request unique={}", unique));
  }
  (void)it;
}

IoUringFuseTransport::RingEntry& IoUringFuseTransport::takeOutstandingEntry(
    uint64_t unique) const {
  auto outstandingEntries = outstandingShard(unique).wlock();
  auto it = outstandingEntries->find(unique);
  if (it == outstandingEntries->end()) {
    throw std::runtime_error(
        fmt::format("unknown io_uring outstanding request unique={}", unique));
  }
  auto* entry = it->second;
  outstandingEntries->erase(it);
  return *entry;
}

void IoUringFuseTransport::queueCommitAndFetch(
    RingEntry& entry,
    const EdenStatsPtr& stats) const {
  auto& queue = entry.pool->queues.at(entry.queueId);
  const bool sameThread = std::this_thread::get_id() == queue.ownerThreadId;
  if (sameThread) {
    stats->increment(&FuseStats::ioUringReplySameThread);
  } else {
    stats->increment(&FuseStats::ioUringReplyCrossThread);
  }
  {
    auto pendingCommits = queue.pendingCommits->wlock();
    pendingCommits->push_back(&entry);
  }
  // The owner thread drains pendingCommits after its CQE loop and before
  // the next io_uring_submit_and_wait, so a reply queued from the owner
  // thread itself needs no eventfd nudge. Only cross-thread replies must
  // wake the worker.
  if (!sameThread || !skipSelfWakeup_) {
    notifyWorker(queue);
  }
}

void IoUringFuseTransport::processPendingCommits(RingQueue& queue) const {
  std::vector<RingEntry*> pendingEntries;
  {
    auto pendingCommits = queue.pendingCommits->wlock();
    pendingEntries.swap(*pendingCommits);
  }

  if (pendingEntries.empty()) {
    return;
  }

  for (auto* entry : pendingEntries) {
    prepareCommitAndFetchSqe(queue, *entry);
  }

  auto rc = io_uring_submit(&queue.ring);
  if (rc < 0) {
    throw std::system_error(
        -rc,
        std::generic_category(),
        fmt::format(
            "failed to submit io_uring commit SQEs on queue {} while batching {} commits",
            queue.queueId,
            pendingEntries.size()));
  }
}

bool IoUringFuseTransport::hasPendingCommits(const RingQueue& queue) const {
  auto pendingCommits = queue.pendingCommits->rlock();
  return !pendingCommits->empty();
}

bool IoUringFuseTransport::shouldExitWorkerLoop(
    const FuseChannel& channel,
    const RingQueue& queue) const {
  if (!channel.isStopRequested()) {
    return false;
  }
  if (hasPendingCommits(queue)) {
    return false;
  }
  return !channel.hasPendingRequests();
}

void IoUringFuseTransport::notifyWorker(const RingQueue& queue) const {
  // Every caller (queueCommitAndFetch(), the self-notify in
  // createQueueRing(), and requestQueueStopWakeup()'s fallback) already
  // guarantees the eventfd is published before calling this. Don't silently
  // ignore an unpublished/invalid fd here, since that would mask a genuine
  // caller bug instead of surfacing it.
  const auto eventFd = queue.eventFd.load(std::memory_order_acquire);
  if (eventfd_write(eventFd, 1) != 0) {
    throw std::system_error(
        errno,
        std::generic_category(),
        fmt::format(
            "failed to notify io_uring worker for queue {}", queue.queueId));
  }
}

void IoUringFuseTransport::requestQueueStopWakeup(RingQueue& queue) const {
  auto fd = queue.eventFd.load(std::memory_order_acquire);
  while (fd < 0 && fd != kStopRequestedBeforeReady) {
    // The owning worker hasn't published its eventfd yet. Record that a stop
    // wakeup is owed so createQueueRing() can self-notify once it publishes
    // the fd, instead of silently dropping this wakeup the way a plain
    // load-then-write would.
    if (queue.eventFd.compare_exchange_weak(
            fd,
            kStopRequestedBeforeReady,
            std::memory_order_acq_rel,
            std::memory_order_acquire)) {
      return;
    }
  }
  if (fd == kStopRequestedBeforeReady) {
    // Another call already recorded the pending wakeup.
    return;
  }
  notifyWorker(queue);
}

void IoUringFuseTransport::prepareCommitAndFetchSqe(
    RingQueue& queue,
    RingEntry& entry) const {
  auto* sqe = io_uring_get_sqe(&queue.ring);
  if (!sqe) {
    throw std::runtime_error(
        fmt::format(
            "failed to get io_uring SQE for queue {} commit", entry.queueId));
  }

  prepareUringCmdSqe(
      sqe,
      FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
      static_cast<uint16_t>(entry.queueId),
      entry.requestCommitId,
      &entry);
  markCommitAndFetchSubmission(entry);
}

fuse_out_header& IoUringFuseTransport::getReplyHeader(RingEntry& entry) {
  return *reinterpret_cast<fuse_out_header*>(&entry.requestHeader->in_out);
}

fuse_uring_ent_in_out& IoUringFuseTransport::getRingEntryInOut(
    RingEntry& entry) {
  return entry.requestHeader->ring_ent_in_out;
}

void IoUringFuseTransport::destroyRingPool() noexcept {
  // Take the pool out under the exclusive lock so this waits for any
  // requestStopWakeup() call already iterating queues under the shared lock
  // to finish first, and so any later requestStopWakeup() call observes
  // ringPool_ as null instead of a dangling pointer. The actual teardown
  // (io_uring_queue_exit(), closing fds) happens after releasing the lock,
  // since by then no reader can still be holding a reference to `pool`.
  std::unique_ptr<RingPool> pool;
  {
    std::unique_lock lock{ringPoolMutex_};
    pool = std::move(ringPool_);
  }
}

void* IoUringFuseTransport::allocatePageAlignedBuffer(size_t size) {
  if (size == 0) {
    throw std::invalid_argument(
        "io_uring page-aligned buffer size must be non-zero");
  }

  void* ptr = nullptr;
  auto rc = posix_memalign(&ptr, static_cast<size_t>(getpagesize()), size);
  if (rc != 0) {
    throw std::system_error(
        rc,
        std::generic_category(),
        fmt::format(
            "failed to allocate page-aligned io_uring buffer of size {}",
            size));
  }

  if (!ptr) {
    throw std::runtime_error(
        fmt::format(
            "posix_memalign returned null for io_uring buffer of size {}",
            size));
  }

  return ptr;
}

bool IoUringFuseTransport::isTransientSubmitAndWaitError(int result) {
  return result == -EINTR || result == -EAGAIN;
}

bool IoUringFuseTransport::shouldRetrySubmitAndWaitError(
    int result,
    bool stopRequested) {
  return !stopRequested && isTransientSubmitAndWaitError(result);
}

bool IoUringFuseTransport::shouldIgnoreSubmitAndWaitError(
    int result,
    bool stopRequested) {
  return stopRequested && isTransientSubmitAndWaitError(result);
}

bool IoUringFuseTransport::shouldIgnoreCqeError(int result) {
  return result == -EINTR || result == -EOPNOTSUPP || result == -EAGAIN;
}

bool IoUringFuseTransport::shouldIgnoreCqeErrorDuringShutdown(int result) {
  return result == -ECANCELED || result == -ENOTCONN;
}

bool IoUringFuseTransport::shouldIgnoreCqeError(
    int result,
    bool stopRequested) {
  return shouldIgnoreCqeError(result) ||
      (stopRequested && shouldIgnoreCqeErrorDuringShutdown(result));
}

bool IoUringFuseTransport::isWakeEventCqe(
    const RingQueue& queue,
    const io_uring_cqe& cqe,
    void* userData) {
  return cqe.res > 0 && userData == &queue;
}

void IoUringFuseTransport::markRegisterFetchSubmission(RingEntry& entry) const {
  entry.requestCommitId = 0;
  entry.phase = RingEntry::Phase::RegisterFetchInFlight;
}

void IoUringFuseTransport::markDecodedRequest(RingEntry& entry) const {
  entry.phase = RingEntry::Phase::RequestOutstanding;
}

void IoUringFuseTransport::markCommitAndFetchSubmission(
    RingEntry& entry) const {
  entry.phase = RingEntry::Phase::CommitAndFetchInFlight;
}

bool IoUringFuseTransport::shouldRecoverCanceledCommitAndFetchCqe(
    const RingQueue& queue,
    void* userData,
    bool stopRequested) const {
  if (stopRequested || !userData) {
    return false;
  }

  if (userData == &queue) {
    return false;
  }

  const auto& entry = *static_cast<RingEntry*>(userData);
  return entry.phase == RingEntry::Phase::CommitAndFetchInFlight;
}

void IoUringFuseTransport::recoverCanceledCommitAndFetchCqe(
    RingQueue& queue,
    RingEntry& entry) const {
  XLOGF(
      WARN,
      "recovering canceled io_uring commit_and_fetch CQE for queueId={} entry={} commitId={} phase={}",
      queue.queueId,
      static_cast<void*>(&entry),
      entry.requestCommitId,
      static_cast<int>(entry.phase));
  prepareFetchRequest(queue, entry);
}

bool IoUringFuseTransport::isEntryOutstanding(const RingEntry& entry) const {
  for (const auto& shard : outstandingEntries_) {
    auto outstandingEntries = shard.entries.rlock();
    for (const auto& [unique, candidate] : *outstandingEntries) {
      (void)unique;
      if (candidate == &entry) {
        return true;
      }
    }
  }
  return false;
}

void IoUringFuseTransport::logCanceledCqe(
    const RingQueue& queue,
    const io_uring_cqe& cqe,
    bool stopRequested,
    void* userData) const {
  const auto isEventFd = userData == &queue;
  if (!userData || isEventFd) {
    XLOGF(
        ERR,
        "io_uring canceled CQE queueId={} stopRequested={} res={} flags=0x{:x} userData={} eventFd={} isEventFd={}",
        queue.queueId,
        stopRequested,
        cqe.res,
        cqe.flags,
        userData,
        queue.eventFd.load(),
        isEventFd);
    return;
  }

  const auto& entry = *static_cast<RingEntry*>(userData);
  const auto& ringInOut = entry.requestHeader->ring_ent_in_out;
  const auto& bestEffortIn =
      *reinterpret_cast<const fuse_in_header*>(&entry.requestHeader->in_out);
  const auto& bestEffortOut =
      *reinterpret_cast<const fuse_out_header*>(&entry.requestHeader->in_out);
  XLOGF(
      ERR,
      "io_uring canceled CQE queueId={} stopRequested={} res={} flags=0x{:x} userData={} entry.queueId={} phase={} requestCommitId={} outstanding={} bestEffortIn.unique={} bestEffortIn.opcode={} bestEffortIn.len={} ringInOut.commitId={} ringInOut.payloadSz={} bestEffortOut.unique={} bestEffortOut.error={} bestEffortOut.len={}",
      queue.queueId,
      stopRequested,
      cqe.res,
      cqe.flags,
      userData,
      entry.queueId,
      static_cast<int>(entry.phase),
      entry.requestCommitId,
      isEntryOutstanding(entry),
      bestEffortIn.unique,
      bestEffortIn.opcode,
      bestEffortIn.len,
      ringInOut.commit_id,
      ringInOut.payload_sz,
      bestEffortOut.unique,
      bestEffortOut.error,
      bestEffortOut.len);
}

size_t IoUringFuseTransport::getConfiguredQueueCount(
    size_t defaultThreadCount) {
  const auto configuredCpuCount = get_nprocs_conf();
  if (configuredCpuCount <= 0) {
    return defaultThreadCount;
  }
  return static_cast<size_t>(configuredCpuCount);
}
#endif

const char* IoUringFuseTransport::getName() const {
  return kIoUringFuseTransportName;
}

size_t IoUringFuseTransport::getWorkerThreadCount(
    size_t defaultThreadCount) const {
#if EDEN_HAVE_FUSE_IO_URING
  // FORGETs still arrive on /dev/fuse, so keep one dedicated reader in
  // addition to the io_uring queue workers.
  return getConfiguredQueueCount(defaultThreadCount) + 1;
#else
  return defaultThreadCount;
#endif
}

void IoUringFuseTransport::requestStopWakeup() {
#if EDEN_HAVE_FUSE_IO_URING
  // May run on a thread that never called initializeSession()/
  // initializeRingPool(), so ringPool_ itself (a plain unique_ptr) cannot be
  // read directly here without racing its publication in initializeRingPool()
  // or its teardown in destroyRingPool(). Hold the shared lock for the
  // entire loop below (not just to fetch the pointer) so destroyRingPool()
  // cannot free the pool out from under an in-progress iteration.
  std::shared_lock lock{ringPoolMutex_};
  if (!ringPool_) {
    return;
  }

  int result;
  do {
    result = eventfd_write(ringPool_->devFuseStopFd.fd(), 1);
  } while (result < 0 && errno == EINTR);
  if (result < 0 && errno != EAGAIN) {
    XLOGF(
        ERR,
        "failed to wake companion FUSE reader during shutdown: {}",
        folly::errnoStr(errno));
  }

  for (auto& queue : ringPool_->queues) {
    try {
      requestQueueStopWakeup(queue);
    } catch (const std::exception& ex) {
      XLOGF(
          ERR,
          "failed to wake io_uring queue {} during shutdown: {}",
          queue.queueId,
          ex.what());
    }
  }
#endif
}

ssize_t IoUringFuseTransport::readInitPacket(
    int /* fd */,
    void* /* buf */,
    size_t /* size */) const {
  // Not implemented: FUSE_INIT still uses the classic /dev/fuse path.
  throwIoUringNotImplemented("readInitPacket", queueDepth_);
}

void IoUringFuseTransport::processSession(FuseChannel& channel) {
#if EDEN_HAVE_FUSE_IO_URING
  initializeSession(channel);

  const auto workerId = nextWorkerId_.fetch_add(1, std::memory_order_acq_rel);
  if (!ringPool_ || workerId > ringPool_->queues.size()) {
    throw std::runtime_error(
        fmt::format(
            "failed to assign io_uring worker {} (queue_count={})",
            workerId,
            ringPool_ ? ringPool_->queues.size() : 0));
  }

  const auto workerCount = ringPool_->queues.size() + 1;
  if (workerId == 0) {
    // FORGET and other control requests remain on /dev/fuse after io_uring
    // negotiation. This reader shares the managed workers' stop/join lifetime.
    devFuseTransport_.processSession(
        channel, ringPool_->devFuseStopFd.fd(), [&] {
          channel.notifyTransportWorkerReady(
              ringPool_->queues.size(), workerCount);
        });
    return;
  }

  const auto queueId = workerId - 1;
  auto& queue = ringPool_->queues[queueId];
  queue.ownerThreadId = std::this_thread::get_id();
  const auto myPid = getpid();
  const auto configuredCpuCount = get_nprocs_conf();
  XLOGF(
      DBG6,
      "io_uring worker claimed queueId={} queueCount={} queueDepth={} configuredCpuCount={}",
      queue.queueId,
      ringPool_->queues.size(),
      queue.entries.size(),
      configuredCpuCount);
  if (configuredCpuCount > 0) {
    pinThreadToCpu(queue.queueId, static_cast<size_t>(configuredCpuCount));
  }

  initializeQueueForWorker(queue, channel.getFuseDeviceFd());
  channel.notifyTransportWorkerReady(queue.queueId, workerCount);

  while (true) {
    processPendingCommits(queue);
    if (shouldExitWorkerLoop(channel, queue)) {
      break;
    }

    auto rc = io_uring_submit_and_wait(&queue.ring, 1);
    if (rc < 0) {
      const auto stopRequested = channel.isStopRequested();
      if (shouldRetrySubmitAndWaitError(rc, stopRequested)) {
        continue;
      }
      if (shouldIgnoreSubmitAndWaitError(rc, stopRequested)) {
        if (shouldExitWorkerLoop(channel, queue)) {
          break;
        }
        continue;
      }
      throw std::system_error(
          -rc,
          std::generic_category(),
          fmt::format(
              "io_uring_submit_and_wait failed for queue {}", queue.queueId));
    }

    unsigned completed = 0;
    unsigned head = 0;
    io_uring_cqe* cqe = nullptr;
    io_uring_for_each_cqe(&queue.ring, head, cqe) {
      ++completed;
      if (!cqe) {
        throw std::runtime_error(
            fmt::format(
                "io_uring CQE iteration returned null on queue {}",
                queue.queueId));
      }
      auto result = handleCqe(queue, *cqe, channel.isStopRequested());
      switch (result.action) {
        case CqeResult::Action::DispatchRequest: {
          // A DispatchRequest action always carries a decoded request.
          if (!result.request.has_value()) {
            throw std::runtime_error(
                fmt::format(
                    "io_uring dispatch on queue {} produced no request",
                    queue.queueId));
          }
          auto& request = *result.request;
          if (channel.isStopRequested()) {
            // COMMIT_AND_FETCH can return another request while shutdown is
            // draining. Recycle it with an error instead of dispatching new
            // work into FuseChannel after stop was requested.
            rejectDecodedRequestAfterStop(request, channel.getStats());
            break;
          }
          registerOutstandingEntry(request.header.unique, *request.entry);
          channel.dispatchRequestFromTransport(
              *this,
              request.header,
              folly::ByteRange{
                  request.arguments.data(), request.arguments.size()},
              myPid);
          break;
        }
        case CqeResult::Action::Ignored:
          break;
        case CqeResult::Action::StopRequested:
          channel.requestSessionExitFromTransport(
              FuseChannel::StopReason::UNMOUNTED);
          break;
      }
    }
    if (completed > 0) {
      io_uring_cq_advance(&queue.ring, completed);
    }

    processPendingCommits(queue);
  }
#else
  (void)channel;
  throwIoUringNotImplemented("processSession", queueDepth_);
#endif
}

void IoUringFuseTransport::replyError(
    FuseChannel& channel,
    const fuse_in_header& request,
    int errorCode) const {
#if EDEN_HAVE_FUSE_IO_URING
  auto& entry = takeOutstandingEntry(request.unique);
  auto& out = getReplyHeader(entry);
  auto& ringInOut = getRingEntryInOut(entry);

  out.error = -errorCode;
  out.unique = request.unique;
  out.len = 0;
  ringInOut.payload_sz = 0;

  queueCommitAndFetch(entry, channel.getStats());
#else
  (void)channel;
  (void)request;
  (void)errorCode;
  throwIoUringNotImplemented("replyError", queueDepth_);
#endif
}

void IoUringFuseTransport::replyNone(
    FuseChannel& channel,
    const fuse_in_header& request) const {
  // A request without a FUSE reply still owns a ring entry to recycle.
  replyError(channel, request, 0);
}

void IoUringFuseTransport::sendRawReply(
    FuseChannel& channel,
    const iovec iov[],
    size_t count) const {
#if EDEN_HAVE_FUSE_IO_URING
  if (count == 0) {
    throw std::runtime_error("io_uring reply requires at least one iovec");
  }

  const auto& sourceHeader =
      *static_cast<const fuse_out_header*>(iov[0].iov_base);
  auto& entry = takeOutstandingEntry(sourceHeader.unique);
  auto& out = getReplyHeader(entry);
  auto& ringInOut = getRingEntryInOut(entry);

  size_t payloadLength = 0;
  int error = sourceHeader.error;
  auto* payload = static_cast<uint8_t*>(entry.payload);
  for (size_t idx = 1; idx < count; ++idx) {
    if (iov[idx].iov_len > entry.payloadSize - payloadLength) {
      XLOGF(
          WARN,
          "io_uring reply payload exceeds buffer: unique={} iov_index={} iov_len={} copied_payload_length={} payload_buffer_size={}",
          sourceHeader.unique,
          idx,
          iov[idx].iov_len,
          payloadLength,
          entry.payloadSize);
      error = -EINVAL;
      payloadLength = 0;
      break;
    }

    std::memcpy(payload + payloadLength, iov[idx].iov_base, iov[idx].iov_len);
    payloadLength += iov[idx].iov_len;
  }

  if (payloadLength > std::numeric_limits<uint32_t>::max()) {
    XLOGF(
        WARN,
        "io_uring reply payload exceeds uint32_t limit: unique={} payload_length={}",
        sourceHeader.unique,
        payloadLength);
    error = -EINVAL;
    payloadLength = 0;
  }
  const auto payloadLength32 = static_cast<uint32_t>(payloadLength);

  out.error = error;
  out.unique = sourceHeader.unique;
  out.len = payloadLength32;
  ringInOut.payload_sz = payloadLength32;

  queueCommitAndFetch(entry, channel.getStats());
#else
  (void)channel;
  (void)iov;
  (void)count;
  throwIoUringNotImplemented("sendRawReply", queueDepth_);
#endif
}

} // namespace facebook::eden

#endif
