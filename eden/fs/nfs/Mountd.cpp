/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/nfs/Mountd.h"

#include <memory>
#include <unordered_map>

#include <folly/Synchronized.h>
#include <folly/Utility.h>
#include <folly/logging/xlog.h>

#include "eden/common/utils/ImmediateFuture.h"
#include "eden/fs/nfs/MountdRpc.h"
#include "eden/fs/nfs/rpc/RpcServer.h"

namespace facebook::eden {

class MountdServerProcessor final : public RpcServerProcessor {
 public:
  MountdServerProcessor() = default;

  MountdServerProcessor(const MountdServerProcessor&) = delete;
  MountdServerProcessor(MountdServerProcessor&&) = delete;
  MountdServerProcessor& operator=(const MountdServerProcessor&) = delete;
  MountdServerProcessor& operator=(MountdServerProcessor&&) = delete;

  ImmediateFuture<folly::Unit> dispatchRpc(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      uint32_t xid,
      uint32_t progNumber,
      uint32_t progVersion,
      uint32_t procNumber,
      const std::optional<authsys_parms>& authSysCreds) override;

  ImmediateFuture<folly::Unit>
  null(folly::io::Cursor deser, folly::io::QueueAppender ser, uint32_t xid);
  ImmediateFuture<folly::Unit>
  mount(folly::io::Cursor deser, folly::io::QueueAppender ser, uint32_t xid);
  ImmediateFuture<folly::Unit>
  dump(folly::io::Cursor deser, folly::io::QueueAppender ser, uint32_t xid);
  ImmediateFuture<folly::Unit>
  umount(folly::io::Cursor deser, folly::io::QueueAppender ser, uint32_t xid);
  ImmediateFuture<folly::Unit> umountAll(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      uint32_t xid);
  ImmediateFuture<folly::Unit>
  exprt(folly::io::Cursor deser, folly::io::QueueAppender ser, uint32_t xid);

  void registerMount(AbsolutePathPiece path, InodeNumber rootIno);
  void unregisterMount(AbsolutePathPiece path) {
    unregisterMountImpl(path, /*enforceRemoval=*/true);
  }
  void tryUnregisterMount(AbsolutePathPiece path) {
    unregisterMountImpl(path, /*enforceRemoval=*/false);
  }
  bool isMountRegistered(AbsolutePathPiece path);

 private:
  folly::Synchronized<std::unordered_map<AbsolutePath, InodeNumber>>
      mountPoints_;
  void unregisterMountImpl(AbsolutePathPiece path, bool enforceRemoval);
};

namespace {

using Handler = ImmediateFuture<folly::Unit> (MountdServerProcessor::*)(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    uint32_t xid);

struct HandlerEntry {
  constexpr HandlerEntry() = default;
  constexpr HandlerEntry(folly::StringPiece n, Handler h)
      : name(n), handler(h) {}

