/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/nfs/Nfsd3.h"

#include <algorithm>
#include <memory>
#include <string_view>
#include <type_traits>

#include <fb303/ThreadCachedServiceData.h>
#include <fmt/format.h>
#include <utility>

#include <folly/ScopeGuard.h>

#include <folly/String.h>
#include <folly/Synchronized.h>
#include <folly/Utility.h>
#include <folly/container/F14Map.h>
#include <folly/futures/Future.h>
#include <folly/portability/Stdlib.h>
#include "eden/common/utils/FaultInjector.h"

#include "eden/common/telemetry/RequestMetricsScope.h"
#include "eden/common/utils/IDGen.h"
#include "eden/common/utils/SystemError.h"
#include "eden/common/utils/Throw.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/nfs/NfsAccessRateLimiter.h"
#include "eden/fs/nfs/NfsRequestContext.h"
#include "eden/fs/nfs/NfsUtils.h"
#include "eden/fs/nfs/NfsdRpc.h"
#include "eden/fs/privhelper/PrivHelper.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/telemetry/EdenErrorInfoBuilder.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/telemetry/LogEvent.h"
#include "eden/fs/utils/Clock.h"
#include "eden/fs/utils/StaticAssert.h"

#ifdef __linux__
#include <sys/sysmacros.h>
#endif

namespace facebook::eden {

namespace detail {

void logNfsError(
    nfsstat3 error,
    const folly::exception_wrapper& ex,
    ErrorLogger& errorLogger,
    uint64_t inode,
    const AbsolutePath& mountPath) {
  if (error == nfsstat3::NFS3ERR_SERVERFAULT) {
    ex.with_exception([&](const std::exception& e) {
      errorLogger.log(EdenErrorInfo::nfs(e, inode, mountPath.asString()));
    });
  }
}

} // namespace detail

namespace {
static_assert(CheckSize<NfsTraceEvent, 40>());

void incrementNfsGcInvalidationCounter(
    const EdenStatsPtr& stats,
    std::optional<NfsInvalidationSource> source,
    NfsStats::CounterPtr counter) {
  if (source != NfsInvalidationSource::Gc) {
    return;
  }
  stats->increment(counter);
}

/**
 * Whether an AUTH_SYS credential carries `gid` as its primary gid or as one
 * of its auxiliary gids; this is what an nfs:gid-access-policy entry matches.
 */
bool credsHaveGid(const authsys_parms& creds, uint32_t gid) {
  return creds.gid == gid ||
      std::find(creds.gids.begin(), creds.gids.end(), gid) != creds.gids.end();
}

/**
 * Procedures the uid/gid access modes never act on (never counted,
 * never rejected): NULL is the liveness and mount handshake probe, and
 * FSSTAT/FSINFO/PATHCONF are per-mount bookkeeping that NFS clients issue
 * on their own behalf. Rejecting any of these could wedge the mount itself
 * rather than shed the targeted file I/O.
 */
bool isAccessModeExempt(uint32_t proc) {
  switch (static_cast<nfsv3Procs>(proc)) {
    case nfsv3Procs::null:
    case nfsv3Procs::fsstat:
    case nfsv3Procs::fsinfo:
    case nfsv3Procs::pathconf:
      return true;
    default:
      return false;
  }
}

using AccessRateLimiters =
    folly::Synchronized<folly::F14FastMap<uint32_t, NfsAccessRateLimiter>>;

/**
 * Whether one access mode entry rejects a request: Block always does,
 * RateLimit only once `id`'s budget of `count` per `windowSeconds` is
 * exhausted, and Log never does.
 */
bool accessModeRejects(
    NfsAccessMode mode,
    AccessRateLimiters& limiters,
    uint32_t id,
    uint32_t count,
    uint32_t windowSeconds) {
  switch (mode) {
    case NfsAccessMode::Log:
      return false;
    case NfsAccessMode::Block:
      return true;
    case NfsAccessMode::RateLimit: {
      // Shared lock on the common path; the bucket itself is atomic. Only
      // the first request for an id takes the exclusive lock to create it.
      // Bucket references never outlive the lock, so the map is free to
      // rehash.
      {
        auto locked = limiters.rlock();
        if (auto it = locked->find(id); it != locked->end()) {
          return !it->second.allow(count, windowSeconds);
        }
      }
      return !limiters.wlock()->try_emplace(id).first->second.allow(
          count, windowSeconds);
    }
  }
  return false;
}

/**
 * Per-id access stats, exported as nfs.<name>.<id>.sum, .sum.60, etc. The
 * wrapper formats the key only the first time it sees an id and caches the
 * handle per thread.
 */
DEFINE_dynamic_timeseries(nfs_access_uid, "nfs.access.uid.{}", fb303::SUM);
DEFINE_dynamic_timeseries(nfs_policed_uid, "nfs.policed.uid.{}", fb303::SUM);
DEFINE_dynamic_timeseries(nfs_blocked_uid, "nfs.blocked.uid.{}", fb303::SUM);
DEFINE_dynamic_timeseries(nfs_access_gid, "nfs.access.gid.{}", fb303::SUM);
DEFINE_dynamic_timeseries(nfs_policed_gid, "nfs.policed.gid.{}", fb303::SUM);
DEFINE_dynamic_timeseries(nfs_blocked_gid, "nfs.blocked.gid.{}", fb303::SUM);
DEFINE_dynamic_timeseries(
    nfs_access_uid_proc,
    "nfs.access.uid.{}.{}",
    fb303::SUM);
DEFINE_dynamic_timeseries(
    nfs_access_gid_proc,
    "nfs.access.gid.{}.{}",
    fb303::SUM);

class Nfsd3ServerProcessor final : public RpcServerProcessor {
 public:
  explicit Nfsd3ServerProcessor(
      std::unique_ptr<NfsDispatcher> dispatcher,
      const folly::Logger* straceLogger,
      const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
      ErrorLogger& errorLogger,
      AbsolutePath mountPath,
      CaseSensitivity caseSensitive,
      uint32_t readIoSize,
      uint32_t writeIoSize,
      folly::Promise<FsStopDataPtr>& stopPromise,
      ProcessAccessLog& processAccessLog,
      std::atomic<size_t>& traceDetailedArguments,
      std::shared_ptr<TraceBus<NfsTraceEvent>>& traceBus,
      NfsRequestTracker& requestTracker,
      std::chrono::nanoseconds longRunningFSRequestThreshold,
      bool fastPathRPCs,
      std::shared_ptr<ReloadableConfig> config,
      InvalidatingInodes& invalidatingInodes)
      : dispatcher_(std::move(dispatcher)),
        straceLogger_(straceLogger),
        edenFsEventsLogger_(edenFsEventsLogger),
        errorLogger_(errorLogger),
        mountPath_(std::move(mountPath)),
        caseSensitive_(caseSensitive),
        readIoSize_(readIoSize),
        writeIoSize_(writeIoSize),
        stopPromise_{stopPromise},
        processAccessLog_{processAccessLog},
        traceDetailedArguments_(traceDetailedArguments),
        metadataSizeMismatchLogged_(false),
        traceBus_(traceBus),
        requestTracker_(requestTracker),
        longRunningFSRequestThreshold_(longRunningFSRequestThreshold),
        fastPathRPCs_(fastPathRPCs),
        config_{std::move(config)},
        invalidatingInodes_{invalidatingInodes} {}

  Nfsd3ServerProcessor(const Nfsd3ServerProcessor&) = delete;
  Nfsd3ServerProcessor(Nfsd3ServerProcessor&&) = delete;
  Nfsd3ServerProcessor& operator=(const Nfsd3ServerProcessor&) = delete;
  Nfsd3ServerProcessor& operator=(Nfsd3ServerProcessor&&) = delete;

  ImmediateFuture<folly::Unit> dispatchRpc(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      uint32_t xid,
      uint32_t progNumber,
      uint32_t progVersion,
      uint32_t procNumber,
      const std::optional<authsys_parms>& authSysCreds) override;

  bool shouldParseAuthSysCreds() override;

  auth_stat checkAuthentication(
      const call_body& callBody,
      const std::optional<authsys_parms>& authSysCreds) override;

  void onShutdown(RpcStopData stopData) override;
  void clientConnected() override;
  void onExtraConnection() override {
    dispatcher_->getStats()->increment(&NfsStats::nfsRpcExtraConnection);
  }
  void onExtraConnectionRefused() override {
    dispatcher_->getStats()->increment(&NfsStats::nfsRpcExtraConnectionRefused);
  }
  bool shouldFastPathRPCs() const override {
    return fastPathRPCs_;
  }
  bool acceptsMultipleConnections() const override {
    return !config_ ||
        !config_->getEdenConfig()->nfsRefuseExtraClientConnections.getValue();
  }
  bool gcStaleReplyEnabled() const {
    return !config_ || config_->getEdenConfig()->nfsGcStaleReply.getValue();
  }
  bool isUnimplementedProc(uint32_t proc) const override;

  InlineRejectResult tryInlineReject() override;

  void serializeInlineReject(
      uint32_t proc,
      uint32_t xid,
      folly::io::QueueAppender& ser) override;

  void onRequestComplete(const RpcRequestTimeline& timeline) override;

  void setFsChannel(FsChannel* channel) {
    fsChannel_ = channel;
  }

