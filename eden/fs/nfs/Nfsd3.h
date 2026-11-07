/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

// Implementation of the NFSv3 protocol as described in:
// https://tools.ietf.org/html/rfc1813

#include <array>
#include <chrono>
#include <optional>
#include <vector>

#include <folly/ExceptionWrapper.h>
#include <folly/Synchronized.h>
#include <folly/container/F14Map.h>
#include <folly/futures/Future.h>
#include "eden/common/telemetry/TraceBus.h"
#include "eden/common/utils/CaseSensitivity.h"
#include "eden/fs/inodes/FsChannel.h"
#include "eden/fs/nfs/NfsDispatcher.h"
#include "eden/fs/nfs/rpc/RpcServer.h"
#include "eden/fs/utils/InvalidationQueue.h"
#include "eden/fs/utils/ProcessAccessLog.h"
#include "folly/Function.h"

namespace folly {
class Executor;
}

namespace facebook::eden {

class ErrorLogger;
class Notifier;
class PrivHelper;
class ProcessInfoCache;
class EdenFsEventsLogger;
class ReloadableConfig;

namespace detail {
/**
 * Log SERVERFAULT errors to structured error telemetry.
 * Normal filesystem errors (ENOENT, EACCES, etc.) are not logged.
 */
void logNfsError(
    nfsstat3 error,
    const folly::exception_wrapper& ex,
    ErrorLogger& errorLogger,
    uint64_t inode,
    const AbsolutePath& mountPath);
} // namespace detail

using TraceDetailedArgumentsHandle = std::shared_ptr<void>;

enum class NfsInvalidationSource : uint8_t {
  Gc,
};

struct NfsArgsDetails {
  /* implicit */ NfsArgsDetails(
      std::string argStr,
      std::optional<InodeNumber> ino = std::nullopt)
      : str{std::move(argStr)}, inode{ino} {}

  std::string str;
  std::optional<InodeNumber> inode;
};

struct NfsTraceEvent : TraceEventBase {
  enum Type : unsigned char {
    START,
    FINISH,
  };

  NfsTraceEvent() = delete;

  static NfsTraceEvent start(uint32_t xid, uint32_t procNumber) {
    return NfsTraceEvent{
        xid, procNumber, StartDetails{std::unique_ptr<NfsArgsDetails>{}}};
  }

  static NfsTraceEvent
  start(uint32_t xid, uint32_t procNumber, NfsArgsDetails&& args) {
    return NfsTraceEvent{
        xid, procNumber, StartDetails{std::make_unique<NfsArgsDetails>(args)}};
  }

  static NfsTraceEvent finish(uint32_t xid, uint32_t procNumber) {
    return NfsTraceEvent{xid, procNumber, FinishDetails{}};
  }

  Type getType() const {
    return std::holds_alternative<StartDetails>(details_) ? Type::START
                                                          : Type::FINISH;
  }

  uint32_t getXid() const {
    return xid_;
  }

  uint32_t getProcNumber() const {
    return procNumber_;
  }

  // `getArguments` and `getInode` must only be called on a start event. The
  // caller is responsible to check this.
  std::optional<folly::StringPiece> getArguments() const {
    auto& argDetails = std::get<StartDetails>(details_).argDetails;
    return argDetails ? std::make_optional<folly::StringPiece>(argDetails->str)
                      : std::nullopt;
  }
  std::optional<InodeNumber> getInode() const {
    auto& argDetails = std::get<StartDetails>(details_).argDetails;
    return argDetails ? argDetails->inode : std::nullopt;
  }

 private:
  struct StartDetails {
    explicit StartDetails(std::unique_ptr<NfsArgsDetails> args)
        : argDetails{std::move(args)} {}
    std::unique_ptr<NfsArgsDetails> argDetails;
  };

  struct FinishDetails {};

  using Details = std::variant<StartDetails, FinishDetails>;

  NfsTraceEvent(uint32_t xid, uint32_t procNumber, Details&& details)
      : xid_{xid}, procNumber_{procNumber}, details_{std::move(details)} {}