  folly::StringPiece name;
  Handler handler = nullptr;
};

constexpr auto kMountHandlers = [] {
  std::array<HandlerEntry, 6> handlers;
  handlers[folly::to_underlying(mountProcs::null)] = {
      "NULL", &MountdServerProcessor::null};
  handlers[folly::to_underlying(mountProcs::mnt)] = {
      "MNT", &MountdServerProcessor::mount};
  handlers[folly::to_underlying(mountProcs::dump)] = {
      "DUMP", &MountdServerProcessor::dump};
  handlers[folly::to_underlying(mountProcs::umnt)] = {
      "UMOUNT", &MountdServerProcessor::umount};
  handlers[folly::to_underlying(mountProcs::umntAll)] = {
      "UMOUNTALL", &MountdServerProcessor::umountAll};
  handlers[folly::to_underlying(mountProcs::exprt)] = {
      "EXPORT", &MountdServerProcessor::exprt};

  return handlers;
}();

} // namespace

ImmediateFuture<folly::Unit> MountdServerProcessor::null(
    folly::io::Cursor /*deser*/,
    folly::io::QueueAppender ser,
    uint32_t xid) {
  serializeReply(ser, accept_stat::SUCCESS, xid);
  return folly::unit;
}

ImmediateFuture<folly::Unit> MountdServerProcessor::mount(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    uint32_t xid) {
  serializeReply(ser, accept_stat::SUCCESS, xid);

  AbsolutePath path = canonicalPath(XdrTrait<std::string>::deserialize(deser));
  XLOGF(DBG7, "Mounting: {}", path);

  auto mounts = mountPoints_.rlock();
  auto found = mounts->find(path);
  if (found != mounts->end()) {
    XdrTrait<mountstat3>::serialize(ser, mountstat3::MNT3_OK);
    XdrTrait<mountres3_ok>::serialize(
        ser, mountres3_ok{{found->second}, {auth_flavor::AUTH_UNIX}});
  } else {
    XdrTrait<mountstat3>::serialize(ser, mountstat3::MNT3ERR_NOENT);
  }
  return folly::unit;
}

ImmediateFuture<folly::Unit> MountdServerProcessor::dump(
    folly::io::Cursor /*deser*/,
    folly::io::QueueAppender ser,
    uint32_t xid) {
  serializeReply(ser, accept_stat::PROC_UNAVAIL, xid);
  return folly::unit;
}

ImmediateFuture<folly::Unit> MountdServerProcessor::umount(
    folly::io::Cursor /*deser*/,
    folly::io::QueueAppender ser,
    uint32_t xid) {
  // TODO: This needs to be implemented to support umount without the
  // lazy flag.
  serializeReply(ser, accept_stat::PROC_UNAVAIL, xid);
  return folly::unit;
}

ImmediateFuture<folly::Unit> MountdServerProcessor::umountAll(
    folly::io::Cursor /*deser*/,
    folly::io::QueueAppender ser,
    uint32_t xid) {
  serializeReply(ser, accept_stat::PROC_UNAVAIL, xid);
  return folly::unit;
}

ImmediateFuture<folly::Unit> MountdServerProcessor::exprt(
    folly::io::Cursor /*deser*/,
    folly::io::QueueAppender ser,
    uint32_t xid) {
  serializeReply(ser, accept_stat::SUCCESS, xid);
  /*
   * In theory, we're supposed to return a list of exported FS, but since
   * EdenFS is not intended to be exposed as a generic NFS server, properly
   * answering with the list of exported FS isn't necessary. For now we can
   * just pretend that we don't export anything.
   *
   * When using libnfs, this may be called during mount to recursively mount
   * nested NFS mounts.
   */
  XdrTrait<bool>::serialize(ser, false);
  return folly::unit;
}

ImmediateFuture<folly::Unit> MountdServerProcessor::dispatchRpc(
    folly::io::Cursor deser,
    folly::io::QueueAppender ser,
    uint32_t xid,
    uint32_t progNumber,
    uint32_t progVersion,
    uint32_t procNumber,
    const std::optional<authsys_parms>& /*authSysCreds*/) {
  if (progNumber != kMountdProgNumber) {
    serializeReply(ser, accept_stat::PROG_UNAVAIL, xid);
    return folly::unit;
  }

  if (progVersion != kMountdProgVersion) {
    serializeReply(ser, accept_stat::PROG_MISMATCH, xid);
    XdrTrait<mismatch_info>::serialize(
        ser, mismatch_info{kMountdProgVersion, kMountdProgVersion});
    return folly::unit;
  }

  if (procNumber >= kMountHandlers.size()) {
    XLOGF(ERR, "Invalid procedure: {}", procNumber);
    serializeReply(ser, accept_stat::PROC_UNAVAIL, xid);
    return folly::unit;
  }

  auto handlerEntry = kMountHandlers[procNumber];

  XLOG(DBG7, handlerEntry.name);
  return (this->*handlerEntry.handler)(std::move(deser), std::move(ser), xid);
}

void MountdServerProcessor::registerMount(
    AbsolutePathPiece path,
    InodeNumber ino) {
  auto map = mountPoints_.wlock();
  auto [iter, inserted] = map->emplace(path.copy(), ino);
  XCHECK_EQ(inserted, true) << fmt::format(
      ". The mount point {} (ino: {}) is already inserted into the mount map as {}",
      path,
      ino,
      iter->second);
}

void MountdServerProcessor::unregisterMountImpl(
    AbsolutePathPiece path,
    bool enforceRemoval) {
  auto map = mountPoints_.wlock();
  auto numRemoved = map->erase(path.copy());
  if (enforceRemoval) {
    XCHECK_EQ(numRemoved, 1u);
  }
}

bool MountdServerProcessor::isMountRegistered(AbsolutePathPiece path) {
  auto map = mountPoints_.rlock();
  return map->contains(path.copy());
}

Mountd::Mountd(
    folly::EventBase* evb,
    std::shared_ptr<folly::Executor> threadPool,
    const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
    size_t maximumInFlightRequests,
    std::chrono::nanoseconds highNfsRequestsLogInterval)
    : proc_(std::make_shared<MountdServerProcessor>()),
      server_(
          RpcServer::create(
              proc_,
              evb,
              std::move(threadPool),
              edenFsEventsLogger,
              maximumInFlightRequests,
              highNfsRequestsLogInterval)) {}

void Mountd::initialize(folly::SocketAddress addr) {
  server_->initialize(std::move(addr));
}

void Mountd::initialize(folly::File socket) {
  XLOGF(DBG7, "initializing mountd: {}", socket.fd());
  server_->initializeServerSocket(std::move(socket));
}

void Mountd::resumeAccepting() {
  server_->resumeAccepting();
}

void Mountd::registerMount(AbsolutePathPiece path, InodeNumber ino) {
  proc_->registerMount(path, ino);
}

void Mountd::unregisterMount(AbsolutePathPiece path) {
  proc_->unregisterMount(path);
}

void Mountd::tryUnregisterMount(AbsolutePathPiece path) {
  proc_->tryUnregisterMount(path);
}

bool Mountd::isMountRegistered(AbsolutePathPiece path) {
  return proc_->isMountRegistered(path);
}

folly::SocketAddress Mountd::getAddr() const {
  return server_->getAddr();
}

folly::SemiFuture<folly::File> Mountd::takeoverStop() {
  return server_->takeoverStop();
}

} // namespace facebook::eden