  ImmediateFuture<folly::Unit> null(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> getattr(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> setattr(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> lookup(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> access(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> readlink(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> read(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> write(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> create(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> mkdir(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> symlink(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> mknod(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> remove(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> rmdir(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> rename(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> link(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> readdir(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> readdirplus(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> fsstat(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> fsinfo(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> pathconf(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);
  ImmediateFuture<folly::Unit> commit(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      NfsRequestContext& context);

 private:
  std::unique_ptr<NfsDispatcher> dispatcher_;
  // Logger that is used to observe NFS procedure calls in the EdenFS daemon.
  // All events are published here when we are in stace mode. This is a local
  // logger, the events are not logged anywhere outside of the machine this
  // EdenFS instance runs on.
  const folly::Logger* straceLogger_;
  const std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger_;
  ErrorLogger& errorLogger_;
  AbsolutePath mountPath_;
  CaseSensitivity caseSensitive_;
  uint32_t readIoSize_;
  uint32_t writeIoSize_;
  // This promise is owned by the nfs3d. The nfs3d owns an RPC server that owns
  // this server processor. This promise should only be used during the
  // lifetime of  nfs3d. The way we currently enforce this is by waiting for
  // this promise to be set before destroying of the nfs3d.
  folly::Promise<FsStopDataPtr>& stopPromise_;
  ProcessAccessLog& processAccessLog_;
  std::atomic_int32_t numberOfClients_;
  std::atomic<size_t>& traceDetailedArguments_;
  // TODO(T136422586): Remove once we've identified the cause of mismatched file
  // size metadata.
  std::atomic_bool metadataSizeMismatchLogged_;
  std::shared_ptr<TraceBus<NfsTraceEvent>>& traceBus_;
  NfsRequestTracker& requestTracker_;
  /**
   * The duration that must elapse before we consider a NFS request to be
   * "long running" and therefore log it with EdenFsEventsLogger. This value
   * is configured with EdenConfig::longRunningFSRequestThreshold.
   */
  std::chrono::nanoseconds longRunningFSRequestThreshold_;
  bool fastPathRPCs_;
  std::shared_ptr<ReloadableConfig> config_;
  InvalidatingInodes& invalidatingInodes_;
  // Per-id budgets for NfsAccessMode::RateLimit entries, scoped to this
  // mount's processor. Created on first use and kept for the processor's
  // lifetime: the id set is small and bounded by config.
  AccessRateLimiters uidRateLimiters_;
  AccessRateLimiters gidRateLimiters_;
  std::atomic<size_t> inflightRequests_{0};
  // Used to check rate limiting. Set once in constructor via setFsChannel().
  // Nulled in onShutdown() before Nfsd3 destruction. The backpressure check
  // in dispatchRpc tests for nullptr before dereferencing.
  FsChannel* fsChannel_{nullptr};
};

/**
 * Convert an exception to the appropriate NFS error value. Uses the passed-in
 * EdenStats pointer to log the source of the error.
 */
nfsstat3 exceptionToNfsError(
    const folly::exception_wrapper& ex,
    const EdenStatsPtr& stats) {
  if (auto* err = ex.get_exception<std::system_error>()) {
    if (!isErrnoError(*err)) {
      stats->increment(&NfsStats::nfsErrorServerFaultNoErrno);
      return nfsstat3::NFS3ERR_SERVERFAULT;
    }

    switch (err->code().value()) {
      case EPERM:
        stats->increment(&NfsStats::nfsErrorPerm);
        return nfsstat3::NFS3ERR_PERM;
      case ENOENT:
        stats->increment(&NfsStats::nfsErrorNoEnt);
        return nfsstat3::NFS3ERR_NOENT;
      case EIO:
      case ETXTBSY:
        stats->increment(&NfsStats::nfsErrorIO);
        return nfsstat3::NFS3ERR_IO;
      case ENXIO:
        stats->increment(&NfsStats::nfsErrorNXIO);
        return nfsstat3::NFS3ERR_NXIO;
      case EACCES:
        stats->increment(&NfsStats::nfsErrorAcces);
        return nfsstat3::NFS3ERR_ACCES;
      case EEXIST:
        stats->increment(&NfsStats::nfsErrorExist);
        return nfsstat3::NFS3ERR_EXIST;
      case EXDEV:
        stats->increment(&NfsStats::nfsErrorXDev);
        return nfsstat3::NFS3ERR_XDEV;
      case ENODEV:
        stats->increment(&NfsStats::nfsErrorNoDev);
        return nfsstat3::NFS3ERR_NODEV;
      case ENOTDIR:
        stats->increment(&NfsStats::nfsErrorNotDir);
        return nfsstat3::NFS3ERR_NOTDIR;
      case EISDIR:
        stats->increment(&NfsStats::nfsErrorIsDir);
        return nfsstat3::NFS3ERR_ISDIR;
      case EINVAL:
        stats->increment(&NfsStats::nfsErrorInval);
        return nfsstat3::NFS3ERR_INVAL;
      case EFBIG:
        stats->increment(&NfsStats::nfsErrorFBig);
        return nfsstat3::NFS3ERR_FBIG;
      case EROFS:
        stats->increment(&NfsStats::nfsErrorROFS);
        return nfsstat3::NFS3ERR_ROFS;
      case EMLINK:
        stats->increment(&NfsStats::nfsErrorMLink);
        return nfsstat3::NFS3ERR_MLINK;
      case ENAMETOOLONG:
        stats->increment(&NfsStats::nfsErrorNameTooLong);
        return nfsstat3::NFS3ERR_NAMETOOLONG;
      case ENOTEMPTY:
        stats->increment(&NfsStats::nfsErrorNotEmpty);
        return nfsstat3::NFS3ERR_NOTEMPTY;
// TODO: Are these needed on Windows, probably ESTALE is.
#ifndef _WIN32
      case EDQUOT:
        stats->increment(&NfsStats::nfsErrorDQuot);
        return nfsstat3::NFS3ERR_DQUOT;
      case ESTALE:
        stats->increment(&NfsStats::nfsErrorStale);
        return nfsstat3::NFS3ERR_STALE;
#endif
      case ETIMEDOUT:
        stats->increment(&NfsStats::nfsErrorJukeboxTimedOut);
        return nfsstat3::NFS3ERR_JUKEBOX;
      case EAGAIN:
        stats->increment(&NfsStats::nfsErrorJukeboxAgain);
        return nfsstat3::NFS3ERR_JUKEBOX;
      case ENOMEM:
        stats->increment(&NfsStats::nfsErrorJukeboxNoMem);
        return nfsstat3::NFS3ERR_JUKEBOX;
      case ENOTSUP:
        stats->increment(&NfsStats::nfsErrorNotSupp);
        return nfsstat3::NFS3ERR_NOTSUPP;
      case ENFILE:
        stats->increment(&NfsStats::nfsErrorServerFaultEnfile);
        return nfsstat3::NFS3ERR_SERVERFAULT;
      default:
        stats->increment(&NfsStats::nfsErrorServerFaultDefaultErrno);
        return nfsstat3::NFS3ERR_SERVERFAULT;
    }
  } else if (ex.get_exception<folly::FutureTimeout>()) {
    stats->increment(&NfsStats::nfsErrorJukeboxFollyFut);
    return nfsstat3::NFS3ERR_JUKEBOX;
  } else {
    stats->increment(&NfsStats::nfsErrorServerFaultDefaultEx);
    return nfsstat3::NFS3ERR_SERVERFAULT;
  }
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::null(
    folly::io::Cursor /*deser*/,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  return folly::unit;
}

/**
 * Convert the given string onto a PathComponent.
 *
 * Any raised exception that constructing the PathComponent may raise will be
 * captured by the returned ImmediateFuture.
 */
ImmediateFuture<PathComponent> extractPathComponent(std::string str) {
  return makeImmediateFutureWith([&]() {
    try {
      return PathComponent{str};
    } catch (const PathComponentNotUtf8& ex) {
      throw std::system_error(EINVAL, std::system_category(), ex.what());
    }
  });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::getattr(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());

  auto args = XdrTrait<GETATTR3args>::deserialize(deser);

  auto ino = args.object.ino;
  return dispatcher_->getattr(ino, context.getObjectFetchContext())
      .thenTry([this,
                ser = std::move(ser),
                stats = dispatcher_->getStats().copy(),
                ino](const folly::Try<struct stat>& try_) mutable {
        if (try_.hasException()) {
          auto error = exceptionToNfsError(try_.exception(), stats);
          detail::logNfsError(
              error, try_.exception(), errorLogger_, ino.get(), mountPath_);
          GETATTR3res res{{{error, std::monostate{}}}};
          XdrTrait<GETATTR3res>::serialize(ser, res);
        } else {
          const auto& stat = try_.value();

          GETATTR3res res{
              {{nfsstat3::NFS3_OK, GETATTR3resok{statToFattr3(stat)}}}};
          XdrTrait<GETATTR3res>::serialize(ser, res);
        }

        return folly::unit;
      });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::setattr(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());

  auto args = XdrTrait<SETATTR3args>::deserialize(deser);

  if (args.guard.tag) {
    // TODO(xavierd): we probably need to support this.
    XLOG(WARN, "Guarded setattr aren't supported.");
    SETATTR3res res{{{nfsstat3::NFS3ERR_INVAL, SETATTR3resfail{}}}};
    XdrTrait<SETATTR3res>::serialize(ser, res);
    return folly::unit;
  }

  auto size = args.new_attributes.size.tag
      ? std::optional(std::get<uint64_t>(args.new_attributes.size.v))
      : std::nullopt;
  auto mode = args.new_attributes.mode.tag
      ? std::optional(std::get<uint32_t>(args.new_attributes.mode.v))
      : std::nullopt;
  auto uid = args.new_attributes.uid.tag
      ? std::optional(std::get<uint32_t>(args.new_attributes.uid.v))
      : std::nullopt;
  auto gid = args.new_attributes.gid.tag
      ? std::optional(std::get<uint32_t>(args.new_attributes.gid.v))
      : std::nullopt;

  auto makeTimespec = [this](auto& time) -> std::optional<struct timespec> {
    switch (time.tag) {
      case time_how::SET_TO_CLIENT_TIME:
        return std::optional(nfsTimeToTimespec(std::get<nfstime3>(time.v)));
      case time_how::SET_TO_SERVER_TIME:
        return std::optional(dispatcher_->getClock().getRealtime());
      default:
        return std::nullopt;
    }
  };

  DesiredMetadata desired{
      /*size*/ size,
      /*mode*/ mode,
      /*uid*/ uid,
      /*gid*/ gid,
      /*atime*/ makeTimespec(args.new_attributes.atime),
      /*mtime*/ makeTimespec(args.new_attributes.mtime),
  };

  if (desired.is_nop(/* ignoreAtime= */ true) && desired.atime.has_value()) {
    // EdenFS does not support `atime`, so ignore `atime`-only changes.
    //
    // Ignoring `atime` is not strictly necessary to work around bugs
    // on macOS ARM64 with nop changes since `atime` is empty but let's be safe.
    XLOG(DBG7, "Treating atime-only NFS SETATTR as a no-op");
    desired.atime = std::nullopt;
  }

  return dispatcher_
      ->setattr(args.object.ino, desired, context.getObjectFetchContext())
      .thenTry([this,
                ser = std::move(ser),
                stats = dispatcher_->getStats().copy(),
                ino = args.object.ino](
                   folly::Try<NfsDispatcher::SetattrRes>&& try_) mutable {
        if (try_.hasException()) {
          auto error = exceptionToNfsError(try_.exception(), stats);
          detail::logNfsError(
              error, try_.exception(), errorLogger_, ino.get(), mountPath_);
          SETATTR3res res{{{error, SETATTR3resfail{}}}};
          XdrTrait<SETATTR3res>::serialize(ser, res);
        } else {
          const auto& setattrRes = try_.value();
          if (setattrRes.noop && gcStaleReplyEnabled()) {
            // A no-op SETATTR of a directory whose GC chmod is running is that
            // chmod, see Nfsd3::invalidateWithQueueLimit. The stale reply is
            // what makes the client forget the directory's names, so the
            // children's references are cleared now, before it is sent:
            // anything the client looks up afterwards it references anew. No
            // lock is held here, so forget may take the directory's contents
            // lock. Nothing tells GC's chmod apart from a client's own
            // same-mode chmod of the directory while it runs: such a client
            // takes the stale reply, fails once, and GC's chmod is then
            // answered normally with nothing left to do. With the stale reply
            // disabled the chmod is answered normally and
            // Nfsd3::runInvalidation forgets once it succeeded.
            if (auto forget = invalidatingInodes_.takeForget(ino)) {
              forget();
              stats->increment(&NfsStats::nfsInvalidationGcStaleReply);
              SETATTR3res res{{{nfsstat3::NFS3ERR_STALE, SETATTR3resfail{}}}};
              XdrTrait<SETATTR3res>::serialize(ser, res);
              return folly::unit;
            }
          }

          SETATTR3res res{
              {{nfsstat3::NFS3_OK,
                SETATTR3resok{
                    statToWccData(setattrRes.preStat, setattrRes.postStat)}}}};
          XdrTrait<SETATTR3res>::serialize(ser, res);
        }

        return folly::unit;
      });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::lookup(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<LOOKUP3args>::deserialize(deser);

  // TODO(xavierd): the lifetime of this future is a bit tricky and it needs to
  // be consumed in this function to avoid use-after-free. This future may also
  // need to be executed after the lookup call to conform to fill the "post-op"
  // attributes
  auto dirIno = args.what.dir.ino;
  auto dirAttrFut =
      dispatcher_->getattr(dirIno, context.getObjectFetchContext());

  if (args.what.name.length() > NAME_MAX) {
    // The filename is too long, let's try to get the attributes of the
    // directory and fail.
    return std::move(dirAttrFut)
        .thenTry([ser = std::move(ser)](
                     const folly::Try<struct stat>& try_) mutable {
          if (try_.hasException()) {
            LOOKUP3res res{
                {{nfsstat3::NFS3ERR_NAMETOOLONG,
                  LOOKUP3resfail{post_op_attr{}}}}};
            XdrTrait<LOOKUP3res>::serialize(ser, res);
          } else {
            LOOKUP3res res{
                {{nfsstat3::NFS3ERR_NAMETOOLONG,
                  LOOKUP3resfail{post_op_attr{statToFattr3(try_.value())}}}}};
            XdrTrait<LOOKUP3res>::serialize(ser, res);
          }

          return folly::unit;
        });
  }

  return makeImmediateFutureWith([this, args = std::move(args), &context]() {
           using LookupResult =
               std::optional<std::tuple<InodeNumber, struct stat>>;
           if (args.what.name == ".") {
             return dispatcher_
                 ->getattr(args.what.dir.ino, context.getObjectFetchContext())
                 .thenValue(
                     [ino = args.what.dir.ino](
                         struct stat&& stat) -> LookupResult {
                       return std::make_tuple(ino, std::move(stat));
                     });
           } else if (args.what.name == "..") {
             return dispatcher_
                 ->getParent(args.what.dir.ino, context.getObjectFetchContext())
                 .thenValue([this, &context](InodeNumber ino) {
                   return dispatcher_
                       ->getattr(ino, context.getObjectFetchContext())
                       .thenValue([ino](struct stat&& stat) -> LookupResult {
                         return std::make_tuple(ino, std::move(stat));
                       });
                 });
           } else {
             return extractPathComponent(std::move(args.what.name))
                 .thenValue([this, ino = args.what.dir.ino, &context](
                                PathComponent&& name) {
                   return dispatcher_->lookup(
                       ino, std::move(name), context.getObjectFetchContext());
                 });
           }
         })
      .thenTry(
          [this,
           ser = std::move(ser),
           dirAttrFut = std::move(dirAttrFut),
           stats = dispatcher_->getStats().copy(),
           ino = dirIno,
           &context](
              folly::Try<std::optional<std::tuple<InodeNumber, struct stat>>>&&
                  lookupTry) mutable {
            return std::move(dirAttrFut)
                .thenTry([this,
                          ser = std::move(ser),
                          lookupTry = std::move(lookupTry),
                          stats = std::move(stats),
                          ino,
                          &context](
                             const folly::Try<struct stat>& dirStat) mutable {
                  if (lookupTry.hasException()) {
                    auto error =
                        exceptionToNfsError(lookupTry.exception(), stats);
                    // Whatever the error, no entry was handed out.
                    context.markNegativeLookup();
                    detail::logNfsError(
                        error,
                        lookupTry.exception(),
                        errorLogger_,
                        ino.get(),
                        mountPath_);
                    LOOKUP3res res{
                        {{error, LOOKUP3resfail{statToPostOpAttr(dirStat)}}}};
                    XdrTrait<LOOKUP3res>::serialize(ser, res);
                  } else if (!lookupTry->has_value()) {
                    // No such entry: the usual answer to the lookup a client
                    // sends before create and mkdir, so it arrives as a value
                    // rather than an exception. It is counted and recorded like
                    // any other NOENT reply.
                    stats->increment(&NfsStats::nfsErrorNoEnt);
                    context.markNegativeLookup();
                    LOOKUP3res res{
                        {{nfsstat3::NFS3ERR_NOENT,
                          LOOKUP3resfail{statToPostOpAttr(dirStat)}}}};
                    XdrTrait<LOOKUP3res>::serialize(ser, res);
                  } else {
                    const auto& [lookupIno, stat] = **lookupTry;
                    LOOKUP3res res{
                        {{nfsstat3::NFS3_OK,
                          LOOKUP3resok{
                              /*object*/ nfs_fh3{lookupIno},
                              /*obj_attributes*/
                              post_op_attr{statToFattr3(stat)},
                              /*dir_attributes*/
                              statToPostOpAttr(dirStat),
                          }}}};
                    XdrTrait<LOOKUP3res>::serialize(ser, res);
                  }
                  return folly::unit;
                });
          });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::access(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());

  auto args = XdrTrait<ACCESS3args>::deserialize(deser);

  return dispatcher_->getattr(args.object.ino, context.getObjectFetchContext())
      .thenTry([this,
                ser = std::move(ser),
                desiredAccess = args.access,
                stats = dispatcher_->getStats().copy(),
                ino = args.object.ino](folly::Try<struct stat>&& try_) mutable {
        if (try_.hasException()) {
          auto error = exceptionToNfsError(try_.exception(), stats);
          detail::logNfsError(
              error, try_.exception(), errorLogger_, ino.get(), mountPath_);
          ACCESS3res res{{{error, ACCESS3resfail{post_op_attr{}}}}};
          XdrTrait<ACCESS3res>::serialize(ser, res);
        } else {
          const auto& stat = try_.value();

          ACCESS3res res{
              {{nfsstat3::NFS3_OK,
                ACCESS3resok{
                    post_op_attr{statToFattr3(stat)},
                    /*access*/ getEffectiveAccessRights(stat, desiredAccess),
                }}}};
          XdrTrait<ACCESS3res>::serialize(ser, res);
        }

        return folly::unit;
      });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::readlink(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<READLINK3args>::deserialize(deser);

  auto getattr =
      dispatcher_->getattr(args.symlink.ino, context.getObjectFetchContext());
  return dispatcher_
      ->readlink(args.symlink.ino, context.getObjectFetchContext())
      .thenTry([this,
                ser = std::move(ser),
                getattr = std::move(getattr),
                stats = dispatcher_->getStats().copy(),
                ino = args.symlink.ino](
                   folly::Try<std::string> tryReadlink) mutable {
        return std::move(getattr).thenTry(
            [this,
             ser = std::move(ser),
             tryReadlink = std::move(tryReadlink),
             stats = std::move(stats),
             ino](const folly::Try<struct stat>& tryAttr) mutable {
              if (tryReadlink.hasException()) {
                auto error =
                    exceptionToNfsError(tryReadlink.exception(), stats);
                detail::logNfsError(
                    error,
                    tryReadlink.exception(),
                    errorLogger_,
                    ino.get(),
                    mountPath_);
                READLINK3res res{
                    {{error, READLINK3resfail{statToPostOpAttr(tryAttr)}}}};
                XdrTrait<READLINK3res>::serialize(ser, res);
              } else {
                auto&& link = std::move(tryReadlink).value();

                READLINK3res res{
                    {{nfsstat3::NFS3_OK,
                      READLINK3resok{
                          /*symlink_attributes*/ statToPostOpAttr(tryAttr),
                          /*data*/ std::move(link),
                      }}}};
                XdrTrait<READLINK3res>::serialize(ser, res);
              }

              return folly::unit;
            });
      });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::read(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<READ3args>::deserialize(deser);

  return dispatcher_
      ->read(
          args.file.ino,
          args.count,
          args.offset,
          context.getObjectFetchContext())
      .thenTry([this,
                ser = std::move(ser),
                ino = args.file.ino,
                &context,
                stats = dispatcher_->getStats().copy()](
                   folly::Try<NfsDispatcher::ReadRes> tryRead) mutable {
        return dispatcher_->getattr(ino, context.getObjectFetchContext())
            .thenTry([this,
                      ser = std::move(ser),
                      tryRead = std::move(tryRead),
                      ino,
                      stats = std::move(stats)](
                         const folly::Try<struct stat>& tryStat) mutable {
              if (tryRead.hasException()) {
                auto error = exceptionToNfsError(tryRead.exception(), stats);
                detail::logNfsError(
                    error,
                    tryRead.exception(),
                    errorLogger_,
                    ino.get(),
                    mountPath_);
                READ3res res{
                    {{error, READ3resfail{statToPostOpAttr(tryStat)}}}};
                XdrTrait<READ3res>::serialize(ser, res);
              } else {
                auto& read = tryRead.value();
                auto length = read.data->computeChainDataLength();

                if (UNLIKELY(
                        tryStat.hasValue() &&
                        length > folly::to_unsigned(tryStat.value().st_size) &&
                        !this->metadataSizeMismatchLogged_.exchange(true))) {
                  XLOG(
                      ERR,
                      fmt::format(
                          "Mismatch in blob size and cached size for inode {} ! "
                          "content chunk size {} greater than file size {}.",
                          ino,
                          length,
                          tryStat.value().st_size));

                  this->edenFsEventsLogger_->logEvent(
                      MetadataSizeMismatch{"NFS", "read"});
                }

                if (UNLIKELY(tryStat.hasException())) {
                  XLOG(
                      WARN,
                      fmt::format(
                          "getattr error during NFSv3 read: {}",
                          exceptionStr(tryStat.exception())));
                }

                // Make sure that we haven't read more than what we can encode.
                XDCHECK_LE(
                    length, size_t{std::numeric_limits<uint32_t>::max()});

                READ3res res{
                    {{nfsstat3::NFS3_OK,
                      READ3resok{
                          /*file_attributes*/ statToPostOpAttr(tryStat),
                          /*count*/ folly::to_narrow(length),
                          /*eof*/ read.isEof,
                          /*data*/ std::move(read.data),
                      }}}};
                XdrTrait<READ3res>::serialize(ser, res);
              }
              return folly::unit;
            });
      });
}

/**
 * Generate a unique per-EdenFS instance write cookie.
 *
 * TODO(xavierd): Note that for now this will always be 0 as this is to handle
 * the case where the server restart while the client isn't aware.
 */
writeverf3 makeWriteVerf() {
  return 0;
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::write(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<WRITE3args>::deserialize(deser);

  // I have no idea why NFS sent us data that we shouldn't write to the file,
  // but here it is, let's only take up to count bytes from the data.
  auto queue = folly::IOBufQueue();
  queue.append(std::move(args.data));
  auto data = queue.split(args.count);

  return dispatcher_
      ->write(
          args.file.ino,
          std::move(data),
          args.offset,
          context.getObjectFetchContext())
      .thenTry([this,
                ser = std::move(ser),
                stats = dispatcher_->getStats().copy(),
                ino = args.file.ino](
                   folly::Try<NfsDispatcher::WriteRes> writeTry) mutable {
        if (writeTry.hasException()) {
          auto error = exceptionToNfsError(writeTry.exception(), stats);
          detail::logNfsError(
              error, writeTry.exception(), errorLogger_, ino.get(), mountPath_);
          WRITE3res res{{{error, WRITE3resfail{}}}};
          XdrTrait<WRITE3res>::serialize(ser, res);
        } else {
          const auto& writeRes = writeTry.value();

          // NFS is limited to writing a maximum of 4GB (2^32) of data
          // per write call, so despite write returning a size_t, it
          // should always fit in a uint32_t.
          XDCHECK_LE(
              writeRes.written, size_t{std::numeric_limits<uint32_t>::max()});

          WRITE3res res{
              {{nfsstat3::NFS3_OK,
                WRITE3resok{
                    /*file_wcc*/ statToWccData(
                        writeRes.preStat, writeRes.postStat),
                    /*count*/ folly::to_narrow(writeRes.written),
                    // TODO(xavierd): the following is a total lie and we
                    // should call inode->fdatasync() in the case where
                    // args.stable is anything other than
                    // stable_how::UNSTABLE. For testing purpose, this is
                    // OK.
                    /*committed*/ stable_how::FILE_SYNC,
                    /*verf*/ makeWriteVerf(),
                }}}};
          XdrTrait<WRITE3res>::serialize(ser, res);
        }

        return folly::unit;
      });
}

/**
 * Test if the exception was raised due to a EEXIST condition.
 */
bool isEexist(const folly::exception_wrapper& ex) {
  if (auto* err = ex.get_exception<std::system_error>()) {
    return isErrnoError(*err) && err->code().value() == EEXIST;
  }
  return false;
}

/**
 * Convert a set_mode3 into a usable mode_t. When unset, the returned mode
 * will be writable by the owner, readable by the group and other. This is
 * consistent with creating a file with a default umask of 022.
 */
mode_t setMode3ToMode(const set_mode3& mode) {
  if (mode.tag) {
    return std::get<uint32_t>(mode.v);
  } else {
    return 0644;
  }
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::create(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<CREATE3args>::deserialize(deser);

  if (args.how.tag == createmode3::EXCLUSIVE) {
    // Exclusive file creation is complicated, for now let's not support it.
    CREATE3res res{{{nfsstat3::NFS3ERR_NOTSUPP, CREATE3resfail{wcc_data{}}}}};
    XdrTrait<CREATE3res>::serialize(ser, res);
    return folly::unit;
  }

  auto& attr = std::get<sattr3>(args.how.v);
  auto mode = S_IFREG | setMode3ToMode(attr.mode);

  return extractPathComponent(std::move(args.where.name))
      .thenValue([this,
                  ino = args.where.dir.ino,
                  mode,
                  how = args.how,
                  &context](PathComponent&& name) {
        return dispatcher_->create(
            ino, std::move(name), mode, how, context.getObjectFetchContext());
      })
      .thenTry([this,
                ser = std::move(ser),
                createmode = args.how.tag,
                stats = dispatcher_->getStats().copy(),
                ino = args.where.dir.ino](
                   folly::Try<NfsDispatcher::CreateRes> try_) mutable {
        if (try_.hasException()) {
          if (createmode == createmode3::UNCHECKED &&
              isEexist(try_.exception())) {
            XLOG(WARN, "Unchecked file creation returned EEXIST");
            // A file already exist at that location, since this is an
            // UNCHECKED creation, just pretend the file was created just fine.
            // Since no fields are populated, this forces the client to issue a
            // LOOKUP RPC to gather the InodeNumber and attributes for this
            // file. This is probably fine as creating a file that already
            // exists should be a rare event.
            // TODO(xavierd): We should change the file attributes based on
            // the requested args.how.obj_attributes.
            CREATE3res res{
                {{nfsstat3::NFS3_OK,
                  CREATE3resok{/*obj*/ post_op_fh3{},
                               /*obj_attributes*/ post_op_attr{},
                               wcc_data{
                                   /*before*/ pre_op_attr{},
                                   /*after*/ post_op_attr{},
                               }}}}};
            XdrTrait<CREATE3res>::serialize(ser, res);
          } else {
            auto error = exceptionToNfsError(try_.exception(), stats);
            detail::logNfsError(
                error, try_.exception(), errorLogger_, ino.get(), mountPath_);
            CREATE3res res{{{error, CREATE3resfail{}}}};
            XdrTrait<CREATE3res>::serialize(ser, res);
          }
        } else {
          const auto& createRes = try_.value();

          CREATE3res res{
              {{nfsstat3::NFS3_OK,
                CREATE3resok{
                    /*obj*/ post_op_fh3{nfs_fh3{createRes.ino}},
                    /*obj_attributes*/
                    post_op_attr{statToFattr3(createRes.stat)},
                    /*dir_wcc*/
                    statToWccData(createRes.preDirStat, createRes.postDirStat),
                }}}};
          XdrTrait<CREATE3res>::serialize(ser, res);
        }
        return folly::unit;
      });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::mkdir(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<MKDIR3args>::deserialize(deser);

  // Don't allow creating this directory and its parent.
  if (args.where.name == "." || args.where.name == "..") {
    MKDIR3res res{{{nfsstat3::NFS3ERR_EXIST, MKDIR3resfail{}}}};
    XdrTrait<MKDIR3res>::serialize(ser, res);
    return folly::unit;
  }

  // If the mode isn't set, make it writable by the owner, readable by the
  // group and traversable by other.
  auto mode = args.attributes.mode.tag
      ? std::get<uint32_t>(args.attributes.mode.v)
      : (S_IFDIR | 0751);

  // TODO(xavierd): For now, all the other args.attributes are ignored, is it
  // OK?

  return extractPathComponent(std::move(args.where.name))
      .thenValue([this, ino = args.where.dir.ino, mode, &context](
                     PathComponent&& name) {
        return dispatcher_->mkdir(
            ino, std::move(name), mode, context.getObjectFetchContext());
      })
      .thenTry([this,
                ser = std::move(ser),
                stats = dispatcher_->getStats().copy(),
                ino = args.where.dir.ino](
                   folly::Try<NfsDispatcher::MkdirRes> try_) mutable {
        if (try_.hasException()) {
          auto error = exceptionToNfsError(try_.exception(), stats);
          detail::logNfsError(
              error, try_.exception(), errorLogger_, ino.get(), mountPath_);
          MKDIR3res res{{{error, MKDIR3resfail{}}}};
          XdrTrait<MKDIR3res>::serialize(ser, res);
        } else {
          const auto& mkdirRes = try_.value();

          MKDIR3res res{
              {{nfsstat3::NFS3_OK,
                MKDIR3resok{
                    /*obj*/ post_op_fh3{nfs_fh3{mkdirRes.ino}},
                    /*obj_attributes*/
                    post_op_attr{statToFattr3(mkdirRes.stat)},
                    /*dir_wcc*/
                    statToWccData(mkdirRes.preDirStat, mkdirRes.postDirStat),
                }}}};
          XdrTrait<MKDIR3res>::serialize(ser, res);
        }
        return folly::unit;
      });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::symlink(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<SYMLINK3args>::deserialize(deser);

  // Don't allow creating a symlink named . or ..
  if (args.where.name == "." || args.where.name == "..") {
    SYMLINK3res res{{{nfsstat3::NFS3ERR_INVAL, SYMLINK3resfail{}}}};
    XdrTrait<SYMLINK3res>::serialize(ser, res);
    return folly::unit;
  }

  // TODO(xavierd): set the attributes of the symlink with symlink_attr

  return extractPathComponent(std::move(args.where.name))
      .thenValue([this,
                  ino = args.where.dir.ino,
                  symlink_data = std::move(args.symlink.symlink_data),
                  &context](PathComponent&& name) mutable {
        return dispatcher_->symlink(
            ino,
            std::move(name),
            std::move(symlink_data),
            context.getObjectFetchContext());
      })
      .thenTry([this,
                ser = std::move(ser),
                stats = dispatcher_->getStats().copy(),
                ino = args.where.dir.ino](
                   folly::Try<NfsDispatcher::SymlinkRes> try_) mutable {
        if (try_.hasException()) {
          auto error = exceptionToNfsError(try_.exception(), stats);
          detail::logNfsError(
              error, try_.exception(), errorLogger_, ino.get(), mountPath_);
          SYMLINK3res res{{{error, SYMLINK3resfail{}}}};
          XdrTrait<SYMLINK3res>::serialize(ser, res);
        } else {
          const auto& symlinkRes = try_.value();

          SYMLINK3res res{
              {{nfsstat3::NFS3_OK,
                SYMLINK3resok{
                    /*obj*/ post_op_fh3{nfs_fh3{symlinkRes.ino}},
                    /*obj_attributes*/
                    post_op_attr{statToFattr3(symlinkRes.stat)},
                    /*dir_wcc*/
                    statToWccData(
                        symlinkRes.preDirStat, symlinkRes.postDirStat),
                }}}};
          XdrTrait<SYMLINK3res>::serialize(ser, res);
        }
        return folly::unit;
      });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::mknod(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<MKNOD3args>::deserialize(deser);

  switch (args.what.tag) {
    case ftype3::NF3REG:
    case ftype3::NF3DIR:
    case ftype3::NF3LNK: {
      MKNOD3res res{{{nfsstat3::NFS3ERR_BADTYPE, MKNOD3resfail{}}}};
      XdrTrait<MKNOD3res>::serialize(ser, res);
      return folly::unit;
    }
    default:
      break;
  }

  // Don't allow creating a node name . or ..
  if (args.where.name == "." || args.where.name == "..") {
    MKNOD3res res{{{nfsstat3::NFS3ERR_INVAL, MKNOD3resfail{}}}};
    XdrTrait<MKNOD3res>::serialize(ser, res);
    return folly::unit;
  }

  mode_t mode = ftype3ToMode(args.what.tag);
  dev_t rdev;
  if (auto devicedata = std::get_if<devicedata3>(&args.what.v)) {
    mode |= setMode3ToMode(devicedata->dev_attributes.mode);
#ifndef _WIN32
    rdev = makedev(devicedata->spec.specdata1, devicedata->spec.specdata2);
#else
    // TODO: fully implement mknod
    rdev = 0;
#endif
  } else if (auto sattr = std::get_if<sattr3>(&args.what.v)) {
    mode |= setMode3ToMode(sattr->mode);
    rdev = 0;
  } else {
    // This can only happen if the deserialization code is wrong, but let's be
    // safe.
    MKNOD3res res{{{nfsstat3::NFS3ERR_SERVERFAULT, MKNOD3resfail{}}}};
    XdrTrait<MKNOD3res>::serialize(ser, res);
    return folly::unit;
  }

  // TODO(xavierd): we should probably respect the rest of the sattr3
  // attributes.

  return extractPathComponent(std::move(args.where.name))
      .thenValue([this, ino = args.where.dir.ino, mode, rdev, &context](
                     PathComponent&& name) {
        return dispatcher_->mknod(
            ino, std::move(name), mode, rdev, context.getObjectFetchContext());
      })
      .thenTry([this,
                ser = std::move(ser),
                stats = dispatcher_->getStats().copy(),
                ino = args.where.dir.ino](
                   folly::Try<NfsDispatcher::MknodRes> try_) mutable {
        if (try_.hasException()) {
          auto error = exceptionToNfsError(try_.exception(), stats);
          detail::logNfsError(
              error, try_.exception(), errorLogger_, ino.get(), mountPath_);
          MKNOD3res res{{{error, MKNOD3resfail{}}}};
          XdrTrait<MKNOD3res>::serialize(ser, res);
        } else {
          const auto& mknodRes = try_.value();

          MKNOD3res res{
              {{nfsstat3::NFS3_OK,
                MKNOD3resok{
                    /*obj*/ post_op_fh3{nfs_fh3{mknodRes.ino}},
                    /*obj_attributes*/
                    post_op_attr{statToFattr3(mknodRes.stat)},
                    /*dir_wcc*/
                    statToWccData(mknodRes.preDirStat, mknodRes.postDirStat),
                }}}};
          XdrTrait<MKNOD3res>::serialize(ser, res);
        }
        return folly::unit;
      });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::remove(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<REMOVE3args>::deserialize(deser);

  // Don't allow removing the special directories.
  if (args.object.name == "." || args.object.name == "..") {
    REMOVE3res res{{{nfsstat3::NFS3ERR_ACCES, REMOVE3resfail{}}}};
    XdrTrait<REMOVE3res>::serialize(ser, res);
    return folly::unit;
  }

  // TODO(xavierd): What if args.object.name is a directory? This will fail
  // with NFS3ERR_ISDIR, but the spec is vague regarding what needs to happen
  // here, "REMOVE can be used to remove directories, subject to restrictions
  // imposed by either the client or server interfaces"

  return extractPathComponent(std::move(args.object.name))
      .thenValue(
          [this, ino = args.object.dir.ino, &context](PathComponent&& name) {
            return dispatcher_->unlink(
                ino, std::move(name), context.getObjectFetchContext());
          })
      .thenTry([this,
                ser = std::move(ser),
                stats = dispatcher_->getStats().copy(),
                ino = args.object.dir.ino](
                   folly::Try<NfsDispatcher::UnlinkRes> try_) mutable {
        if (try_.hasException()) {
          auto error = exceptionToNfsError(try_.exception(), stats);
          detail::logNfsError(
              error, try_.exception(), errorLogger_, ino.get(), mountPath_);
          REMOVE3res res{{{error, REMOVE3resfail{}}}};
          XdrTrait<REMOVE3res>::serialize(ser, res);
        } else {
          const auto& unlinkRes = try_.value();

          REMOVE3res res{
              {{nfsstat3::NFS3_OK,
                REMOVE3resok{/*dir_wcc*/ statToWccData(
                    unlinkRes.preDirStat, unlinkRes.postDirStat)}}}};
          XdrTrait<REMOVE3res>::serialize(ser, res);
        }
        return folly::unit;
      });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::rmdir(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<RMDIR3args>::deserialize(deser);

  // Don't allow removing the special directories.
  if (args.object.name == "." || args.object.name == "..") {
    // The NFS spec specifies 2 different error status for "." and ".."
    auto status = args.object.name == "." ? nfsstat3::NFS3ERR_INVAL
                                          : nfsstat3::NFS3ERR_EXIST;
    RMDIR3res res{{{status, RMDIR3resfail{}}}};
    XdrTrait<RMDIR3res>::serialize(ser, res);
    return folly::unit;
  }

  return extractPathComponent(std::move(args.object.name))
      .thenValue(
          [this, ino = args.object.dir.ino, &context](PathComponent&& name) {
            return dispatcher_->rmdir(
                ino, std::move(name), context.getObjectFetchContext());
          })
      .thenTry([this,
                ser = std::move(ser),
                stats = dispatcher_->getStats().copy(),
                ino = args.object.dir.ino](
                   folly::Try<NfsDispatcher::RmdirRes> try_) mutable {
        if (try_.hasException()) {
          auto error = exceptionToNfsError(try_.exception(), stats);
          detail::logNfsError(
              error, try_.exception(), errorLogger_, ino.get(), mountPath_);
          RMDIR3res res{{{error, RMDIR3resfail{}}}};
          XdrTrait<RMDIR3res>::serialize(ser, res);
        } else {
          const auto& rmdirRes = try_.value();

          RMDIR3res res{
              {{nfsstat3::NFS3_OK,
                RMDIR3resok{/*dir_wcc*/ statToWccData(
                    rmdirRes.preDirStat, rmdirRes.postDirStat)}}}};
          XdrTrait<RMDIR3res>::serialize(ser, res);
        }
        return folly::unit;
      });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::rename(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<RENAME3args>::deserialize(deser);

  if (args.from.name == "." || args.from.name == ".." || args.to.name == "." ||
      args.to.name == "..") {
    RENAME3res res{{{nfsstat3::NFS3ERR_INVAL, RENAME3resfail{}}}};
    XdrTrait<RENAME3res>::serialize(ser, res);
    return folly::unit;
  }

  // Do nothing if the source and destination are the exact same file.
  if (args.from == args.to) {
    RENAME3res res{{{nfsstat3::NFS3_OK, RENAME3resok{}}}};
    XdrTrait<RENAME3res>::serialize(ser, res);
    return folly::unit;
  }

  return extractPathComponent(std::move(args.from.name))
      .thenValue([toName = args.to.name](PathComponent&& fromName) mutable {
        return extractPathComponent(std::move(toName))
            .thenValue(
                [fromName = std::move(fromName)](PathComponent&& toName) mutable
                    -> std::tuple<PathComponent, PathComponent> {
                  return {std::move(fromName), std::move(toName)};
                });
      })
      .thenValue([this,
                  fromIno = args.from.dir.ino,
                  toIno = args.to.dir.ino,
                  &context](std::tuple<PathComponent, PathComponent>&& paths) {
        auto [fromName, toName] = std::move(paths);
        return dispatcher_->rename(
            fromIno,
            std::move(fromName),
            toIno,
            std::move(toName),
            context.getObjectFetchContext());
      })
      .thenTry([this,
                ser = std::move(ser),
                stats = dispatcher_->getStats().copy(),
                ino = args.from.dir.ino](
                   folly::Try<NfsDispatcher::RenameRes> try_) mutable {
        if (try_.hasException()) {
          auto error = exceptionToNfsError(try_.exception(), stats);
          detail::logNfsError(
              error, try_.exception(), errorLogger_, ino.get(), mountPath_);
          RENAME3res res{{{error, RENAME3resfail{}}}};
          XdrTrait<RENAME3res>::serialize(ser, res);
        } else {
          const auto& renameRes = try_.value();

          RENAME3res res{
              {{nfsstat3::NFS3_OK,
                RENAME3resok{
                    /*fromdir_wcc*/ statToWccData(
                        renameRes.fromPreDirStat, renameRes.fromPostDirStat),
                    /*todir_wcc*/
                    statToWccData(
                        renameRes.toPreDirStat, renameRes.toPostDirStat),
                }}}};
          XdrTrait<RENAME3res>::serialize(ser, res);
        }

        return folly::unit;
      });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::link(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<LINK3args>::deserialize(deser);

  // EdenFS doesn't support hardlinks, let's just collect the attributes for
  // the file and fail.
  return dispatcher_->getattr(args.file.ino, context.getObjectFetchContext())
      .thenTry(
          [ser = std::move(ser)](const folly::Try<struct stat>& try_) mutable {
            LINK3res res{
                {{nfsstat3::NFS3ERR_NOTSUPP,
                  LINK3resfail{statToPostOpAttr(try_), wcc_data{}}}}};
            XdrTrait<LINK3res>::serialize(ser, res);
            return folly::unit;
          });
}

/**
 * Verify that the passed in cookie verifier is valid.
 *
 * The verifier allows the server to know whether the directory was modified
 * across readdir calls, and to restart if this is the case.
 *
 * TODO(xavierd): For now, this only checks that the verifier is 0, in the
 * future, we may want to compare it against a global counter that is
 * incremented for each update operations. The assumption being that: "The
 * client should be careful to avoid holding directory entry cookies across
 * operations that modify the directory contents, such as REMOVE and CREATE.",
 * thus we only need to protect against concurrent update and readdir
 * operations since there is only one client per mount.
 */
bool isReaddirCookieverfValid(uint64_t verf) {
  return verf == 0;
}

/**
 * Return the current global cookie.
 *
 * See the documentation above for the meaning of the cookie verifier.
 */
uint64_t getReaddirCookieverf() {
  return 0;
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::readdir(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<READDIR3args>::deserialize(deser);

  if (!isReaddirCookieverfValid(args.cookieverf)) {
    READDIR3res res{{{nfsstat3::NFS3ERR_BAD_COOKIE, READDIR3resfail{}}}};
    XdrTrait<READDIR3res>::serialize(ser, res);
    return folly::unit;
  }

  return dispatcher_
      ->readdir(
          args.dir.ino,
          args.cookie,
          args.count,
          context.getObjectFetchContext())
      .thenTry([this, ino = args.dir.ino, ser = std::move(ser), &context](
                   folly::Try<NfsDispatcher::ReaddirRes> try_) mutable {
        return dispatcher_->getattr(ino, context.getObjectFetchContext())
            .thenTry([this,
                      ser = std::move(ser),
                      try_ = std::move(try_),
                      stats = dispatcher_->getStats().copy(),
                      ino](const folly::Try<struct stat>& tryStat) mutable {
              if (try_.hasException()) {
                auto error = exceptionToNfsError(try_.exception(), stats);
                detail::logNfsError(
                    error,
                    try_.exception(),
                    errorLogger_,
                    ino.get(),
                    mountPath_);
                READDIR3res res{
                    {{error, READDIR3resfail{statToPostOpAttr(tryStat)}}}};
                XdrTrait<READDIR3res>::serialize(ser, res);
              } else {
                auto& readdirRes = try_.value();

                READDIR3res res{
                    {{nfsstat3::NFS3_OK,
                      READDIR3resok{
                          /*dir_attributes*/ statToPostOpAttr(tryStat),
                          /*cookieverf*/ getReaddirCookieverf(),
                          /*reply*/
                          dirlist3{
                              /*entries*/ readdirRes.entries
                                  .extractList<entry3>(),
                              /*eof*/ readdirRes.isEof,
                          }}}}};
                XdrTrait<READDIR3res>::serialize(ser, res);
              }
              return folly::unit;
            });
      });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::readdirplus(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<READDIRPLUS3args>::deserialize(deser);

  if (!isReaddirCookieverfValid(args.cookieverf)) {
    READDIRPLUS3res res{
        {{nfsstat3::NFS3ERR_BAD_COOKIE, READDIRPLUS3resfail{}}}};
    XdrTrait<READDIRPLUS3res>::serialize(ser, res);
    return folly::unit;
  }

  // TODO(T107744453): Should probably account for args.maxcount somewhere
  return dispatcher_
      ->readdirplus(
          args.dir.ino,
          args.cookie,
          args.dircount,
          context.getObjectFetchContext())
      .thenTry([this, ino = args.dir.ino, ser = std::move(ser), &context](
                   folly::Try<NfsDispatcher::ReaddirRes> try_) mutable {
        return dispatcher_->getattr(ino, context.getObjectFetchContext())
            .thenTry([this,
                      ser = std::move(ser),
                      try_ = std::move(try_),
                      stats = dispatcher_->getStats().copy(),
                      ino](const folly::Try<struct stat>& tryStat) mutable {
              if (try_.hasException()) {
                auto error = exceptionToNfsError(try_.exception(), stats);
                detail::logNfsError(
                    error,
                    try_.exception(),
                    errorLogger_,
                    ino.get(),
                    mountPath_);
                READDIRPLUS3res res{
                    {{error, READDIRPLUS3resfail{statToPostOpAttr(tryStat)}}}};
                XdrTrait<READDIRPLUS3res>::serialize(ser, res);
              } else {
                auto& readdirRes = try_.value();
                /* TODO @cuev: This is prob where we'd use args.maxcount:
                 *
                 * From rfc 1813 section 3.3.17:
                 *
                 * maxcount
                 *    The maximum size of the READDIRPLUS3resok structure, in
                 *    bytes. The size must include all XDR overhead. The server
                 *    is free to return fewer than maxcount bytes of data.
                 */
                READDIRPLUS3res res{
                    {{nfsstat3::NFS3_OK,
                      READDIRPLUS3resok{
                          /*dir_attributes*/ statToPostOpAttr(tryStat),
                          /*cookieverf*/ getReaddirCookieverf(),
                          /*reply*/
                          dirlistplus3{
                              /*entries*/ readdirRes.entries
                                  .extractList<entryplus3>(),
                              /*eof*/ readdirRes.isEof,
                          }}}}};
                XdrTrait<READDIRPLUS3res>::serialize(ser, res);
              }
              return folly::unit;
            });
      });
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::fsstat(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<FSSTAT3args>::deserialize(deser);

#ifndef _WIN32
  return dispatcher_->statfs(args.fsroot.ino, context.getObjectFetchContext())
      .thenTry([this, ser = std::move(ser), ino = args.fsroot.ino, &context](
                   folly::Try<struct statfs> statFsTry) mutable {
        return dispatcher_->getattr(ino, context.getObjectFetchContext())
            .thenTry([this,
                      ser = std::move(ser),
                      statFsTry = std::move(statFsTry),
                      stats = dispatcher_->getStats().copy(),
                      ino](const folly::Try<struct stat>& statTry) mutable {
              if (statFsTry.hasException()) {
                auto error = exceptionToNfsError(statFsTry.exception(), stats);
                detail::logNfsError(
                    error,
                    statFsTry.exception(),
                    errorLogger_,
                    ino.get(),
                    mountPath_);
                FSSTAT3res res{
                    {{error, FSSTAT3resfail{statToPostOpAttr(statTry)}}}};
                XdrTrait<FSSTAT3res>::serialize(ser, res);
              } else {
                auto& statfs = statFsTry.value();

                FSSTAT3res res{
                    {{nfsstat3::NFS3_OK,
                      FSSTAT3resok{
                          /*obj_attributes*/ statToPostOpAttr(statTry),
                          /*tbytes*/ statfs.f_blocks * statfs.f_bsize,
                          /*fbytes*/ statfs.f_blocks * statfs.f_bsize,
                          /*abytes*/ statfs.f_bavail * statfs.f_bsize,
                          /*tfiles*/ statfs.f_files,
                          /*ffiles*/ statfs.f_ffree,
                          /*afiles*/ statfs.f_ffree,
                          /*invarsec*/ 0,
                      }}}};
                XdrTrait<FSSTAT3res>::serialize(ser, res);
              }

              return folly::unit;
            });
      });
#else
  // TODO: implement on windows
  return dispatcher_->getattr(args.fsroot.ino, context.getObjectFetchContext())
      .thenTry([ser = std::move(ser)](
                   const folly::Try<struct stat>& statTry) mutable {
        FSSTAT3res res{
            {{nfsstat3::NFS3ERR_NOTSUPP,
              FSSTAT3resfail{statToPostOpAttr(statTry)}}}};
        XdrTrait<FSSTAT3res>::serialize(ser, res);
      });
#endif
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::fsinfo(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<FSINFO3args>::deserialize(deser);
  (void)args;

  FSINFO3res res{
      {{nfsstat3::NFS3_OK,
        FSINFO3resok{
            // TODO(xavierd): fill the post_op_attr.
            post_op_attr{},
            /*rtmax=*/readIoSize_,
            /*rtpref=*/readIoSize_,
            /*rtmult=*/1,
            /*wtmax=*/writeIoSize_,
            /*wtpref=*/writeIoSize_,
            /*wtmult=*/1,
            // READDIR replies are reads, so they share the read size. The
            // client's dsize is configured separately via `nfs:dir-read-size`.
            /*dtpref=*/readIoSize_,
            /*maxfilesize=*/std::numeric_limits<uint64_t>::max(),
            // TODO: support nanosecond granularity on Windows
            /*time_delta*/ folly::kIsWindows ? nfstime3{1, 0} : nfstime3{0, 1},
            /*properties*/ FSF3_SYMLINK | FSF3_HOMOGENEOUS | FSF3_CANSETTIME,
        }}}};

  XdrTrait<FSINFO3res>::serialize(ser, res);

  return folly::unit;
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::pathconf(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::SUCCESS, context.getXid());
  auto args = XdrTrait<PATHCONF3args>::deserialize(deser);
  (void)args;

  PATHCONF3res res{
      {{nfsstat3::NFS3_OK,
        PATHCONF3resok{
            // TODO(xavierd): fill up the post_op_attr
            post_op_attr{},
            /*linkmax=*/0,
            /*name_max=*/NAME_MAX,
            /*no_trunc=*/true,
            /*chown_restricted=*/true,
            /*case_insensitive=*/caseSensitive_ == CaseSensitivity::Insensitive,
            /*case_preserving=*/true,
        }}}};

  XdrTrait<PATHCONF3res>::serialize(ser, res);

  return folly::unit;
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::commit(
    folly::io::Cursor /*deser*/,
    folly::io::QueueAppender ser,
    NfsRequestContext& context) {
  serializeReply(ser, accept_stat::PROC_UNAVAIL, context.getXid());
  return folly::unit;
}

NfsArgsDetails formatNull(folly::io::Cursor /*deser*/) {
  return {""};
}

NfsArgsDetails formatGetattr(folly::io::Cursor deser) {
  auto args = XdrTrait<GETATTR3args>::deserialize(deser);
  return {fmt::format(FMT_STRING("ino={}"), args.object.ino), args.object.ino};
}

NfsArgsDetails formatSattr3(const sattr3& attr) {
  auto formatOpt = [](auto&& val, const char* fmtString = "{}") {
    using T = std::decay_t<decltype(val)>;
    if (val.tag) {
      return fmt::format(
          fmt::runtime(fmtString), std::get<typename T::TrueVariant>(val.v));
    }
    return std::string();
  };

  auto formatTime = [](const auto& time) {
    if (time.tag == time_how::SET_TO_SERVER_TIME) {
      return std::string{"now"};
    }
    if (time.tag == time_how::SET_TO_CLIENT_TIME) {
      const auto& value = std::get<nfstime3>(time.v);
      return fmt::format("{}.{:09}", value.seconds, value.nseconds);
    }
    return std::string{};
  };
  return fmt::format(
      FMT_STRING("mode={}, uid={}, gid={}, size={}, atime={}, mtime={}"),
      formatOpt(attr.mode, "{:#o}"),
      formatOpt(attr.uid),
      formatOpt(attr.gid),
      formatOpt(attr.size),
      formatTime(attr.atime),
      formatTime(attr.mtime));
}

NfsArgsDetails formatSetattr(folly::io::Cursor deser) {
  auto args = XdrTrait<SETATTR3args>::deserialize(deser);
  return {
      fmt::format(
          FMT_STRING("ino={}, attr=({}) guarded={}"),
          args.object.ino,
          formatSattr3(args.new_attributes).str,
          args.guard.tag),
      args.object.ino};
}

NfsArgsDetails formatLookup(folly::io::Cursor deser) {
  auto args = XdrTrait<LOOKUP3args>::deserialize(deser);
  return {
      fmt::format(
          FMT_STRING("dir={}, name={}"), args.what.dir.ino, args.what.name),
      args.what.dir.ino};
}

NfsArgsDetails formatAccess(folly::io::Cursor deser) {
  auto args = XdrTrait<ACCESS3args>::deserialize(deser);
  return {
      fmt::format(
          FMT_STRING("ino={}, access={:#x}"), args.object.ino, args.access),
      args.object.ino};
}

NfsArgsDetails formatReadlink(folly::io::Cursor deser) {
  auto args = XdrTrait<READLINK3args>::deserialize(deser);
  return {
      fmt::format(FMT_STRING("ino={}"), args.symlink.ino), args.symlink.ino};
}

NfsArgsDetails formatRead(folly::io::Cursor deser) {
  auto args = XdrTrait<READ3args>::deserialize(deser);
  return {
      fmt::format(
          FMT_STRING("ino={}, size={}, offset={}"),
          args.file.ino,
          args.count,
          args.offset),
      args.file.ino};
}

NfsArgsDetails formatWrite(folly::io::Cursor deser) {
  auto args = XdrTrait<WRITE3args>::deserialize(deser);
  auto formatStable = [](stable_how stable) {
    switch (stable) {
      case stable_how::UNSTABLE:
        return "UNSTABLE";
      case stable_how::DATA_SYNC:
        return "DATA_SYNC";
      case stable_how::FILE_SYNC:
        return "FILE_SYNC";
    }
    throw_<std::domain_error>(
        "unexpected stable_how ", folly::to_underlying(stable));
  };
  return {
      fmt::format(
          FMT_STRING("ino={}, size={}, offset={}, stable={}"),
          args.file.ino,
          args.count,
          args.offset,
          formatStable(args.stable)),
      args.file.ino};
}

NfsArgsDetails formatCreate(folly::io::Cursor deser) {
  auto args = XdrTrait<CREATE3args>::deserialize(deser);
  auto formatMode = [](createmode3 createmode) {
    switch (createmode) {
      case createmode3::UNCHECKED:
        return "UNCHECKED";
      case createmode3::GUARDED:
        return "GUARDED";
      case createmode3::EXCLUSIVE:
        return "EXCLUSIVE";
    }
    throw_<std::domain_error>(
        "unexpected createmode3 ", folly::to_underlying(createmode));
  };
  return {
      fmt::format(
          FMT_STRING("dir={}, name={}, mode={}{}"),
          args.where.dir.ino,
          args.where.name,
          formatMode(args.how.tag),
          args.how.tag != createmode3::EXCLUSIVE
              ? fmt::format(
                    FMT_STRING(" attr=({})"),
                    formatSattr3(std::get<sattr3>(args.how.v)).str)
              : ""),
      args.where.dir.ino};
}

NfsArgsDetails formatMkdir(folly::io::Cursor deser) {
  auto args = XdrTrait<MKDIR3args>::deserialize(deser);
  return {
      fmt::format(
          FMT_STRING("dir={}, name={}, attr=({})"),
          args.where.dir.ino,
          args.where.name,
          formatSattr3(args.attributes).str),
      args.where.dir.ino};
}

NfsArgsDetails formatSymlink(folly::io::Cursor deser) {
  auto args = XdrTrait<SYMLINK3args>::deserialize(deser);
  return {
      fmt::format(
          FMT_STRING("dir={}, name={}, symlink={}, attr=({})"),
          args.where.dir.ino,
          args.where.name,
          args.symlink.symlink_data,
          formatSattr3(args.symlink.symlink_attributes).str),
      args.where.dir.ino};
}

NfsArgsDetails formatMknod(folly::io::Cursor deser) {
  auto args = XdrTrait<MKNOD3args>::deserialize(deser);
  auto formatFtype = [](const ftype3& type) {
    switch (type) {
      case ftype3::NF3REG:
        return "REG";
      case ftype3::NF3DIR:
        return "DIR";
      case ftype3::NF3BLK:
        return "BLK";
      case ftype3::NF3CHR:
        return "CHR";
      case ftype3::NF3LNK:
        return "LNK";
      case ftype3::NF3SOCK:
        return "SOCK";
      case ftype3::NF3FIFO:
        return "FIFO";
    }
    throw_<std::domain_error>("unexpected ftype3 ", folly::to_underlying(type));
  };
  auto formatWhat = [](const mknoddata3& data) {
    return std::visit(
        [](auto&& arg) -> std::string {
          using ArgType = std::decay_t<decltype(arg)>;
          if constexpr (std::is_same_v<ArgType, devicedata3>) {
            // TODO(xavierd): format the specdata3 too.
            return fmt::format(
                ", attr=({})", formatSattr3(arg.dev_attributes).str);
          } else if constexpr (std::is_same_v<ArgType, sattr3>) {
            return fmt::format(", attr=({})", formatSattr3(arg).str);
          } else {
            return "";
          }
        },
        data.v);
  };
  return {
      fmt::format(
          FMT_STRING("dir={}, name={}, type={}{}"),
          args.where.dir.ino,
          args.where.name,
          formatFtype(args.what.tag),
          formatWhat(args.what)),
      args.where.dir.ino};
}

NfsArgsDetails formatRemove(folly::io::Cursor deser) {
  auto args = XdrTrait<REMOVE3args>::deserialize(deser);
  return {
      fmt::format(
          FMT_STRING("dir={}, name={}"), args.object.dir.ino, args.object.name),
      args.object.dir.ino};
}

NfsArgsDetails formatRmdir(folly::io::Cursor deser) {
  auto args = XdrTrait<RMDIR3args>::deserialize(deser);
  return {
      fmt::format(
          FMT_STRING("dir={}, name={}"), args.object.dir.ino, args.object.name),
      args.object.dir.ino};
}

NfsArgsDetails formatRename(folly::io::Cursor deser) {
  auto args = XdrTrait<RENAME3args>::deserialize(deser);
  return {
      fmt::format(
          FMT_STRING("fromDir={}, fromName={}, toDir={}, toName={}"),
          args.from.dir.ino,
          args.from.name,
          args.to.dir.ino,
          args.to.name),
      args.to.dir.ino};
}

NfsArgsDetails formatLink(folly::io::Cursor deser) {
  auto args = XdrTrait<LINK3args>::deserialize(deser);
  return {
      fmt::format(
          FMT_STRING("ino={}, dir={}, name={}"),
          args.file.ino,
          args.link.dir.ino,
          args.link.name),
      args.file.ino};
}

NfsArgsDetails formatReaddir(folly::io::Cursor deser) {
  auto args = XdrTrait<READDIR3args>::deserialize(deser);
  return {
      fmt::format(
          FMT_STRING("dir={}, cookie={}, cookieverf={}, count={}"),
          args.dir.ino,
          args.cookie,
          args.cookieverf,
          args.count),
      args.dir.ino};
}

NfsArgsDetails formatReaddirplus(folly::io::Cursor deser) {
  auto args = XdrTrait<READDIRPLUS3args>::deserialize(deser);
  return {
      fmt::format(
          FMT_STRING(
              "dir={}, cookie={}, cookieverf={}, dircount={}, maxcount={}"),
          args.dir.ino,
          args.cookie,
          args.cookieverf,
          args.dircount,
          args.maxcount),
      args.dir.ino};
}

NfsArgsDetails formatFsstat(folly::io::Cursor deser) {
  auto args = XdrTrait<FSSTAT3args>::deserialize(deser);
  return {fmt::format(FMT_STRING("ino={}"), args.fsroot.ino), args.fsroot.ino};
}

NfsArgsDetails formatFsinfo(folly::io::Cursor deser) {
  auto args = XdrTrait<FSINFO3args>::deserialize(deser);
  return {fmt::format(FMT_STRING("ino={}"), args.fsroot.ino), args.fsroot.ino};
}

NfsArgsDetails formatPathconf(folly::io::Cursor deser) {
  auto args = XdrTrait<PATHCONF3args>::deserialize(deser);
  return {fmt::format(FMT_STRING("ino={}"), args.object.ino), args.object.ino};
}

NfsArgsDetails formatCommit(folly::io::Cursor /*deser*/) {
  // TODO(xavierd): Fill this in.
  return {""};
}

using Handler = ImmediateFuture<folly::Unit> (Nfsd3ServerProcessor::*)(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    NfsRequestContext& context);

/**
 * Format the passed in arguments. The Cursor must be passed as a copy to avoid
 * disrupting the actual handler.
 */
using FormatArgs = NfsArgsDetails (*)(folly::io::Cursor deser);
using AccessType = ProcessAccessLog::AccessType;

/**
 * The inode a request operates on, read from its arguments without
 * formatting them: the file handle they start with, or for RENAME the
 * destination directory, the same inode the formatters report.
 */
std::optional<InodeNumber> peekInodeNumber(
    uint32_t procNumber,
    folly::io::Cursor deser) {
  const auto proc = static_cast<nfsv3Procs>(procNumber);
  if (proc == nfsv3Procs::null || proc == nfsv3Procs::commit) {
    return std::nullopt;
  }
  if (proc == nfsv3Procs::rename) {
    return XdrTrait<RENAME3args>::deserialize(deser).to.dir.ino;
  }
  return XdrTrait<nfs_fh3>::deserialize(deser).ino;
}

struct HandlerEntry {
  constexpr HandlerEntry() = default;
  constexpr HandlerEntry(
      folly::StringPiece n,
      Handler h,
      FormatArgs format,
      NfsStats::DurationPtr d,
      NfsStats::CounterPtr cS,
      NfsStats::CounterPtr cF,
      AccessType at = AccessType::FsChannelOther)
      : name(n),
        handler(h),
        formatArgs(format),
        duration{d},
        countSuccessful{cS},
        countFailure{cF},
        accessType(at) {}

  folly::StringPiece name;
  Handler handler = nullptr;
  FormatArgs formatArgs = nullptr;
  NfsStats::DurationPtr duration = nullptr;
  NfsStats::CounterPtr countSuccessful = nullptr;
  NfsStats::CounterPtr countFailure = nullptr;
  AccessType accessType = AccessType::FsChannelOther;
};

constexpr auto kNfs3dHandlers = [] {
  const auto Read = AccessType::FsChannelRead;
  const auto Write = AccessType::FsChannelWrite;

  std::array<HandlerEntry, 22> handlers;
  handlers[folly::to_underlying(nfsv3Procs::null)] = {
      "NULL",
      &Nfsd3ServerProcessor::null,
      formatNull,
      &NfsStats::nfsNull,
      &NfsStats::nfsNullSuccessful,
      &NfsStats::nfsNullFailure};
  handlers[folly::to_underlying(nfsv3Procs::getattr)] = {
      "GETATTR",
      &Nfsd3ServerProcessor::getattr,
      formatGetattr,
      &NfsStats::nfsGetattr,
      &NfsStats::nfsGetattrSuccessful,
      &NfsStats::nfsGetattrFailure,
      Read};
  handlers[folly::to_underlying(nfsv3Procs::setattr)] = {
      "SETATTR",
      &Nfsd3ServerProcessor::setattr,
      formatSetattr,
      &NfsStats::nfsSetattr,
      &NfsStats::nfsSetattrSuccessful,
      &NfsStats::nfsSetattrFailure,
      Write};
  handlers[folly::to_underlying(nfsv3Procs::lookup)] = {
      "LOOKUP",
      &Nfsd3ServerProcessor::lookup,
      formatLookup,
      &NfsStats::nfsLookup,
      &NfsStats::nfsLookupSuccessful,
      &NfsStats::nfsLookupFailure,
      Read};
  handlers[folly::to_underlying(nfsv3Procs::access)] = {
      "ACCESS",
      &Nfsd3ServerProcessor::access,
      formatAccess,
      &NfsStats::nfsAccess,
      &NfsStats::nfsAccessSuccessful,
      &NfsStats::nfsAccessFailure,
      Read};
  handlers[folly::to_underlying(nfsv3Procs::readlink)] = {
      "READLINK",
      &Nfsd3ServerProcessor::readlink,
      formatReadlink,
      &NfsStats::nfsReadlink,
      &NfsStats::nfsReadlinkSuccessful,
      &NfsStats::nfsReadlinkFailure,
      Read};
  handlers[folly::to_underlying(nfsv3Procs::read)] = {
      "READ",
      &Nfsd3ServerProcessor::read,
      formatRead,
      &NfsStats::nfsRead,
      &NfsStats::nfsReadSuccessful,
      &NfsStats::nfsReadFailure,
      Read};
  handlers[folly::to_underlying(nfsv3Procs::write)] = {
      "WRITE",
      &Nfsd3ServerProcessor::write,
      formatWrite,
      &NfsStats::nfsWrite,
      &NfsStats::nfsWriteSuccessful,
      &NfsStats::nfsWriteFailure,
      Write};
  handlers[folly::to_underlying(nfsv3Procs::create)] = {
      "CREATE",
      &Nfsd3ServerProcessor::create,
      formatCreate,
      &NfsStats::nfsCreate,
      &NfsStats::nfsCreateSuccessful,
      &NfsStats::nfsCreateFailure,
      Write};
  handlers[folly::to_underlying(nfsv3Procs::mkdir)] = {
      "MKDIR",
      &Nfsd3ServerProcessor::mkdir,
      formatMkdir,
      &NfsStats::nfsMkdir,
      &NfsStats::nfsMkdirSuccessful,
      &NfsStats::nfsMkdirFailure,
      Write};
  handlers[folly::to_underlying(nfsv3Procs::symlink)] = {
      "SYMLINK",
      &Nfsd3ServerProcessor::symlink,
      formatSymlink,
      &NfsStats::nfsSymlink,
      &NfsStats::nfsSymlinkSuccessful,
      &NfsStats::nfsSymlinkFailure,
      Write};
  handlers[folly::to_underlying(nfsv3Procs::mknod)] = {
      "MKNOD",
      &Nfsd3ServerProcessor::mknod,
      formatMknod,
      &NfsStats::nfsMknod,
      &NfsStats::nfsMknodSuccessful,
      &NfsStats::nfsMknodFailure,
      Write};
  handlers[folly::to_underlying(nfsv3Procs::remove)] = {
      "REMOVE",
      &Nfsd3ServerProcessor::remove,
      formatRemove,
      &NfsStats::nfsRemove,
      &NfsStats::nfsRemoveSuccessful,
      &NfsStats::nfsRemoveFailure,
      Write};
  handlers[folly::to_underlying(nfsv3Procs::rmdir)] = {
      "RMDIR",
      &Nfsd3ServerProcessor::rmdir,
      formatRmdir,
      &NfsStats::nfsRmdir,
      &NfsStats::nfsRmdirSuccessful,
      &NfsStats::nfsRmdirFailure,
      Write};
  handlers[folly::to_underlying(nfsv3Procs::rename)] = {
      "RENAME",
      &Nfsd3ServerProcessor::rename,
      formatRename,
      &NfsStats::nfsRename,
      &NfsStats::nfsRenameSuccessful,
      &NfsStats::nfsRenameFailure,
      Write};
  handlers[folly::to_underlying(nfsv3Procs::link)] = {
      "LINK",
      &Nfsd3ServerProcessor::link,
      formatLink,
      &NfsStats::nfsLink,
      &NfsStats::nfsLinkSuccessful,
      &NfsStats::nfsLinkFailure,
      Write};
  handlers[folly::to_underlying(nfsv3Procs::readdir)] = {
      "READDIR",
      &Nfsd3ServerProcessor::readdir,
      formatReaddir,
      &NfsStats::nfsReaddir,
      &NfsStats::nfsReaddirSuccessful,
      &NfsStats::nfsReaddirFailure,
      Read};
  handlers[folly::to_underlying(nfsv3Procs::readdirplus)] = {
      "READDIRPLUS",
      &Nfsd3ServerProcessor::readdirplus,
      formatReaddirplus,
      &NfsStats::nfsReaddirplus,
      &NfsStats::nfsReaddirplusSuccessful,
      &NfsStats::nfsReaddirplusFailure,
      Read};
  handlers[folly::to_underlying(nfsv3Procs::fsstat)] = {
      "FSSTAT",
      &Nfsd3ServerProcessor::fsstat,
      formatFsstat,
      &NfsStats::nfsFsstat,
      &NfsStats::nfsFsstatSuccessful,
      &NfsStats::nfsFsstatFailure};
  handlers[folly::to_underlying(nfsv3Procs::fsinfo)] = {
      "FSINFO",
      &Nfsd3ServerProcessor::fsinfo,
      formatFsinfo,
      &NfsStats::nfsFsinfo,
      &NfsStats::nfsFsinfoSuccessful,
      &NfsStats::nfsFsinfoFailure};
  handlers[folly::to_underlying(nfsv3Procs::pathconf)] = {
      "PATHCONF",
      &Nfsd3ServerProcessor::pathconf,
      formatPathconf,
      &NfsStats::nfsPathconf,
      &NfsStats::nfsPathconfSuccessful,
      &NfsStats::nfsPathconfFailure};
  handlers[folly::to_underlying(nfsv3Procs::commit)] = {
      "COMMIT",
      &Nfsd3ServerProcessor::commit,
      formatCommit,
      &NfsStats::nfsCommit,
      &NfsStats::nfsCommitSuccessful,
      &NfsStats::nfsCommitFailure,
      Write};

  return handlers;
}();

/**
 * The handler table's procedure names in lowercase, the spelling
 * nfs:access-policy-procedures uses, built once on first use.
 */
const std::array<std::string, kNfs3dHandlers.size()>& lowerProcNames() {
  static const auto names = [] {
    std::array<std::string, kNfs3dHandlers.size()> lower;
    for (size_t proc = 0; proc < kNfs3dHandlers.size(); ++proc) {
      lower[proc] = kNfs3dHandlers[proc].name.str();
      folly::toLowerAscii(lower[proc]);
    }
    return lower;
  }();
  return names;
}

/**
 * Whether `proc` is in nfs:access-policy-procedures: one hash lookup of the
 * procedure's lowercase name. Any other entry in the set, including a
 * procedure name in different casing, matches nothing, and a procedure
 * number outside the handler table is never policed.
 */
bool isPolicedProcedure(
    uint32_t proc,
    const std::unordered_set<std::string>& procedures) {
  return proc < kNfs3dHandlers.size() &&
      procedures.contains(lowerProcNames()[proc]);
}

bool Nfsd3ServerProcessor::isUnimplementedProc(uint32_t proc) const {
  return proc == folly::to_underlying(nfsv3Procs::commit) ||
      proc >= kNfs3dHandlers.size();
}

void Nfsd3ServerProcessor::onRequestComplete(const RpcRequestTimeline& t) {
  auto stats = dispatcher_->getStats().copy();

  if (t.requestReceived && t.dispatched) {
    stats->addDuration(
        &NfsStats::nfsPhaseAccept, *t.dispatched - *t.requestReceived);
  }
  if (t.dispatched && t.handlerStart) {
    stats->addDuration(
        &NfsStats::nfsPhaseQueueWait, *t.handlerStart - *t.dispatched);
  }
  if (t.handlerStart && t.handlerDone) {
    stats->addDuration(
        &NfsStats::nfsPhaseProcessing, *t.handlerDone - *t.handlerStart);
  }
  if (t.handlerDone && t.responseSent) {
    stats->addDuration(
        &NfsStats::nfsPhaseWriteWait, *t.responseSent - *t.handlerDone);
  }

  if (t.requestReceived && t.responseSent) {
    auto total = *t.responseSent - *t.requestReceived;
    stats->addDuration(&NfsStats::nfsPhaseTotal, total);

    if (longRunningFSRequestThreshold_.count() > 0 &&
        total > longRunningFSRequestThreshold_) {
      using namespace std::chrono;
      auto durationNs = [](auto d) {
        return static_cast<double>(duration_cast<nanoseconds>(d).count());
      };
      auto procName = t.procNumber < kNfs3dHandlers.size()
          ? kNfs3dHandlers[t.procNumber].name
          : "unknown";
      edenFsEventsLogger_->logEvent(
          LongRunningFSRequest{
              durationNs(total),
              procName,
              t.dispatched && t.requestReceived
                  ? durationNs(*t.dispatched - *t.requestReceived)
                  : 0.0,
              t.handlerStart && t.dispatched
                  ? durationNs(*t.handlerStart - *t.dispatched)
                  : 0.0,
              t.handlerDone && t.handlerStart
                  ? durationNs(*t.handlerDone - *t.handlerStart)
                  : 0.0,
              t.responseSent && t.handlerDone
                  ? durationNs(*t.responseSent - *t.handlerDone)
                  : 0.0,
          });
    }
  }
}

namespace {
/**
 * Publish the START trace event for a request, with its formatted arguments
 * while `eden trace fs --verbose` is attached.
 */
void publishStartEvent(
    TraceBus<NfsTraceEvent>& traceBus,
    const std::atomic<size_t>& traceDetailedArguments,
    const HandlerEntry& handlerEntry,
    folly::io::Cursor deser,
    uint32_t xid,
    uint32_t procNumber,
    const std::optional<authsys_parms>& authSysCreds) {
  if (!traceDetailedArguments.load(std::memory_order_acquire)) {
    traceBus.publish(NfsTraceEvent::start(xid, procNumber));
    return;
  }
  auto details = handlerEntry.formatArgs(deser);
  // The kernel issues requests of its own, as root, next to those it
  // makes for processes; the credential tells them apart in a trace.
  if (authSysCreds) {
    // Named apart from the uid and gid a SETATTR asks to set.
    details.str += fmt::format(
        "{}cred_uid={}, cred_gid={}",
        details.str.empty() ? "" : ", ",
        authSysCreds->uid,
        authSysCreds->gid);
  }
  traceBus.publish(NfsTraceEvent::start(xid, procNumber, std::move(details)));
}

/**
 * Whether the procedure resolves a directory's entries, handing the client
 * handles for (or names of) its children.
 */
bool resolvesEntries(uint32_t procNumber) {
  const auto proc = static_cast<nfsv3Procs>(procNumber);
  return proc == nfsv3Procs::lookup || proc == nfsv3Procs::readdir ||
      proc == nfsv3Procs::readdirplus;
}

/**
 * Serialize a JUKEBOX error response for the given NFS procedure.
 *
 * Each NFS procedure has a different response type with its own fail fields
 * (post_op_attr, wcc_data, etc.). We serialize the correct per-procedure
 * response with default-constructed fail fields for RFC compliance.
 */
template <typename ResType>
void serializeJukeboxResponse(folly::io::QueueAppender& ser, uint32_t xid) {
  serializeReply(ser, accept_stat::SUCCESS, xid);
  ResType res;
  res.tag = nfsstat3::NFS3ERR_JUKEBOX;
  if constexpr (!std::is_same_v<typename ResType::Default, std::monostate>) {
    res.v = typename ResType::Default{};
  }
  XdrTrait<ResType>::serialize(ser, res);
}

void serializeJukeboxError(
    folly::io::QueueAppender& ser,
    uint32_t xid,
    uint32_t procNumber) {
  switch (procNumber) {
    // null is fast-pathed as SUCCESS; commit and unknown procs are
    // fast-pathed as PROC_UNAVAIL. Neither reaches this switch.
    case folly::to_underlying(nfsv3Procs::getattr):
      serializeJukeboxResponse<GETATTR3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::setattr):
      serializeJukeboxResponse<SETATTR3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::lookup):
      serializeJukeboxResponse<LOOKUP3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::access):
      serializeJukeboxResponse<ACCESS3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::readlink):
      serializeJukeboxResponse<READLINK3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::read):
      serializeJukeboxResponse<READ3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::write):
      serializeJukeboxResponse<WRITE3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::create):
      serializeJukeboxResponse<CREATE3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::mkdir):
      serializeJukeboxResponse<MKDIR3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::symlink):
      serializeJukeboxResponse<SYMLINK3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::mknod):
      serializeJukeboxResponse<MKNOD3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::remove):
      serializeJukeboxResponse<REMOVE3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::rmdir):
      serializeJukeboxResponse<RMDIR3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::rename):
      serializeJukeboxResponse<RENAME3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::link):
      serializeJukeboxResponse<LINK3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::readdir):
      serializeJukeboxResponse<READDIR3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::readdirplus):
      serializeJukeboxResponse<READDIRPLUS3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::fsstat):
      serializeJukeboxResponse<FSSTAT3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::fsinfo):
      serializeJukeboxResponse<FSINFO3res>(ser, xid);
      break;
    case folly::to_underlying(nfsv3Procs::pathconf):
      serializeJukeboxResponse<PATHCONF3res>(ser, xid);
      break;
    default:
      // null is fast-pathed as SUCCESS, commit and unknown procs are
      // fast-pathed as PROC_UNAVAIL, so this default should never be
      // reached for valid NFS procedures.
      XDCHECK(false) << "Unexpected proc in serializeJukeboxError: "
                     << procNumber;
      serializeReply(ser, accept_stat::PROC_UNAVAIL, xid);
      break;
  }
}

} // namespace

RpcServerProcessor::InlineRejectResult Nfsd3ServerProcessor::tryInlineReject() {
  if (!fsChannel_ || !fsChannel_->isRateLimitingEnabled()) {
    return {};
  }

  auto permit = fsChannel_->tryAcquireFsRequestPermit();
  if (!permit) {
    dispatcher_->getStats()->increment(&NfsStats::nfsBackpressureJukebox);
    return {true, nullptr};
  }
  return {false, std::move(permit)};
}

void Nfsd3ServerProcessor::serializeInlineReject(
    uint32_t proc,
    uint32_t xid,
    folly::io::QueueAppender& ser) {
  serializeJukeboxError(ser, xid, proc);
}

bool Nfsd3ServerProcessor::shouldParseAuthSysCreds() {
  // A detailed trace shows each request's uid and gid, which needs the
  // credential parsed.
  if (traceDetailedArguments_.load(std::memory_order_acquire) != 0) {
    return true;
  }
  if (!config_) {
    return false;
  }
  // Single decision point for the credential fast path: with no uid or gid
  // entries configured, requests need no identity and the per-request
  // AUTH_SYS parse is skipped entirely. Both maps are read off one snapshot.
  auto config = config_->getEdenConfig();
  return !config->nfsUidAccessPolicy.getValue().empty() ||
      !config->nfsGidAccessPolicy.getValue().empty();
}

auth_stat Nfsd3ServerProcessor::checkAuthentication(
    const call_body& callBody,
    const std::optional<authsys_parms>& authSysCreds) {
  // Control-plane procedures are exempt (see isAccessModeExempt), and
  // requests without a parsable AUTH_SYS credential carry no identity;
  // neither is ever acted on.
  if (!config_ || !authSysCreds || isAccessModeExempt(callBody.proc)) {
    return auth_stat::AUTH_OK;
  }
  // Every uid/gid entry the credential matches is counted, not just the
  // first, and any rejecting one rejects the request. Block/rate_limit and
  // the rate-limit budget apply only to procedures in
  // nfs:access-policy-procedures; other procedures never reach the limiter.
  auto config = config_->getEdenConfig();
  // Resolved on the first matching entry, at most once per request: most
  // requests match no entry at all.
  std::optional<bool> policed;
  auto isPoliced = [&] {
    if (!policed) {
      policed = isPolicedProcedure(
          callBody.proc, config->nfsAccessPolicyProcedures.getValue());
    }
    return *policed;
  };
  const auto count = config->nfsAccessPolicyRateLimitCount.getValue();
  const auto windowSeconds =
      config->nfsAccessPolicyRateLimitWindowSeconds.getValue();
  // Per-procedure keys use the lowercase handler name; a procedure number
  // outside the table has none and is counted per id only.
  const std::string* procName = callBody.proc < kNfs3dHandlers.size()
      ? &lowerProcNames()[callBody.proc]
      : nullptr;
  bool block = false;

  const auto& uidPolicy = config->nfsUidAccessPolicy.getValue();
  if (auto entry = uidPolicy.find(authSysCreds->uid);
      entry != uidPolicy.end()) {
    STATS_nfs_access_uid.add(1, authSysCreds->uid);
    if (procName) {
      STATS_nfs_access_uid_proc.add(
          1, authSysCreds->uid, std::string_view{*procName});
    }
    if (isPoliced()) {
      STATS_nfs_policed_uid.add(1, authSysCreds->uid);
      if (accessModeRejects(
              entry->second,
              uidRateLimiters_,
              authSysCreds->uid,
              count,
              windowSeconds)) {
        STATS_nfs_blocked_uid.add(1, authSysCreds->uid);
        block = true;
      }
    }
  }
  for (const auto& [gid, mode] : config->nfsGidAccessPolicy.getValue()) {
    if (!credsHaveGid(*authSysCreds, gid)) {
      continue;
    }
    STATS_nfs_access_gid.add(1, gid);
    if (procName) {
      STATS_nfs_access_gid_proc.add(1, gid, std::string_view{*procName});
    }
    if (!isPoliced()) {
      continue;
    }
    STATS_nfs_policed_gid.add(1, gid);
    if (accessModeRejects(mode, gidRateLimiters_, gid, count, windowSeconds)) {
      STATS_nfs_blocked_gid.add(1, gid);
      block = true;
    }
  }
  if (!block) {
    return auth_stat::AUTH_OK;
  }
  dispatcher_->getStats()->increment(&NfsStats::nfsBlockedAccess);
  // AUTH_TOOWEAK rather than AUTH_REJECTEDCRED: NFS clients treat TOOWEAK
  // as terminal and surface a permission error to the caller (the macOS
  // client maps it to a clean EACCES), while REJECTEDCRED asks the client
  // to refresh its credential and retry, which would turn every blocked
  // call into a retry loop.
  return auth_stat::AUTH_TOOWEAK;
}

ImmediateFuture<folly::Unit> Nfsd3ServerProcessor::dispatchRpc(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    uint32_t xid,
    uint32_t progNumber,
    uint32_t progVersion,
    uint32_t procNumber,
    const std::optional<authsys_parms>& authSysCreds) {
  if (progNumber != kNfsdProgNumber) {
    serializeReply(ser, accept_stat::PROG_UNAVAIL, xid);
    return folly::unit;
  }

  if (progVersion != kNfsd3ProgVersion) {
    serializeReply(ser, accept_stat::PROG_MISMATCH, xid);
    XdrTrait<mismatch_info>::serialize(
        ser, mismatch_info{kNfsd3ProgVersion, kNfsd3ProgVersion});
    return folly::unit;
  }

  if (procNumber >= kNfs3dHandlers.size()) {
    XLOGF(ERR, "Invalid procedure: {}", procNumber);
    serializeReply(ser, accept_stat::PROC_UNAVAIL, xid);
    return folly::unit;
  }

  auto depth = inflightRequests_.fetch_add(1, std::memory_order_relaxed);
  dispatcher_->getStats()->increment(
      &NfsStats::nfsInflightAtRequest, depth + 1);

  auto& handlerEntry = kNfs3dHandlers[procNumber];
  FB_LOGF(
      *straceLogger_,
      DBG7,
      "{}({})",
      handlerEntry.name,
      handlerEntry.formatArgs(deser).str);

  auto inodeNumber = peekInodeNumber(procNumber, deser);

  const auto now = std::chrono::steady_clock::now();
  // Trace events only feed live `eden trace fs` streams, so they are not
  // built or published while nothing is subscribed.
  const bool traced = traceBus_->hasSubscription();
  if (traced) {
    publishStartEvent(
        *traceBus_,
        traceDetailedArguments_,
        handlerEntry,
        deser,
        xid,
        procNumber,
        authSysCreds);
  }
  requestTracker_.start(xid, now);

  // TODO: Add requestMetrics for NFS.
  std::shared_ptr<RequestMetricsScope::LockedRequestWatchList> nullRequestWatch;
  auto context = std::make_unique<NfsRequestContext>(
      xid,
      static_cast<nfsv3Procs>(procNumber),
      processAccessLog_,
      edenFsEventsLogger_,
      longRunningFSRequestThreshold_,
      authSysCreds);
  context->startRequest(
      dispatcher_->getStats().copy(), handlerEntry.duration, nullRequestWatch);
  // The data that contextRef reference to is alive for the duration of the
  // handler function and is deleted when context unique_ptr goes out of the
  // scope at the `ensure` lambda.
  auto& contextRef = *context;
  return makeImmediateFutureWith([&] {
           return (this->*handlerEntry.handler)(
               std::move(deser), std::move(ser), contextRef);
         })
      .thenValue(
          [this, inodeNumber = std::move(inodeNumber), procNumber, &contextRef](
              auto&&) {
            if (inodeNumber.has_value()) {
              const bool handsOutEntries =
                  resolvesEntries(procNumber) && !contextRef.isNegativeLookup();
              if (!handsOutEntries &&
                  invalidatingInodes_.contains(inodeNumber.value())) {
                XLOGF(
                    DBG9,
                    "Not updating last fs request time for inode {} being "
                    "invalidated",
                    inodeNumber.value());
                return ImmediateFuture<folly::Unit>(folly::unit);
              }
              XLOGF(
                  DBG9,
                  "Update last fs request time for inode: {}",
                  inodeNumber.value());
              return dispatcher_->markGcRef(
                  inodeNumber.value(), contextRef.getObjectFetchContext());
            }
            return ImmediateFuture<folly::Unit>(folly::unit);
          })
      .thenTry([this, &handlerEntry](folly::Try<folly::Unit>&& res) {
        if (res.hasException()) {
          if (dispatcher_->getStats() && handlerEntry.countFailure) {
            dispatcher_->getStats()->increment(handlerEntry.countFailure);
          }
          if (auto* err =
                  res.exception().get_mutable_exception<RpcParsingError>()) {
            err->setProcedureContext(std::string{handlerEntry.name});
          }
        } else if (dispatcher_->getStats() && handlerEntry.countSuccessful) {
          dispatcher_->getStats()->increment(handlerEntry.countSuccessful);
        }
        return res;
      })
      .ensure([this, xid, procNumber, context = std::move(context)]() {
        requestTracker_.finish(xid);
        // A subscriber may have attached after this request started and
        // learned of it through debugOutstandingNfsCalls, as `eden trace fs`
        // does.
        if (traceBus_->hasSubscription()) {
          traceBus_->publish(NfsTraceEvent::finish(xid, procNumber));
        }
        inflightRequests_.fetch_sub(1, std::memory_order_relaxed);
      });
}

void Nfsd3ServerProcessor::onShutdown(RpcStopData data) {
  // Clear fsChannel_ before triggering destruction. tryInlineReject checks
  // for nullptr, so this prevents accessing a dangling FsChannel pointer
  // during the shutdown window.
  fsChannel_ = nullptr;
  // Note this triggers the Nfsd3 destruction which will also destroy
  // Nfsd3ServerProcessor. Don't do anything with the Nfsd3ServerProcessor
  // member variables after this!
  stopPromise_.setValue(std::make_unique<RpcStopData>(std::move(data)));
}

void Nfsd3ServerProcessor::clientConnected() {
  auto numberOfClients =
      numberOfClients_.fetch_add(1, std::memory_order_acq_rel);
  if (numberOfClients > 1) {
    edenFsEventsLogger_->logEvent(TooManyNfsClients{});
  }
}
} // namespace

void NfsRequestTracker::start(
    uint32_t xid,
    std::chrono::steady_clock::time_point startTime) {
  auto state = stripes_[xid % kStripes].wlock();
  (void)state->requests.emplace(xid, OutstandingRequest{xid, startTime});
}

void NfsRequestTracker::finish(uint32_t xid) {
  auto state = stripes_[xid % kStripes].wlock();
  state->requests.erase(xid);
}

std::vector<NfsRequestTracker::OutstandingRequest>
NfsRequestTracker::outstanding() const {
  std::vector<OutstandingRequest> result;
  for (const auto& stripe : stripes_) {
    auto state = stripe.rlock();
    for (const auto& entry : state->requests) {
      result.push_back(entry.second);
    }
  }
  return result;
}

Nfsd3::Nfsd3(
    PrivHelper* privHelper,
    AbsolutePath mountPath,
    folly::EventBase* evb,
    std::shared_ptr<folly::Executor> threadPool,
    std::unique_ptr<NfsDispatcher> dispatcher,
    const folly::Logger* straceLogger,
    std::shared_ptr<ProcessInfoCache> processInfoCache,
    const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
    ErrorLogger& errorLogger,
    folly::Duration /*requestTimeout*/,
    std::shared_ptr<Notifier> /*notifier*/,
    CaseSensitivity caseSensitive,
    uint32_t readIoSize,
    uint32_t writeIoSize,
    size_t maximumInFlightRequests,
    std::chrono::nanoseconds highNfsRequestsLogInterval,
    std::chrono::nanoseconds longRunningFSRequestThreshold,
    size_t traceBusCapacity,
    bool fastPathRPCs,
    std::shared_ptr<ReloadableConfig> config,
    FaultInjector& faultInjector)
    : privHelper_{privHelper},
      mountPath_{std::move(mountPath)},
      faultInjector_{faultInjector},
      stats_{dispatcher->getStats().copy()},
      server_([&]() {
        auto proc = std::make_shared<Nfsd3ServerProcessor>(
            std::move(dispatcher),
            straceLogger,
            edenFsEventsLogger,
            errorLogger,
            mountPath_,
            caseSensitive,
            readIoSize,
            writeIoSize,
            stopPromise_,
            processAccessLog_,
            traceDetailedArguments_,
            traceBus_,
            requestTracker_,
            longRunningFSRequestThreshold,
            fastPathRPCs,
            // Not moved: the invalidation queue below reads its thread count
            // from the config too.
            config,
            invalidatingInodes_);
        proc->setFsChannel(this);
        return RpcServer::create(
            std::move(proc),
            evb,
            std::move(threadPool),
            edenFsEventsLogger,
            maximumInFlightRequests,
            highNfsRequestsLogInterval,
            config ? config->getEdenConfig()->nfsSocketBufferSize.getValue()
                   : 0);
      }()),
      processAccessLog_(std::move(processInfoCache)),
      edenFsEventsLogger_{edenFsEventsLogger},
      traceDetailedArguments_{0},
      traceBus_{TraceBus<NfsTraceEvent>::create("NfsTrace", traceBusCapacity)},
      invalidationQueue_{
          config ? config->getEdenConfig()->nfsNumInvalidationThreads.getValue()
                 : 1,
          [this](Invalidation& invalidation) { runInvalidation(invalidation); },
          fmt::format("nfsinval{}", mountPath_.basename())} {
  XLOGF(
      INFO,
      "Creating Nfsd3: mountPath={}, caseSensitive={}",
      mountPath_,
      caseSensitive);
  invalidationQueue_.start();

  initializeInflightRequestsRateLimiter(maximumInFlightRequests);
}

folly::Future<FsChannel::StopFuture> Nfsd3::initialize() {
  return folly::makeFuture<FsChannel::StopFuture>(getStopFuture());
}

void Nfsd3::initialize(folly::SocketAddress addr) {
  server_->initialize(addr);
}

void Nfsd3::initialize(folly::File connectedSocket) {
  XLOGF(
      DBG7,
      "Initializing nfsd3 with connected socket: {}",
      connectedSocket.fd());
  server_->initializeConnectedSocket(std::move(connectedSocket));
}

folly::SemiFuture<folly::Unit> Nfsd3::unmount(UnmountOptions /* options */) {
  return privHelper_->nfsUnmount(mountPath_.view());
}

void Nfsd3::invalidate(
    AbsolutePath path,
    mode_t mode,
    std::optional<NfsInvalidationSource> source) {
  invalidationQueue_.add(
      Invalidation{.path = std::move(path), .mode = mode, .source = source});
}

std::optional<folly::SemiFuture<std::optional<uint64_t>>>
Nfsd3::invalidateWithQueueLimit(
    AbsolutePath path,
    mode_t mode,
    folly::Function<uint64_t()> forget,
    std::vector<InodeNumber> lineage,
    size_t maxQueueSize,
    const folly::CancellationToken& cancellationToken) {
  folly::Promise<uint64_t> result;
  auto resultFuture = result.getSemiFuture();
  Invalidation invalidation{
      std::move(path),
      mode,
      NfsInvalidationSource::Gc,
      std::move(lineage),
      [forget = std::move(forget), result = std::move(result)]() mutable {
        auto outcome = folly::makeTryWith([&] { return forget(); });
        // Release the inode references forget holds before waking the walk.
        forget = nullptr;
        result.setTry(std::move(outcome));
      },
      std::move(resultFuture)};
  invalidation.done = folly::Promise<std::optional<uint64_t>>{};
  auto done = invalidation.done.getSemiFuture();
  if (!invalidationQueue_.addWithLimit(
          std::move(invalidation), maxQueueSize, cancellationToken)) {
    return std::nullopt;
  }
  return done;
}

/**
 * Runs on the invalidation queue's threads. The chmod runs off the request
 * threads because both the kernel and EdenFS hold locks that would otherwise
 * deadlock; see InvalidationQueue.
 */
void InvalidatingInodes::add(
    const std::vector<InodeNumber>& lineage,
    folly::Function<void()> forget) {
  if (lineage.empty()) {
    return;
  }
  // A replaced forget holds inode references and is destroyed outside the
  // lock, as in remove().
  folly::Function<void()> replaced;
  auto inodes = inodes_.wlock();
  for (auto ino : lineage) {
    ++(*inodes)[ino].count;
  }
  auto& head = (*inodes)[lineage[0]];
  if (head.forget) {
    // Both forgets clear the same directory's children, so replacing the
    // earlier one only leaves its walk reporting no invalidation.
    XLOGF_EVERY_MS(
        ERR,
        60'000,
        "directory {} is already being invalidated",
        lineage[0].get());
  }
  replaced = std::exchange(head.forget, std::move(forget));
  numLineages_.fetch_add(1, std::memory_order_release);
}

void InvalidatingInodes::remove(const std::vector<InodeNumber>& lineage) {
  if (lineage.empty()) {
    return;
  }
  // A forget that no SETATTR took holds inode references; destroying it
  // can take inode locks, so it dies after the lock below is released.
  folly::Function<void()> forget;
  auto inodes = inodes_.wlock();
  if (auto head = inodes->find(lineage[0]); head != inodes->end()) {
    forget = std::move(head->second.forget);
  }
  for (auto ino : lineage) {
    auto it = inodes->find(ino);
    XDCHECK(it != inodes->end()) << "inode " << ino.get() << " not registered";
    if (it != inodes->end() && --it->second.count == 0) {
      inodes->erase(it);
    }
  }
  numLineages_.fetch_sub(1, std::memory_order_release);
}

bool InvalidatingInodes::contains(InodeNumber ino) const {
  if (numLineages_.load(std::memory_order_acquire) == 0) {
    return false;
  }
  return inodes_.rlock()->count(ino) != 0;
}

folly::Function<void()> InvalidatingInodes::takeForget(InodeNumber ino) {
  if (numLineages_.load(std::memory_order_acquire) == 0) {
    return nullptr;
  }
  auto inodes = inodes_.wlock();
  if (auto it = inodes->find(ino); it != inodes->end()) {
    return std::move(it->second.forget);
  }
  return nullptr;
}

void Nfsd3::runInvalidation(Invalidation& invalidation) {
  const auto& path = invalidation.path;
  const auto mode = invalidation.mode;
  const auto source = invalidation.source;
  const auto& stats = stats_;
  const bool isGc = !invalidation.lineage.empty();
  incrementNfsGcInvalidationCounter(
      stats, source, &NfsStats::nfsInvalidationGcAttempt);
  invalidatingInodes_.add(invalidation.lineage, std::move(invalidation.forget));
  SCOPE_EXIT {
    invalidatingInodes_.remove(invalidation.lineage);
    if (invalidation.done.valid()) {
      // remove() destroyed forget if no SETATTR took it, breaking result.
      auto result = std::move(invalidation.result).getTry();
      if (result.hasValue()) {
        invalidation.done.setValue(result.value());
      } else if (result.hasException<folly::BrokenPromise>()) {
        invalidation.done.setValue(std::nullopt);
      } else {
        invalidation.done.setException(result.exception());
      }
    }
  };
  // Tests hold the chmod here, with the directory registered above, and send
  // the requests the kernel would make for it themselves.
  std::string_view relative = path.view();
  const auto mountPrefix = mountPath_.view();
  relative = relative.size() > mountPrefix.size() &&
          relative.substr(0, mountPrefix.size()) == mountPrefix &&
          relative[mountPrefix.size()] == '/'
      ? relative.substr(mountPrefix.size() + 1)
      : std::string_view{};
  faultInjector_.check("nfsInvalidation", relative);
  XLOGF(DBG9, "Invalidating: {} mode: {}", path.c_str(), mode);
  const auto chmodResult = chmod(path.c_str(), mode);
  const auto error = errno;
  // GC's chmod is answered with a stale handle error on purpose, see
  // invalidateWithQueueLimit, so ESTALE is its success.
  bool staleReply = false;
#ifndef _WIN32
  staleReply = isGc && error == ESTALE;
#endif
  if (chmodResult == 0 || staleReply) {
    incrementNfsGcInvalidationCounter(
        stats, source, &NfsStats::nfsInvalidationGcSuccess);
    XLOGF(DBG9, "Finished invalidating: {}", path.c_str());
    if (chmodResult == 0 && !invalidation.lineage.empty()) {
      // The SETATTR was answered normally, because
      // experimental:nfs-gc-stale-reply is off or because it did not look
      // like GC's own: forget the children now that the chmod succeeded, as
      // GC did before the stale reply. The client keeps its names, so a
      // forgotten child it uses again comes back as a stale file handle.
      if (auto forget =
              invalidatingInodes_.takeForget(invalidation.lineage[0])) {
        forget();
      }
    }
  } else if (error == ENOENT) {
    incrementNfsGcInvalidationCounter(
        stats, source, &NfsStats::nfsInvalidationGcEnoent);
    // ENOENT is expected after removing files.
    XLOGF(DBG9, "Finished invalidating (no longer exists): {}", path.c_str());
  } else if (error == EACCES) {
    incrementNfsGcInvalidationCounter(
        stats, source, &NfsStats::nfsInvalidationGcFailure);
    // Restricted directories can reject the synthetic chmod used to
    // invalidate the NFS client cache.
    XLOGF(DBG9, "Finished invalidating (permission denied): {}", path.c_str());
#ifdef __APPLE__
  } else if (error == EPERM) {
    incrementNfsGcInvalidationCounter(
        stats, source, &NfsStats::nfsInvalidationGcFailure);
    // On macOS, EPERM is a known operational condition rather than a
    // programming error: it typically means TCC denied the synthetic chmod
    // because the daemon's responsible process lacks the
    // SystemPolicyNetworkVolumes grant. When that happens every GC
    // invalidation fails identically, so one line per daemon lifetime
    // carries all the information. On other platforms EPERM stays in the
    // generic DFATAL branch below: there it is a genuine anomaly.
    XLOGF_FIRST_N(
        ERR,
        1,
        "Permission denied invalidating path {} to mode {} using chmod. "
        "This usually means TCC denied SystemPolicyNetworkVolumes "
        "for the daemon's responsible process. Run `eden doctor`, or "
        "restart EdenFS with `eden restart` to recover. Further EPERM "
        "failures will not be logged; see the nfs.invalidation.gc.failure "
        "counter.",
        path,
        mode);
    // Emit the telemetry event on every EPERM failure so the event table
    // carries the true failure count; the log line above stays
    // once-per-daemon to avoid log spam. Per-failure emission is cheap and
    // non-blocking: XplatLogger enqueues into a bounded queue (dropping,
    // never blocking, when full) drained by a background Scribe producer.
    if (edenFsEventsLogger_) {
      edenFsEventsLogger_->logEvent(
          TccInvalidationDenied{error, path.asString()});
    }
#endif
  } else {
    incrementNfsGcInvalidationCounter(
        stats, source, &NfsStats::nfsInvalidationGcFailure);
    // Not a programming error: the path is on the mount, so the chmod can
    // fail for any reason an NFS request can.
    XLOGF_EVERY_MS(
        ERR,
        60'000,
        "Error invalidating path {} to mode {} using chmod: {}",
        path,
        mode,
        folly::errnoStr(error));
  }
}

void Nfsd3::invalidateInodes(
    const std::vector<std::pair<AbsolutePath, mode_t>>& pathsAndModes) {
  for (auto& pathAndMode : pathsAndModes) {
    invalidate(pathAndMode.first, pathAndMode.second);
  }
}

ImmediateFuture<folly::Unit> Nfsd3::completeInvalidations() {
  return invalidationQueue_.flush();
}

folly::coro::now_task<folly::Unit> Nfsd3::co_completeInvalidations() {
  co_await invalidationQueue_.flush().semi();
  co_return folly::unit;
}

std::vector<Nfsd3::OutstandingRequest> Nfsd3::getOutstandingRequests() {
  return requestTracker_.outstanding();
}

TraceDetailedArgumentsHandle Nfsd3::traceDetailedArguments() {
  auto handle =
      std::shared_ptr<void>(nullptr, [&copy = traceDetailedArguments_](void*) {
        copy.fetch_sub(1, std::memory_order_acq_rel);
      });
  traceDetailedArguments_.fetch_add(1, std::memory_order_acq_rel);
  return handle;
}

Nfsd3::~Nfsd3() {
  // TODO(xavierd): wait for the pending requests,
  // Note the socket will already have been torn down, as this is only destroyed
  // when the socket was closed.
}

void Nfsd3::destroy() {
  // Make sure that the Nfsd3 is destroyed in the EventBase that it was
  // created on. This is necessary as the various async sockets cannot be
  // used in multiple threads and can only be manipulated in the EventBase
  // they are attached to.
  server_->getEventBase()->runInEventBaseThreadAlwaysEnqueue(
      [this] { delete this; });
}

folly::SemiFuture<FsStopDataPtr> Nfsd3::getStopFuture() {
  return stopPromise_.getSemiFuture();
}

bool Nfsd3::takeoverStop() {
  XLOG(DBG7, "calling takeover stop on the nfs RpcServer");

  // RpcServer::takeoverStop() must run on the server's EventBase, which is
  // not the caller's: EdenServer::stopMountsForTakeover() runs on the main
  // EventBase. The SemiFuture it returns carries the detached socket, which
  // is delivered through getStopFuture() instead, and may have deferred
  // callbacks that need an executor to run, so it is detached on the
  // server's EventBase rather than dropped.
  server_->getEventBase()->runInEventBaseThread([server = server_] {
    folly::futures::detachOn(server->getEventBase(), server->takeoverStop());
  });
  return true;
}

folly::StringPiece nfsProcName(uint32_t procNumber) {
  XDCHECK(procNumber < kNfs3dHandlers.size())
      << "got invalid NFS procedure: " << procNumber;
  return kNfs3dHandlers[procNumber].name;
}

ProcessAccessLog::AccessType nfsProcAccessType(uint32_t procNumber) {
  return procNumber < kNfs3dHandlers.size()
      ? kNfs3dHandlers[procNumber].accessType
      : ProcessAccessLog::AccessType::FsChannelOther;
}

} // namespace facebook::eden
