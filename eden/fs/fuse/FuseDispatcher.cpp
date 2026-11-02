/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/fuse/FuseDispatcher.h"

#include <folly/Exception.h>
#include <folly/executors/GlobalExecutor.h>
#include <folly/futures/Future.h>
#include <folly/logging/xlog.h>

#include "eden/common/utils/StatTimes.h"
#include "eden/fs/fuse/FuseDirList.h"
#include "eden/fs/telemetry/EdenStats.h"

using namespace folly;

namespace facebook::eden {

FuseDispatcher::Attr::Attr(const struct stat& st, uint64_t timeout)
    : st(st), timeout_seconds(timeout) {}

fuse_attr_out FuseDispatcher::Attr::asFuseAttr() const {
  // Ensure that we initialize the members to zeroes;
  // This is important on macOS where there are a couple
  // of additional fields (notably `flags`) that influence
  // file accessibility.
  fuse_attr_out result{};

  result.attr.ino = st.st_ino;
  result.attr.size = st.st_size;
  result.attr.blocks = st.st_blocks;
  result.attr.atime = st.st_atime;
  result.attr.atimensec = stAtime(st).tv_nsec;
  result.attr.mtime = st.st_mtime;
  result.attr.mtimensec = stMtime(st).tv_nsec;
  result.attr.ctime = st.st_ctime;
  result.attr.ctimensec = stCtime(st).tv_nsec;
  result.attr.mode = st.st_mode;
  result.attr.nlink = st.st_nlink;
  result.attr.uid = st.st_uid;
  result.attr.gid = st.st_gid;
  result.attr.rdev = st.st_rdev;
  result.attr.blksize = st.st_blksize;

  result.attr_valid_nsec = 0;
  result.attr_valid = timeout_seconds;

  return result;
}

FuseDispatcher::~FuseDispatcher() = default;

FuseDispatcher::FuseDispatcher(EdenStatsPtr stats) : stats_(std::move(stats)) {}

void FuseDispatcher::initConnection(const fuse_init_out& out) {
  connInfo_ = out;
}

void FuseDispatcher::destroy() {}

ImmediateFuture<fuse_entry_out> FuseDispatcher::lookup(
    uint64_t /*requestID*/,
    InodeNumber /*parent*/,
    PathComponentPiece /*name*/,
    const ObjectFetchContextPtr& /*context*/) {
  throwSystemErrorExplicit(ENOENT);
}

void FuseDispatcher::forget(InodeNumber /*ino*/, unsigned long /*nlookup*/) {}

ImmediateFuture<FuseDispatcher::Attr> FuseDispatcher::getattr(
    InodeNumber /*ino*/,
    const ObjectFetchContextPtr& /*context*/) {
  throwSystemErrorExplicit(ENOENT);
}

ImmediateFuture<FuseDispatcher::Attr> FuseDispatcher::setattr(
    InodeNumber /*ino*/,
    const fuse_setattr_in& /*attr*/,
    const ObjectFetchContextPtr& /*context*/
) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<std::string> FuseDispatcher::readlink(
    InodeNumber /*ino*/,
    bool /*kernelCachesReadlink*/,
    const ObjectFetchContextPtr& /*context*/) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<fuse_entry_out> FuseDispatcher::mknod(
    InodeNumber /*parent*/,
    PathComponentPiece /*name*/,
    mode_t /*mode*/,
    dev_t /*rdev*/,
    const ObjectFetchContextPtr& /*context*/) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<fuse_entry_out> FuseDispatcher::mkdir(
    InodeNumber,
    PathComponentPiece,
    mode_t,
    const ObjectFetchContextPtr&) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<folly::Unit> FuseDispatcher::unlink(
    InodeNumber,
    PathComponentPiece,
    const ObjectFetchContextPtr&) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<folly::Unit> FuseDispatcher::rmdir(
    InodeNumber,
    PathComponentPiece,
    const ObjectFetchContextPtr&) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<fuse_entry_out> FuseDispatcher::symlink(
    InodeNumber,
    PathComponentPiece,
    folly::StringPiece,
    const ObjectFetchContextPtr&) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<folly::Unit> FuseDispatcher::rename(
    InodeNumber,
    PathComponentPiece,
    InodeNumber,
    PathComponentPiece,
    const ObjectFetchContextPtr&) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<folly::Unit> FuseDispatcher::rename2(
    InodeNumber,
    PathComponentPiece,
    InodeNumber,
    PathComponentPiece,
    uint32_t,
    const ObjectFetchContextPtr&) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<fuse_entry_out>
FuseDispatcher::link(InodeNumber, InodeNumber, PathComponentPiece) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<uint64_t> FuseDispatcher::open(
    InodeNumber /*ino*/,
    int /*flags*/) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<folly::Unit> FuseDispatcher::release(
    InodeNumber /*ino*/,
    uint64_t /*fh*/) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<uint64_t> FuseDispatcher::opendir(
    InodeNumber /*ino*/,
    int /*flags*/) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<folly::Unit> FuseDispatcher::releasedir(
    InodeNumber /*ino*/,
    uint64_t /*fh*/) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<BufVec> FuseDispatcher::read(
    InodeNumber /*ino*/,
    size_t /*size*/,
    off_t /*off*/,
    const ObjectFetchContextPtr& /*context*/) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<size_t> FuseDispatcher::write(
    InodeNumber /*ino*/,
    StringPiece /*data*/,
    off_t /*off*/,
    const ObjectFetchContextPtr& /*context*/) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<folly::Unit> FuseDispatcher::flush(InodeNumber, uint64_t) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<folly::Unit> FuseDispatcher::fallocate(
    InodeNumber,
    uint64_t,
    uint64_t,
    const ObjectFetchContextPtr&) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<folly::Unit>
FuseDispatcher::fsync(InodeNumber, bool, const ObjectFetchContextPtr&) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<folly::Unit> FuseDispatcher::fsyncdir(InodeNumber, bool) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<FuseDirList> FuseDispatcher::readdir(
    InodeNumber,
    FuseDirList&&,
    off_t,
    uint64_t,
    const ObjectFetchContextPtr&) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<struct fuse_kstatfs> FuseDispatcher::statfs(
    InodeNumber /*ino*/) {
  struct fuse_kstatfs info = {};

  // Suggest a large blocksize to software that looks at that kind of thing
  // bsize will be returned to applications that call pathconf() with
  // _PC_REC_MIN_XFER_SIZE
  info.bsize = getConnInfo().max_readahead;

  // The fragment size is returned as the _PC_REC_XFER_ALIGN and
  // _PC_ALLOC_SIZE_MIN pathconf() settings.
  // 4096 is commonly used by many filesystem types.
  info.frsize = 4096;

  // Ensure that namelen is set to a non-zero value.
  // The value we return here will be visible to programs that call pathconf()
  // with _PC_NAME_MAX.  Returning 0 will confuse programs that try to honor
  // this value.
  info.namelen = 255;

  return info;
}

ImmediateFuture<folly::Unit> FuseDispatcher::setxattr(
    InodeNumber /*ino*/,
    folly::StringPiece /*name*/,
    folly::StringPiece /*value*/,
    int /*flags*/) {
  FUSELL_NOT_IMPL();
}

const int FuseDispatcher::kENOATTR =
#ifndef ENOATTR
    ENODATA // Linux
#else
    ENOATTR
#endif
    ;

ImmediateFuture<std::string> FuseDispatcher::getxattr(
    InodeNumber /*ino*/,
    folly::StringPiece /*name*/,
    const ObjectFetchContextPtr& /*context*/) {
  throwSystemErrorExplicit(kENOATTR);
}

ImmediateFuture<std::vector<std::string>> FuseDispatcher::listxattr(
    InodeNumber /*ino*/,
    const ObjectFetchContextPtr& /*context*/) {
  return std::vector<std::string>();
}

ImmediateFuture<folly::Unit> FuseDispatcher::removexattr(
    InodeNumber /*ino*/,
    folly::StringPiece /*name*/) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<folly::Unit> FuseDispatcher::access(
    InodeNumber /*ino*/,
    int /*mask*/) {
  // Note that if you mount with the "default_permissions" kernel mount option,
  // the kernel will perform all permissions checks for you, and will never
  // invoke access() directly.
  //
  // Implementing access() is only needed when not using the
  // "default_permissions" option.
  FUSELL_NOT_IMPL();
}

ImmediateFuture<fuse_entry_out> FuseDispatcher::create(
    InodeNumber,
    PathComponentPiece,
    mode_t,
    int,
    const ObjectFetchContextPtr&) {
  FUSELL_NOT_IMPL();
}

ImmediateFuture<uint64_t> FuseDispatcher::bmap(
    InodeNumber /*ino*/,
    size_t /*blocksize*/,
    uint64_t /*idx*/) {
  FUSELL_NOT_IMPL();
}

const fuse_init_out& FuseDispatcher::getConnInfo() const {
  return connInfo_;
}

const EdenStatsPtr& FuseDispatcher::getStats() const {
  return stats_;
}

} // namespace facebook::eden

#endif