  uint32_t xid_;
  uint32_t procNumber_;
  Details details_;
};

/**
 * The directories whose GC invalidation chmod is currently running, with
 * their ancestors: the inodes the kernel may name while it resolves and
 * authorizes the chmod, see Nfsd3::invalidateWithQueueLimit().
 */
class InvalidatingInodes {
 public:
  /**
   * Register a running invalidation of lineage[0], whose ancestors follow.
   * forget runs when the SETATTR the chmod turns into arrives, see
   * takeForget(). GC invalidates a directory once per pass, so no other
   * invalidation of lineage[0] may be registered at the time.
   */
  void add(
      const std::vector<InodeNumber>& lineage,
      folly::Function<void()> forget);
  void remove(const std::vector<InodeNumber>& lineage);
  /** Whether the inode is a directory being invalidated or an ancestor. */
  bool contains(InodeNumber ino) const;
  /**
   * The forget callback of the running invalidation of this directory, if it
   * has not been taken yet: null for an ancestor, and for the directory once
   * a SETATTR has taken it. The SETATTR handler takes it exactly once, so a
   * second no-op SETATTR of the directory while its chmod runs is answered
   * normally and changes nothing.
   */
  folly::Function<void()> takeForget(InodeNumber ino);

 private:
  struct Entry {
    /** Number of running invalidations naming the inode. */
    uint32_t count{0};
    /** Set while the inode is the directory being invalidated itself. */
    folly::Function<void()> forget;
  };
  folly::Synchronized<folly::F14FastMap<InodeNumber, Entry>> inodes_;
  /** Number of running invalidations, so requests skip the lock while idle. */
  std::atomic<size_t> numLineages_{0};
};

class FaultInjector;

/**
 * Requests received but not yet answered, for `debugOutstandingNfsCalls`.
 * Striped by xid so the worker threads rarely contend on one lock.
 */
class NfsRequestTracker {
 public:
  struct OutstandingRequest {
    uint32_t xid;
    std::chrono::steady_clock::time_point requestStartTime;
  };

  void start(uint32_t xid, std::chrono::steady_clock::time_point startTime);

  /**
   * Forget the request. A xid that was never started, or was already
   * finished, is ignored.
   */
  void finish(uint32_t xid);

  std::vector<OutstandingRequest> outstanding() const;

 private:
  static constexpr size_t kStripes = 16;

  struct State {
    folly::F14FastMap<uint32_t, OutstandingRequest> requests;
  };

  std::array<folly::Synchronized<State>, kStripes> stripes_;
};

class Nfsd3 final : public FsChannel {
 public:
  /**
   * Create a new RPC NFSv3 program.
   *
   * All the socket processing will be run on the EventBase passed in. This
   * also must be called on that EventBase thread.
   *
   * Note: at mount time, EdenFS will manually call mount.nfs with -o port
   * to manually specify the port on which this server is bound, so registering
   * is not necessary for a properly behaving EdenFS.
   */
  Nfsd3(
      PrivHelper* privHelper,
      AbsolutePath mountPath,
      folly::EventBase* evb,
      std::shared_ptr<folly::Executor> threadPool,
      std::unique_ptr<NfsDispatcher> dispatcher,
      const folly::Logger* straceLogger,
      std::shared_ptr<ProcessInfoCache> processInfoCache,
      const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
      ErrorLogger& errorLogger,
      folly::Duration requestTimeout,
      std::shared_ptr<Notifier> notifications,
      CaseSensitivity caseSensitive,
      uint32_t readIoSize,
      uint32_t writeIoSize,
      size_t maximumInFlightRequests,
      std::chrono::nanoseconds highNfsRequestsLogInterval,
      std::chrono::nanoseconds longRunningFSRequestThreshold,
      size_t traceBusCapacity,
      bool fastPathRPCs,
      std::shared_ptr<ReloadableConfig> config,
      FaultInjector& faultInjector);

  void destroy() override;

  const char* getName() const override {
    return "nfs3";
  }

  [[nodiscard]] folly::Future<StopFuture> initialize() override;

  void initialize(folly::SocketAddress addr);
  void initialize(folly::File connectedSocket);

  /**
   * Uses the configured PrivHelper to unmount this NFS mount from the
   * filesystem.
   *
   * That causes Nfsd3's RpcServer to receive EOF from the NFS socket, which
   * shuts down the Nfsd3. The future returned by initialize() will be fulfilled
   * with a non-takeover StopData.
   */
  [[nodiscard]] folly::SemiFuture<folly::Unit> unmount(
      UnmountOptions /* options */) override;

  /**
   * Trigger an invalidation for the given path.
   *
   * To avoid a very large amount of traffic between an NFS client and the
   * server, the client will cache attributes that the server previously
   * returned for a file. This allows stat(2) calls to be fully resolved on the
   * client.
   *
   * NFS v3 does not support explicit invalidation. We are hacking this in.
   *
   * This invalidate method simply tries to chmod the given path in a
   * background thread.
   *
   * We rely on 2 things here:
   *   1. chmod goes all the way through the kernel to EdenFS. All "writes"
   *   seem to function this way.
   *   2. When the kernel sees the mtime in the post op attr in the response
   *   from EdenFS has updated in the response to chmod, it will drop its
   *   caches for the children of the directory. Mutations update the mtime
   *   before invalidating. Inode GC changes nothing, so its chmod is instead
   *   answered with a stale handle error, which makes the client drop those
   *   caches at once; see invalidateWithQueueLimit().
   *
   * 1. is implied by the NFS mode 2. isn't really guaranteed anywhere, but
   * this works well enough on Linux and macOS and we don't have many other
   * options.
   *
   * We used to just do an open call here. This was insufficient because the
   * open and subsequent reads can be served purely from cache on macOS.
   * This was sufficient on Linux as all open calls go to EdenFS and CTO
   * (close-to-open) guarantees from NFS guarantees the caches must be flushed.
   *
   * Note that the chmod(2) call runs asynchronously in a background thread as
   * both the kernel and EdenFS are holding locks that would otherwise cause
   * EdenFS to deadlock. The completeInvalidations method below should be called
   * with all the locks released to wait for all the invalidation to complete.
   *
   * @param path the path to invalidate
   * @param mode the mode to set. Use the previous mode to only invalidate.
   */
  void invalidate(
      AbsolutePath path,
      mode_t mode,
      std::optional<NfsInvalidationSource> source = std::nullopt);

  /**
   * Queue a GC invalidation like invalidate() with source Gc, but only once
   * the invalidation queue holds fewer than maxQueueSize entries, waiting for
   * it to drain otherwise. Returns false without queuing if cancellation was
   * requested first or the channel is stopping. Must not be called while
   * holding inode locks, since it can block.
   *
   * GC's chmod changes nothing, so the client would keep every name it has
   * cached for the directory; NFS has no way to tell it otherwise except a
   * stale handle error, on which the client drops the directory's own name
   * and all of its children's at once. So the SETATTR the chmod turns into is
   * answered NFS3ERR_STALE, and forget runs right before that reply: it is
   * GC's equivalent of the FORGET FUSE gets from the kernel, and clears the
   * FS references of the directory's children. A LOOKUP that arrives later
   * references a child anew, as a FUSE lookup would. The chmod then fails
   * with ESTALE, which counts as its success, and since it failed the kernel
   * emits no file system event for it.
   *
   * lineage holds the inode numbers of the directory and of its ancestors.
   * While the chmod runs, the requests the kernel makes on those inodes to
   * resolve and authorize it (GETATTR, ACCESS, negative LOOKUP, SETATTR) do
   * not refresh their last FS request time, so that GC's own work does not
   * make them look in use. Requests that resolve a directory's entries
   * always do.
   *
   * Returns a future that completes once the chmod has run and forget, if it
   * ran, has returned: with what forget returned, or with nullopt if the
   * chmod never reached EdenFS as a SETATTR. It fails if the channel stopped
   * before getting to the chmod. GC waits on it for each directory instead of
   * flushing the whole queue, so that with several invalidation threads the
   * chmods of unrelated directories overlap.
   */
  std::optional<folly::SemiFuture<std::optional<uint64_t>>>
  invalidateWithQueueLimit(
      AbsolutePath path,
      mode_t mode,
      folly::Function<uint64_t()> forget,
      std::vector<InodeNumber> lineage,
      size_t maxQueueSize,
      const folly::CancellationToken& cancellationToken);

  bool takeoverStop() override;

  ImmediateFuture<folly::Unit> waitForPendingWrites() override {
    return folly::unit;
  }

  folly::coro::now_task<folly::Unit> co_waitForPendingWrites() override {
    co_return folly::unit;
  }

  /*
   * Request that the kernel invalidate its cached data for the specified
   * paths+modes.
   *
   * This operation is performed asynchronously.  completeInvalidations() can be
   * called if you need to determine when this operation has completed.
   *
   * @param pathsAndModes a vector of each inodes path and mode to invalidate.
   */
  void invalidateInodes(
      const std::vector<std::pair<AbsolutePath, mode_t>>& pathsAndModes);

  /**
   * Wait for all pending invalidation to complete.
   *
   * The future will complete when all the previously triggered invalidation
   * completed.
   */
  ImmediateFuture<folly::Unit> completeInvalidations() override;

  folly::coro::now_task<folly::Unit> co_completeInvalidations() override;

  /**
   * Obtain the address that this NFSv3 program is listening on.
   */
  folly::SocketAddress getAddr() const {
    return server_->getAddr();
  }

  using OutstandingRequest = NfsRequestTracker::OutstandingRequest;

  using StopData = RpcStopData;

  /**
   * Return a future that will be triggered on unmount.
   */
  StopFuture getStopFuture();

  const std::shared_ptr<ProcessAccessLog>& getProcessAccessLog() override {
    return processAccessLog_;
  }

  Nfsd3(const Nfsd3&) = delete;
  Nfsd3(Nfsd3&&) = delete;
  Nfsd3& operator=(const Nfsd3&) = delete;
  Nfsd3& operator=(Nfsd3&&) = delete;

  /**
   * Returns the set of outstanding NFS requests.
   */
  std::vector<Nfsd3::OutstandingRequest> getOutstandingRequests();

  /**
   * While the returned handle is alive, NfsTraceEvents published on the
   * TraceBus will have detailed argument strings.
   */
  TraceDetailedArgumentsHandle traceDetailedArguments();

  TraceBus<NfsTraceEvent>& getTraceBus() {
    return *traceBus_;
  }

  /** How many threads send invalidation chmods, see
   * nfs:num-invalidation-threads. */
  size_t numInvalidationThreads() const {
    return invalidationQueue_.numWorkers();
  }

 private:
  /**
   * This is triggered when the kernel closes the socket. The socket is closed
   * when the privhelper or a user runs umount.
   */
  ~Nfsd3();

  PrivHelper* const privHelper_;
  AbsolutePath mountPath_;

  // Referenced by the RpcServerProcessor that server_ owns, so declared first.
  NfsRequestTracker requestTracker_;
  InvalidatingInodes invalidatingInodes_;
  FaultInjector& faultInjector_;

  folly::Promise<FsStopDataPtr> stopPromise_;
  EdenStatsPtr stats_;
  // Copied into the RpcServerProcessor that server_ owns, so declared first.
  std::shared_ptr<ProcessAccessLog> processAccessLog_;
  std::shared_ptr<RpcServer> server_;
  std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger_;
  std::atomic<size_t> traceDetailedArguments_;
  std::shared_ptr<TraceBus<NfsTraceEvent>> traceBus_;

  /**
   * One directory invalidation: a chmod of the directory to its current
   * mode, which makes the NFS client refetch its attributes.
   */
  struct Invalidation {
    AbsolutePath path;
    mode_t mode;
    std::optional<NfsInvalidationSource> source;
    /** GC only: the directory and its ancestors, and what to do when the
     * chmod reaches EdenFS as a SETATTR. */
    std::vector<InodeNumber> lineage;
    folly::Function<void()> forget;
    /** GC only: what forget returned, once it has run. Broken if it never
     * ran. */
    folly::SemiFuture<uint64_t> result{
        folly::SemiFuture<uint64_t>::makeEmpty()};
    /** Fulfilled once the chmod and its callback are done; empty for
     * invalidations nobody waits on. Broken if the entry is abandoned. */
    folly::Promise<std::optional<uint64_t>> done{
        folly::Promise<std::optional<uint64_t>>::makeEmpty()};
  };
  void runInvalidation(Invalidation& invalidation);

  // Declared last: its workers use the members above, so it must be stopped,
  // which its destructor does, before they are destroyed.
  InvalidationQueue<Invalidation> invalidationQueue_;
};

// Returns a view backed by the static NFS handler table.
folly::StringPiece nfsProcName(uint32_t procNumber);
ProcessAccessLog::AccessType nfsProcAccessType(uint32_t procNumber);
} // namespace facebook::eden
