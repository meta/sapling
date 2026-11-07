/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/fuse/FuseChannel.h"
#include <boost/cast.hpp>
#include <fmt/core.h>
#include <folly/ScopeGuard.h>
#include <folly/futures/Future.h>
#include <folly/logging/xlog.h>
#include <folly/system/ThreadName.h>
#include <re2/re2.h>
#include <algorithm>
#if EDEN_HAVE_FUSE_IO_URING
#include <sys/utsname.h>
#endif
#include <cerrno>
#include <chrono>
#include <csignal>
#include <exception>
#include <type_traits>

#include "eden/common/utils/Bug.h"
#include "eden/common/utils/IDGen.h"
#include "eden/common/utils/Synchronized.h"
#include "eden/common/utils/SystemError.h"
#include "eden/fs/fuse/DevFuseTransport.h"
#include "eden/fs/fuse/FuseDirList.h"
#include "eden/fs/fuse/FuseDispatcher.h"
#include "eden/fs/fuse/FuseRequestContext.h"
#include "eden/fs/fuse/FuseTransport.h"
#include "eden/fs/fuse/IoUringFuseTransport.h"
#include "eden/fs/privhelper/PrivHelper.h"
#include "eden/fs/telemetry/EdenErrorInfoBuilder.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/telemetry/LogEvent.h"
#include "eden/fs/utils/StaticAssert.h"
#include "eden/fs/utils/Thread.h"

using namespace folly;
using std::string;

namespace facebook::eden {

namespace {

constexpr size_t kMaxInvalidationThreads = 64;

/**
 * For most FUSE requests, the protocol is simple: an optional request
 * parameters struct followed by zero or more null-terminated strings. Provide
 * handy range-checking parsers.
 */
struct FuseArg {
  /* implicit */ FuseArg(ByteRange arg) : range{arg} {}

  /**
   * Reads a trivial struct or primitive of type T from the ByteRange and
   * advances the internal pointer.
   *
   * Throws std::out_of_range if not enough space remaining.
   */
  template <typename T>
  const T& read() {
    static_assert(std::is_trivial_v<T>);
    XCHECK_EQ(0u, reinterpret_cast<uintptr_t>(range.data()) % alignof(T))
        << "unaligned struct data";
    const void* data = range.data();
    // Throws std::out_of_range if too small.
    range.advance(sizeof(T));
    return *static_cast<const T*>(data);
  }

  /**
   * Skips n bytes without interpreting them.
   */
  void skip(size_t n) {
    range.advance(n);
  }

  /**
   * Reads a null-terminated from the ByteRange.
   *
   * Throws std::out_of_range if not enough space remaining.
   */
  folly::StringPiece readz() {
    const char* data = reinterpret_cast<const char*>(range.data());
    size_t length = strnlen(data, range.size());
    if (UNLIKELY(length == range.size())) {
      throw_exception<std::out_of_range>(
          "no null terminator in remaining bytes");
    }
    range.advance(length);
    return StringPiece{data, data + length};
  }

 private:
  folly::ByteRange range;
};

namespace argrender {

using RenderFn = std::string (&)(FuseArg arg);

std::string default_render(FuseArg /*arg*/) {
  return {};
}

std::string single_string_render(FuseArg arg) {
  auto name = arg.readz();
  return name.str();
}

constexpr RenderFn lookup = single_string_render;
constexpr RenderFn forget = default_render;
constexpr RenderFn getattr = default_render;
constexpr RenderFn setattr = default_render;
constexpr RenderFn readlink = default_render;

std::string symlink(FuseArg arg) {
  auto name = arg.readz();
  auto target = arg.readz();
  return fmt::format("name={}, target={}", name, target);
}

std::string mknod(FuseArg arg) {
  auto& in = arg.read<fuse_mknod_in>();
  auto name = arg.readz();

  return fmt::format("{}, mode={:#o}, rdev={}", name, in.mode, in.rdev);
}

std::string mkdir(FuseArg arg) {
  auto& in = arg.read<fuse_mkdir_in>();
  auto name = arg.readz();
  auto mode = in.mode & ~in.umask;
  return fmt::format("{}, mode={:#o}", name, mode);
}

constexpr RenderFn unlink = single_string_render;
constexpr RenderFn rmdir = single_string_render;

std::string rename(FuseArg arg) {
  auto& in = arg.read<fuse_rename_in>();
  auto oldName = arg.readz();
  auto newName = arg.readz();
  return fmt::format("old={}, newdir={}, new={}", oldName, in.newdir, newName);
}

std::string rename2(FuseArg arg) {
  auto& in = arg.read<fuse_rename2_in>();
  auto oldName = arg.readz();
  auto newName = arg.readz();
  return fmt::format(
      "old={}, newdir={}, new={}, flags={:#x}",
      oldName,
      in.newdir,
      newName,
      in.flags);
}

std::string link(FuseArg arg) {
  auto& in = arg.read<fuse_link_in>();
  auto newName = arg.readz();
  return fmt::format("oldParent={}, newName={}", in.oldnodeid, newName);
}

constexpr RenderFn open = default_render;

std::string read(FuseArg arg) {
  auto& in = arg.read<fuse_read_in>();
  return fmt::format("off={}, len={}", in.offset, in.size);
}

std::string write(FuseArg arg) {
  auto& in = arg.read<fuse_write_in>();
  return fmt::format("off={}, len={}", in.offset, in.size);
}

constexpr RenderFn statfs = default_render;
constexpr RenderFn release = default_render;
constexpr RenderFn fsync = default_render;

std::string setxattr(FuseArg arg) {
  // Skip the compat-sized setxattr_in (8 bytes) because EdenFS does not
  // negotiate FUSE_SETXATTR_EXT, so the kernel sends the old 2-field layout.
  arg.skip(FUSE_COMPAT_SETXATTR_IN_SIZE);
  auto name = arg.readz();
  return fmt::format("name={}", name);
}

std::string getxattr(FuseArg arg) {
  auto& in = arg.read<fuse_getxattr_in>();
  (void)in;
  auto name = arg.readz();
  return fmt::format("name={}", name);
}

constexpr RenderFn listxattr = default_render;
constexpr RenderFn removexattr = default_render;
constexpr RenderFn flush = default_render;
constexpr RenderFn opendir = default_render;

std::string readdir(FuseArg arg) {
  auto& in = arg.read<fuse_read_in>();
  return fmt::format("offset={}", in.offset);
}

constexpr RenderFn releasedir = default_render;
constexpr RenderFn fsyncdir = default_render;

std::string access(FuseArg arg) {
  auto& in = arg.read<fuse_access_in>();
  return fmt::format("mask={}", in.mask);
}

std::string create(FuseArg arg) {
  auto& in = arg.read<fuse_create_in>();
  auto name = arg.readz();
  return fmt::format("name={}, mode={:#o}", name, in.mode);
}

constexpr RenderFn bmap = default_render;

std::string batchforget(FuseArg arg) {
  auto& in = arg.read<fuse_batch_forget_in>();
  // TODO: could print some specific inode values here
  return fmt::format("count={}", in.count);
}

std::string fallocate(FuseArg arg) {
  auto& in = arg.read<fuse_fallocate_in>();
  return fmt::format(
      "mode={}, offset={}, length={}", in.mode, in.offset, in.length);
}

} // namespace argrender

// These static asserts exist to make explicit the memory usage of the per-mount
// FUSE TraceBus. TraceBus uses 2 * capacity * sizeof(TraceEvent) memory usage,
// so limit total memory usage to around 4 MB per mount.
static_assert(CheckSize<FuseTraceEvent, 72>());

// This is the minimum size used by libfuse so we use it too!
constexpr size_t MIN_BUFSIZE = 0x21000;

// The buffer a worker reads into must hold the largest request the kernel can
// send, which is a write of max_write bytes plus its headers.
size_t requestBufferSize(size_t maxWrite) {
  return std::max(MIN_BUFSIZE, maxWrite + 0x1000);
}

using Handler = ImmediateFuture<folly::Unit> (FuseChannel::*)(
    FuseRequestContext& request,
    const fuse_in_header& header,
    folly::ByteRange arg);

using FuseArgRenderer = std::string (*)(FuseArg arg);

using AccessType = ProcessAccessLog::AccessType;

struct HandlerEntry {
  constexpr HandlerEntry() = default;
  /*implicit*/ constexpr HandlerEntry(StringPiece n) : name{n} {}
  constexpr HandlerEntry(StringPiece n, AccessType at)
      : name{n}, accessType{at} {}
  constexpr HandlerEntry(
      StringPiece n,
      Handler h,
      FuseArgRenderer r,
      FuseStats::DurationPtr d,
      FuseStats::CounterPtr cS,
      FuseStats::CounterPtr cF,
      AccessType at = AccessType::FsChannelOther,
      FuseStats::CounterPtr dispatchImmediateCounter = nullptr,
      FuseStats::CounterPtr dispatchDeferredCounter = nullptr)
      : name{n},
        handler{h},
        argRenderer{r},
        duration{d},
        countSuccessful{cS},
        countFailure{cF},
        accessType{at},
        dispatchImmediate{dispatchImmediateCounter},
        dispatchDeferred{dispatchDeferredCounter} {}

  std::string getShortName() const {
    if (name.startsWith("FUSE_")) {
      std::string rv;
      rv.reserve(name.size() - 5);
      for (const char* p = name.begin() + 5; p != name.end(); ++p) {
        char c = *p;
        if (c == '_') {
          continue;
        }
        rv.push_back((c >= 'A' && c <= 'Z') ? c - 'A' + 'a' : c);
      }
      return rv;
    } else {
      // We shouldn't hit CUSE ops, so be explicit and return the entire
      // capitalized name.
      return name.str();
    }
  }

  StringPiece name;
  Handler handler = nullptr;
  FuseArgRenderer argRenderer = nullptr;
  FuseStats::DurationPtr duration = nullptr;
  FuseStats::CounterPtr countSuccessful = nullptr;
  FuseStats::CounterPtr countFailure = nullptr;
  AccessType accessType = AccessType::FsChannelOther;
  FuseStats::CounterPtr dispatchImmediate = nullptr;
  FuseStats::CounterPtr dispatchDeferred = nullptr;
};

constexpr auto kFuseHandlers = [] {
  const auto Read = AccessType::FsChannelRead;
  const auto Write = AccessType::FsChannelWrite;

  // Rely on assignment out of bounds to a constexpr array giving a
  // compiler error.
  std::array<HandlerEntry, 64> handlers;
  handlers[FUSE_LOOKUP] = {
      "FUSE_LOOKUP",
      &FuseChannel::fuseLookup,
      &argrender::lookup,
      &FuseStats::lookup,
      &FuseStats::lookupSuccessful,
      &FuseStats::lookupFailure,
      Read,
      &FuseStats::dispatchLookupImmediate,
      &FuseStats::dispatchLookupDeferred};
  handlers[FUSE_FORGET] = {
      "FUSE_FORGET",
      &FuseChannel::fuseForget,
      &argrender::forget,
      &FuseStats::forget,
      &FuseStats::forgetSuccessful,
      &FuseStats::forgetFailure};
  handlers[FUSE_GETATTR] = {
      "FUSE_GETATTR",
      &FuseChannel::fuseGetAttr,
      &argrender::getattr,
      &FuseStats::getattr,
      &FuseStats::getattrSuccessful,
      &FuseStats::getattrFailure,
      Read,
      &FuseStats::dispatchGetattrImmediate,
      &FuseStats::dispatchGetattrDeferred};
  handlers[FUSE_SETATTR] = {
      "FUSE_SETATTR",
      &FuseChannel::fuseSetAttr,
      &argrender::setattr,
      &FuseStats::setattr,
      &FuseStats::setattrSuccessful,
      &FuseStats::setattrFailure,
      Write};
  handlers[FUSE_READLINK] = {
      "FUSE_READLINK",
      &FuseChannel::fuseReadLink,
      &argrender::readlink,
      &FuseStats::readlink,
      &FuseStats::readlinkSuccessful,
      &FuseStats::readlinkFailure,
      Read,
      &FuseStats::dispatchReadlinkImmediate,
      &FuseStats::dispatchReadlinkDeferred};
  handlers[FUSE_SYMLINK] = {
      "FUSE_SYMLINK",
      &FuseChannel::fuseSymlink,
      &argrender::symlink,
      &FuseStats::symlink,
      &FuseStats::symlinkSuccessful,
      &FuseStats::symlinkFailure,
      Write};
  handlers[FUSE_MKNOD] = {
      "FUSE_MKNOD",
      &FuseChannel::fuseMknod,
      &argrender::mknod,
      &FuseStats::mknod,
      &FuseStats::mknodSuccessful,
      &FuseStats::mknodFailure,
      Write};
  handlers[FUSE_MKDIR] = {
      "FUSE_MKDIR",
      &FuseChannel::fuseMkdir,
      &argrender::mkdir,
      &FuseStats::mkdir,
      &FuseStats::mkdirSuccessful,
      &FuseStats::mkdirFailure,
      Write};
  handlers[FUSE_UNLINK] = {
      "FUSE_UNLINK",
      &FuseChannel::fuseUnlink,
      &argrender::unlink,
      &FuseStats::unlink,
      &FuseStats::unlinkSuccessful,
      &FuseStats::unlinkFailure,
      Write};
  handlers[FUSE_RMDIR] = {
      "FUSE_RMDIR",
      &FuseChannel::fuseRmdir,
      &argrender::rmdir,
      &FuseStats::rmdir,
      &FuseStats::rmdirSuccessful,
      &FuseStats::rmdirFailure,
      Write};
  handlers[FUSE_RENAME] = {
      "FUSE_RENAME",
      &FuseChannel::fuseRename,
      &argrender::rename,
      &FuseStats::rename,
      &FuseStats::renameSuccessful,
      &FuseStats::renameFailure,
      Write};
  handlers[FUSE_LINK] = {
      "FUSE_LINK",
      &FuseChannel::fuseLink,
      &argrender::link,
      &FuseStats::link,
      &FuseStats::linkSuccessful,
      &FuseStats::linkFailure,
      Write};
  handlers[FUSE_OPEN] = {
      "FUSE_OPEN",
      &FuseChannel::fuseOpen,
      &argrender::open,
      &FuseStats::open,
      &FuseStats::openSuccessful,
      &FuseStats::openFailure,
      AccessType::FsChannelOther,
      &FuseStats::dispatchOpenImmediate,
      &FuseStats::dispatchOpenDeferred};
  handlers[FUSE_READ] = {
      "FUSE_READ",
      &FuseChannel::fuseRead,
      &argrender::read,
      &FuseStats::read,
      &FuseStats::readSuccessful,
      &FuseStats::readFailure,
      Read,
      &FuseStats::dispatchReadImmediate,
      &FuseStats::dispatchReadDeferred};
  handlers[FUSE_WRITE] = {
      "FUSE_WRITE",
      &FuseChannel::fuseWrite,
      &argrender::write,
      &FuseStats::write,
      &FuseStats::writeSuccessful,
      &FuseStats::writeFailure,
      Write};
  handlers[FUSE_STATFS] = {
      "FUSE_STATFS",
      &FuseChannel::fuseStatFs,
      &argrender::statfs,
      &FuseStats::statfs,
      &FuseStats::statfsSuccessful,
      &FuseStats::statfsFailure,
      Read};
  handlers[FUSE_RELEASE] = {
      "FUSE_RELEASE",
      &FuseChannel::fuseRelease,
      &argrender::release,
      &FuseStats::release,
      &FuseStats::releaseSuccessful,
      &FuseStats::releaseFailure};
  handlers[FUSE_FSYNC] = {
      "FUSE_FSYNC",
      &FuseChannel::fuseFsync,
      &argrender::fsync,
      &FuseStats::fsync,
      &FuseStats::fsyncSuccessful,
      &FuseStats::fsyncFailure,
      Write};
  handlers[FUSE_SETXATTR] = {
      "FUSE_SETXATTR",
      &FuseChannel::fuseSetXAttr,
      &argrender::setxattr,
      &FuseStats::setxattr,
      &FuseStats::setxattrSuccessful,
      &FuseStats::setxattrFailure,
      Write};
  handlers[FUSE_GETXATTR] = {
      "FUSE_GETXATTR",
      &FuseChannel::fuseGetXAttr,
      &argrender::getxattr,
      &FuseStats::getxattr,
      &FuseStats::getxattrSuccessful,
      &FuseStats::getxattrFailure,
      Read,
      &FuseStats::dispatchGetxattrImmediate,
      &FuseStats::dispatchGetxattrDeferred};
  handlers[FUSE_LISTXATTR] = {
      "FUSE_LISTXATTR",
      &FuseChannel::fuseListXAttr,
      &argrender::listxattr,
      &FuseStats::listxattr,
      &FuseStats::listxattrSuccessful,
      &FuseStats::listxattrFailure,
      Read};
  handlers[FUSE_REMOVEXATTR] = {
      "FUSE_REMOVEXATTR",
      &FuseChannel::fuseRemoveXAttr,
      &argrender::removexattr,
      &FuseStats::removexattr,
      &FuseStats::removexattrSuccessful,
      &FuseStats::removexattrFailure,
      Write};
  handlers[FUSE_FLUSH] = {
      "FUSE_FLUSH",
      &FuseChannel::fuseFlush,
      &argrender::flush,
      &FuseStats::flush,
      &FuseStats::flushSuccessful,
      &FuseStats::flushFailure};
  handlers[FUSE_INIT] = {"FUSE_INIT"};
  handlers[FUSE_OPENDIR] = {
      "FUSE_OPENDIR",
      &FuseChannel::fuseOpenDir,
      &argrender::opendir,
      &FuseStats::opendir,
      &FuseStats::opendirSuccessful,
      &FuseStats::opendirFailure};
  handlers[FUSE_READDIR] = {
      "FUSE_READDIR",
      &FuseChannel::fuseReadDir,
      &argrender::readdir,
      &FuseStats::readdir,
      &FuseStats::readdirSuccessful,
      &FuseStats::readdirFailure,
      Read,
      &FuseStats::dispatchReaddirImmediate,
      &FuseStats::dispatchReaddirDeferred};
  handlers[FUSE_RELEASEDIR] = {
      "FUSE_RELEASEDIR",
      &FuseChannel::fuseReleaseDir,
      &argrender::releasedir,
      &FuseStats::releasedir,
      &FuseStats::releasedirSuccessful,
      &FuseStats::releasedirFailure};
  handlers[FUSE_FSYNCDIR] = {
      "FUSE_FSYNCDIR",
      &FuseChannel::fuseFsyncDir,
      &argrender::fsyncdir,
      &FuseStats::fsyncdir,
      &FuseStats::fsyncdirSuccessful,
      &FuseStats::fsyncdirFailure,
      Write};
  handlers[FUSE_GETLK] = {"FUSE_GETLK"};
  handlers[FUSE_SETLK] = {"FUSE_SETLK"};
  handlers[FUSE_SETLKW] = {"FUSE_SETLKW"};
  handlers[FUSE_ACCESS] = {
      "FUSE_ACCESS",
      &FuseChannel::fuseAccess,
      &argrender::access,
      &FuseStats::access,
      &FuseStats::accessSuccessful,
      &FuseStats::accessFailure,
      Read};
  handlers[FUSE_CREATE] = {
      "FUSE_CREATE",
      &FuseChannel::fuseCreate,
      &argrender::create,
      &FuseStats::create,
      &FuseStats::createSuccessful,
      &FuseStats::createFailure,
      Write};
  handlers[FUSE_INTERRUPT] = {"FUSE_INTERRUPT"};
  handlers[FUSE_BMAP] = {
      "FUSE_BMAP",
      &FuseChannel::fuseBmap,
      &argrender::bmap,
      &FuseStats::bmap,
      &FuseStats::bmapSuccessful,
      &FuseStats::bmapFailure};
  handlers[FUSE_DESTROY] = {"FUSE_DESTROY"};
  handlers[FUSE_IOCTL] = {"FUSE_IOCTL"};
  handlers[FUSE_POLL] = {"FUSE_POLL"};
  handlers[FUSE_NOTIFY_REPLY] = {"FUSE_NOTIFY_REPLY"};
  handlers[FUSE_BATCH_FORGET] = {
      "FUSE_BATCH_FORGET",
      &FuseChannel::fuseBatchForget,
      &argrender::batchforget,
      &FuseStats::forgetmulti,
      &FuseStats::forgetmultiSuccessful,
      &FuseStats::forgetmultiFailure};
  handlers[FUSE_FALLOCATE] = {
      "FUSE_FALLOCATE",
      &FuseChannel::fuseFallocate,
      &argrender::fallocate,
      &FuseStats::fallocate,
      &FuseStats::fallocateSuccessful,
      &FuseStats::fallocateFailure,
      Write};
  handlers[FUSE_READDIRPLUS] = {"FUSE_READDIRPLUS", Read};
  handlers[FUSE_RENAME2] = {
      "FUSE_RENAME2",
      &FuseChannel::fuseRename2,
      &argrender::rename2,
      &FuseStats::rename2,
      &FuseStats::rename2Successful,
      &FuseStats::rename2Failure,
      Write};
  handlers[FUSE_LSEEK] = {"FUSE_LSEEK"};
  handlers[FUSE_COPY_FILE_RANGE] = {"FUSE_COPY_FILE_RANGE", Write};
  handlers[FUSE_SETUPMAPPING] = {"FUSE_SETUPMAPPING", Read};
  handlers[FUSE_REMOVEMAPPING] = {"FUSE_REMOVEMAPPING", Read};
  return handlers;
}();

// Separate to avoid bloating the FUSE opcode table; CUSE_INIT is 4096.
constexpr HandlerEntry kCuseInitHandler{"CUSE_INIT"};

constexpr const HandlerEntry* FOLLY_NULLABLE
lookupFuseHandlerEntry(uint32_t opcode) {
  if (CUSE_INIT == opcode) {
    return &kCuseInitHandler;
  }
  if (opcode >= std::size(kFuseHandlers)) {
    return nullptr;
  }
  auto& entry = kFuseHandlers[opcode];
  return entry.name.empty() ? nullptr : &entry;
}

constexpr std::pair<uint64_t, const char*> kCapsLabels[] = {
    {FUSE_ASYNC_READ, "ASYNC_READ"},
    {FUSE_POSIX_LOCKS, "POSIX_LOCKS"},
    {FUSE_ATOMIC_O_TRUNC, "ATOMIC_O_TRUNC"},
    {FUSE_EXPORT_SUPPORT, "EXPORT_SUPPORT"},
    {FUSE_BIG_WRITES, "BIG_WRITES"},
    {FUSE_DONT_MASK, "DONT_MASK"},
    {FUSE_FLOCK_LOCKS, "FLOCK_LOCKS"},
    {FUSE_SPLICE_WRITE, "SPLICE_WRITE"},
    {FUSE_SPLICE_MOVE, "SPLICE_MOVE"},
    {FUSE_SPLICE_READ, "SPLICE_READ"},
    {FUSE_HAS_IOCTL_DIR, "IOCTL_DIR"},
    {FUSE_AUTO_INVAL_DATA, "AUTO_INVAL_DATA"},
    {FUSE_DO_READDIRPLUS, "DO_READDIRPLUS"},
    {FUSE_READDIRPLUS_AUTO, "READDIRPLUS_AUTO"},
    {FUSE_ASYNC_DIO, "ASYNC_DIO"},
    {FUSE_WRITEBACK_CACHE, "WRITEBACK_CACHE"},
    {FUSE_PARALLEL_DIROPS, "PARALLEL_DIROPS"},
    {FUSE_HANDLE_KILLPRIV, "HANDLE_KILLPRIV"},
#ifdef FUSE_HANDLE_KILLPRIV_V2
    {FUSE_HANDLE_KILLPRIV_V2, "HANDLE_KILLPRIV_V2"},
#endif
    {FUSE_POSIX_ACL, "POSIX_ACL"},
    {FUSE_ABORT_ERROR, "ABORT_ERROR"},
    {FUSE_MAX_PAGES, "MAX_PAGES"},
    {FUSE_CACHE_SYMLINKS, "CACHE_SYMLINKS"},
    {FUSE_EXPLICIT_INVAL_DATA, "EXPLICIT_INVAL_DATA"},
#ifdef FUSE_NO_OPEN_SUPPORT
    {FUSE_NO_OPEN_SUPPORT, "NO_OPEN_SUPPORT"},
#endif
#ifdef FUSE_NO_OPENDIR_SUPPORT
    {FUSE_NO_OPENDIR_SUPPORT, "NO_OPENDIR_SUPPORT"},
#endif
#ifdef FUSE_INIT_EXT
    {FUSE_INIT_EXT, "INIT_EXT"},
#endif
#if EDEN_HAVE_FUSE_IO_URING
    {FUSE_OVER_IO_URING, "OVER_IO_URING"},
#endif
#ifdef FUSE_ALLOW_IDMAP
    {FUSE_ALLOW_IDMAP, "ALLOW_IDMAP"},
#endif
};

std::string capsFlagsToLabel(uint64_t flags) {
  std::vector<const char*> bits;
  bits.reserve(std::size(kCapsLabels));
  for (const auto& [flag, name] : kCapsLabels) {
    if (flag == 0) {
      // Sometimes a define evaluates to zero; it's not useful so skip it
      continue;
    }
    if ((flags & flag) == flag) {
      bits.push_back(name);
      flags &= ~flag;
    }
  }
  std::string str;
  folly::join(" ", bits, str);
  if (flags == 0) {
    return str;
  }
  return fmt::format("{} unknown:0x{:x}", str, flags);
}

folly::exception_wrapper makeTakeoverReadinessStopException(
    FuseChannel::StopReason reason) {
  return folly::make_exception_wrapper<std::runtime_error>(fmt::format(
      "FUSE transport stopped before readiness completed: reason={}",
      static_cast<int>(reason)));
}

#if EDEN_HAVE_FUSE_IO_URING
std::string getRunningKernelRelease() {
  struct utsname uts = {};
  if (uname(&uts) != 0) {
    const auto savedErrno = errno;
    XLOGF(
        WARN,
        "failed to read running kernel release while evaluating FUSE io_uring support: errno={}",
        savedErrno);
    return {};
  }
  return uts.release;
}
#endif
uint64_t fuseInitFlags(const fuse_init_out& connInfo) {
  uint64_t flags = connInfo.flags;
#ifdef FUSE_INIT_EXT
  if (flags & FUSE_INIT_EXT) {
    flags |= uint64_t{connInfo.flags2} << 32;
  }
#endif
  return flags;
}

// fuse:use-io-uring config only gates whether we request FUSE_OVER_IO_URING
// during FUSE_INIT. After init, including takeover, the transport must follow
// the capability that was actually negotiated for this FUSE connection.
bool negotiatedIoUringTransport(const fuse_init_out& connInfo) {
#if EDEN_HAVE_FUSE_IO_URING
  return (fuseInitFlags(connInfo) & FUSE_OVER_IO_URING) != 0;
#else
  (void)connInfo;
  return false;
#endif
}

void sigusr2Handler(int /* signum */) {
  // Do nothing.
  // The purpose of this signal is only to interrupt the blocking read() calls
  // in processSession() and readInitPacket()
}

void installSignalHandler() {
  // We use SIGUSR2 to wake up our worker threads when we want to shut down.
  // Install a signal handler for this signal.  The signal handler itself is a
  // no-op, we simply want to use it to interrupt blocking read() calls.
  //
  // We will re-install this handler each time a FuseChannel object is called,
  // but that should be fine.
  //
  // This must be installed using sigaction() rather than signal(), so we can
  // ensure that the SA_RESTART flag is not ste.
  struct sigaction action = {};
  action.sa_handler = sigusr2Handler;
  sigemptyset(&action.sa_mask);
  action.sa_flags = 0; // We intentionally turn off SA_RESTART
  struct sigaction oldAction{};
  folly::checkUnixError(
      sigaction(SIGUSR2, &action, &oldAction), "failed to set SIGUSR2 handler");
}

template <typename T>
iovec make_iovec(const T& t) {
  static_assert(std::is_standard_layout_v<T>);
  static_assert(std::is_trivial_v<T>);
  iovec iov{};
  iov.iov_base = const_cast<T*>(&t);
  iov.iov_len = sizeof(t);
  return iov;
}

} // namespace

StringPiece fuseOpcodeName(uint32_t opcode) {
  auto* entry = lookupFuseHandlerEntry(opcode);
  return entry ? entry->name : "<unknown>";
}

ProcessAccessLog::AccessType fuseOpcodeAccessType(uint32_t opcode) {
  auto* entry = lookupFuseHandlerEntry(opcode);
  return entry ? entry->accessType
               : ProcessAccessLog::AccessType::FsChannelOther;
}

#if EDEN_HAVE_FUSE_IO_URING
bool FuseChannel::isIoUringTransportAvailable() const {
  folly::call_once(ioUringTransportAvailabilityInitFlag_, [this] {
    if (!useIoUring_) {
      return;
    }

    if (ioUringKernelReleaseRegex_.empty()) {
      XLOGF(
          WARN,
          "Not negotiating FUSE io_uring on mount \"{}\": no kernel release regex is configured",
          mountPath_);
      return;
    }

    re2::RE2 regex{ioUringKernelReleaseRegex_};
    if (!regex.ok()) {
      XLOGF(
          WARN,
          "Not negotiating FUSE io_uring on mount \"{}\": kernel release regex \"{}\" is invalid: {}",
          mountPath_,
          ioUringKernelReleaseRegex_,
          regex.error());
      return;
    }

    const auto kernelRelease = getRunningKernelRelease();
    if (!re2::RE2::PartialMatch(kernelRelease, regex)) {
      XLOGF(
          WARN,
          "Not negotiating FUSE io_uring on mount \"{}\": kernel release \"{}\" does not match configured regex \"{}\"",
          mountPath_,
          kernelRelease,
          ioUringKernelReleaseRegex_);
      return;
    }

    const auto setupError = IoUringFuseTransport::getMaybeSetupError(
        ioUringQueueDepth_, getFuseDeviceFd());
    if (setupError.has_value()) {
      XLOGF(
          WARN,
          "Not negotiating FUSE io_uring on mount \"{}\": {}",
          mountPath_,
          *setupError);
      return;
    }

    ioUringTransportAvailable_ = true;
  });

  return ioUringTransportAvailable_;
}
#endif

FuseChannel::DataRange::DataRange(int64_t off, int64_t len)
    : offset(off), length(len) {}

FuseChannel::InvalidationEntry::InvalidationEntry(
    InodeNumber num,
    PathComponentPiece n)
    : type(InvalidationType::DIR_ENTRY), inode(num), name(n) {}

FuseChannel::InvalidationEntry::InvalidationEntry(
    InodeNumber num,
    int64_t offset,
    int64_t length)
    : type(InvalidationType::INODE), inode(num), range(offset, length) {}

FuseChannel::InvalidationEntry::~InvalidationEntry() {
  switch (type) {
    case InvalidationType::INODE:
      range.~DataRange();
      return;
    case InvalidationType::DIR_ENTRY:
      name.~PathComponent();
      return;
  }
  XLOGF(
      FATAL, "unknown InvalidationEntry type: {}", static_cast<uint64_t>(type));
}

FuseChannel::InvalidationEntry::
    InvalidationEntry(InvalidationEntry&& other) noexcept(
        std::is_nothrow_move_constructible_v<PathComponent> &&
        std::is_nothrow_move_constructible_v<DataRange>)
    : type(other.type), inode(other.inode) {
  switch (type) {
    case InvalidationType::INODE:
      new (&range) DataRange(std::move(other.range));
      return;
    case InvalidationType::DIR_ENTRY:
      new (&name) PathComponent(std::move(other.name));
      return;
  }
}

void FuseChannel::replyError(
    const FuseTransport& source,
    const fuse_in_header& request,
    int errorCode) {
  source.replyError(*this, request, errorCode);
}

void FuseChannel::sendReply(
    const FuseTransport& source,
    const fuse_in_header& request,
    folly::fbvector<iovec>&& vec) const {
  fuse_out_header out{};
  out.unique = request.unique;
  out.error = 0;

  vec.insert(vec.begin(), make_iovec(out));

  sendRawReply(source, vec.data(), vec.size());
}

void FuseChannel::sendReply(
    const FuseTransport& source,
    const fuse_in_header& request,
    const folly::IOBuf& buf) const {
  fuse_out_header out{};
  out.unique = request.unique;
  out.error = 0;

  folly::fbvector<iovec> vec;
  vec.reserve(1 + buf.countChainElements());
  vec.push_back(make_iovec(out));
  buf.appendToIov(&vec);

  sendRawReply(source, vec.data(), vec.size());
}

void FuseChannel::sendReply(
    const FuseTransport& source,
    const fuse_in_header& request,
    folly::ByteRange bytes) const {
  fuse_out_header out{};
  out.unique = request.unique;
  out.error = 0;

  std::array<iovec, 2> iov{};
  iov[0].iov_base = &out;
  iov[0].iov_len = sizeof(out);
  iov[1].iov_base = const_cast<uint8_t*>(bytes.data());
  iov[1].iov_len = bytes.size();

  sendRawReply(source, iov.data(), iov.size());
}

void FuseChannel::sendRawReply(
    const FuseTransport& source,
    const iovec iov[],
    size_t count) const {
  source.sendRawReply(const_cast<FuseChannel&>(*this), iov, count);
}

void FuseChannel::sendRawReplyDevFuse(const iovec iov[], size_t count) const {
  // Ensure that the length is set correctly
  XDCHECK_EQ(iov[0].iov_len, sizeof(fuse_out_header));
  const auto header = reinterpret_cast<fuse_out_header*>(iov[0].iov_base);
  header->len = 0;
  for (size_t i = 0; i < count; ++i) {
    header->len += iov[i].iov_len;
  }

  const auto res = writev(fuseDevice_.fd(), iov, count);
  const int err = errno;
  XLOGF(
      DBG7,
      "sendRawReply: unique={} header->len={} wrote={}",
      header->unique,
      header->len,
      res);

  if (res < 0) {
    if (err == ENOENT) {
      // Interrupted by a signal.  We don't need to log this,
      // but will propagate it back to our caller.
    } else if (!isFuseDeviceValid(state_.rlock()->stopReason)) {
      XLOG(INFO, "error writing to fuse device: session closed");
    } else {
      XLOGF(WARNING, "error writing to fuse device: {}", folly::errnoStr(err));
    }
    throwSystemErrorExplicit(err, "error writing to fuse device");
  }
}

FuseChannel::FuseChannel(
    PrivHelper* privHelper,
    folly::File fuseDevice,
    AbsolutePathPiece mountPath,
    std::shared_ptr<folly::Executor> threadPool,
    size_t numThreads,
    std::unique_ptr<FuseDispatcher> dispatcher,
    const folly::Logger* straceLogger,
    std::shared_ptr<ProcessInfoCache> processInfoCache,
    const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
    ErrorLogger& errorLogger,
    folly::Duration requestTimeout,
    std::shared_ptr<Notifier> notifier,
    bool requireUtf8Path,
    int32_t maximumBackgroundRequests,
    size_t maximumInFlightRequests,
    std::chrono::nanoseconds highFuseRequestsLogInterval,
    std::chrono::nanoseconds longRunningFSRequestThreshold,
    bool useWriteBackCache,
    size_t fuseTraceBusCapacity,
    std::optional<uint32_t> fuseBdiReadAheadKb,
    uint32_t fuseMaxPages,
    bool handleKillPrivV2,
    bool useIoUring,
    std::string ioUringKernelReleaseRegex,
    uint32_t ioUringQueueDepth,
    bool ioUringDisableIoWait,
    bool ioUringSkipSelfWakeup,
    bool ioUringPreCreateQueues,
    size_t numInvalidationThreads,
    std::chrono::nanoseconds busyPoll)
    : privHelper_{privHelper},
      // Pre-allocate based on configured max_pages so the buffer can handle
      // the larger requests we'll negotiate during FUSE_INIT. This is
      // optimistic: if the kernel doesn't support FUSE_MAX_PAGES, the buffer
      // will be larger than necessary but still correct.
      bufferSize_(
          requestBufferSize(size_t(fuseMaxPages) * size_t(getpagesize()))),
      threadPool_{std::move(threadPool)},
      configuredWorkerThreadCount_(numThreads),
      numInvalidationThreads_{std::clamp<size_t>(
          numInvalidationThreads,
          1,
          kMaxInvalidationThreads)},
      dispatcher_(std::move(dispatcher)),
      straceLogger_(straceLogger),
      edenFsEventsLogger_(edenFsEventsLogger),
      errorLogger_(errorLogger),
      mountPath_(mountPath),
      requestTimeout_(requestTimeout),
      notifier_(std::move(notifier)),
      requireUtf8Path_{requireUtf8Path},
      maximumBackgroundRequests_{maximumBackgroundRequests},
      maximumInFlightRequests_{maximumInFlightRequests},
      highFuseRequestsLogInterval_{highFuseRequestsLogInterval},
      longRunningFSRequestThreshold_{longRunningFSRequestThreshold},
      useWriteBackCache_{useWriteBackCache},
      fuseBdiReadAheadKb_{fuseBdiReadAheadKb},
      fuseMaxPages_{fuseMaxPages},
      handleKillPrivV2_{handleKillPrivV2},
      useIoUring_{useIoUring},
      ioUringKernelReleaseRegex_{std::move(ioUringKernelReleaseRegex)},
      ioUringQueueDepth_{ioUringQueueDepth},
      ioUringDisableIoWait_{ioUringDisableIoWait},
      ioUringSkipSelfWakeup_{ioUringSkipSelfWakeup},
      ioUringPreCreateQueues_{ioUringPreCreateQueues},
      busyPoll_{busyPoll},
      fuseDevice_(std::move(fuseDevice)),
      transport_(std::make_unique<DevFuseTransport>()),
      transportName_{transport_->getName()},
      invalidationQueue_{
          numInvalidationThreads_,
          [this](InvalidationEntry& entry) { sendInvalidation(entry); },
          fmt::format("inval{}", mountPath.basename())},
      processAccessLog_(
          std::make_shared<ProcessAccessLog>(std::move(processInfoCache))),
      traceDetailedArguments_(std::make_shared<std::atomic<size_t>>(0)),
      traceBus_(
          TraceBus<FuseTraceEvent>::create(
              "FuseTrace" + mountPath.asString(),
              fuseTraceBusCapacity)) {
  if (numInvalidationThreads == 0) {
    XLOGF(
        WARN,
        "fuse:num-invalidation-threads is 0 for {}; using 1 instead",
        mountPath_);
  } else if (numInvalidationThreads > kMaxInvalidationThreads) {
    XLOGF(
        WARN,
        "fuse:num-invalidation-threads is {} for {}; using {} instead",
        numInvalidationThreads,
        mountPath_,
        kMaxInvalidationThreads);
  }
  XLOGF(
      INFO,
      "Creating FuseChannel: mountPath={}, numThreads={}, requireUtf8={}, maximumBackgroundRequests={}, maximumInFlightRequests={}, useWriteBackCache={}, fuseMaxPages={}",
      mountPath,
      numThreads,
      requireUtf8Path,
      maximumBackgroundRequests,
      maximumInFlightRequests,
      useWriteBackCache,
      fuseMaxPages_);
  XCHECK_GE(configuredWorkerThreadCount_, 1ul);
  XCHECK_GE(numInvalidationThreads_, 1ul);
  updateEffectiveWorkerThreadCount();
  installSignalHandler();

  initializeInflightRequestsRateLimiter(maximumInFlightRequests);
}

FsStopReason FuseChannel::StopData::getStopReason() const {
  switch (reason) {
    case StopReason::RUNNING:
      return {"running", false};
    case StopReason::INIT_FAILED:
      return {"fuse_init_failed", false};
    case StopReason::UNMOUNTED:
      return {"fuse_unmounted", true};
    case StopReason::TAKEOVER:
      return {"takeover", false};
    case StopReason::DESTRUCTOR:
      return {"fuse_destructor", false};
    case StopReason::FUSE_READ_ERROR:
      return {"fuse_read_error", true};
    case StopReason::FUSE_WRITE_ERROR:
      return {"fuse_write_error", true};
    case StopReason::FUSE_TRUNCATED_REQUEST:
      return {"fuse_truncated_request", true};
    case StopReason::WORKER_EXCEPTION:
      return {"worker_exception", true};
  }
  return {"unknown", false};
}

FuseChannel::~FuseChannel() {
  XCHECK_EQ(1, traceBus_.use_count())
      << "This shared_ptr should not be copied; see attached comment.";
}

const char* FuseChannel::getTransportName() const {
  return transportName_.load(std::memory_order_relaxed);
}

const char* FuseChannel::getDesiredTransportName() const {
#if EDEN_HAVE_FUSE_IO_URING
  if (isIoUringTransportAvailable()) {
    return kIoUringFuseTransportName;
  }
#endif
  return kDevFuseTransportName;
}

void FuseChannel::logTakeoverTransportMismatch(
    const fuse_init_out& connInfo) const {
  const char* inheritedTransport = negotiatedIoUringTransport(connInfo)
      ? kIoUringFuseTransportName
      : kDevFuseTransportName;
  const char* desiredTransport = getDesiredTransportName();
  if (folly::StringPiece{inheritedTransport} == desiredTransport) {
    return;
  }

  XLOGF(
      WARN,
      "FUSE transport config changed for mount \"{}\": configured transport is {}, but the existing FUSE connection negotiated {}. Graceful restart will preserve the existing {} transport; run a full EdenFS restart to apply the new transport config.",
      mountPath_,
      desiredTransport,
      inheritedTransport,
      inheritedTransport);
}

void FuseChannel::dispatchRequestFromTransport(
    const FuseTransport& source,
    const fuse_in_header& header,
    folly::ByteRange arg,
    pid_t myPid) {
  dispatchRequest(source, header, arg, myPid);
}

void FuseChannel::requestSessionExitFromTransport(StopReason reason) {
  requestSessionExit(reason);
}

void FuseChannel::notifyTransportWorkerReady(
    size_t queueId,
    size_t expectedWorkerCount) {
  if (!takeoverReadinessStarted_.load(std::memory_order_acquire)) {
    return;
  }

  const auto readyCount =
      takeoverReadyWorkerCount_.fetch_add(1, std::memory_order_acq_rel) + 1;
  XLOGF(
      DBG6,
      "FUSE transport worker ready mount={} transport={} queueId={} readyWorkers={} expectedWorkers={}",
      mountPath_,
      transport_->getName(),
      queueId,
      readyCount,
      expectedWorkerCount);

  if (readyCount == expectedWorkerCount) {
    fulfillTakeoverReadiness();
  } else if (readyCount > expectedWorkerCount) {
    XLOGF(
        WARN,
        "FUSE transport reported more ready workers than expected mount={} transport={} readyWorkers={} expectedWorkers={}",
        mountPath_,
        transport_->getName(),
        readyCount,
        expectedWorkerCount);
  }
}

void FuseChannel::logUnmountEventAndExit() {
  folly::call_once(unmountLogFlag_, [this] {
    XLOGF(DBG3, "received unmount event ENODEV on mount {}", mountPath_);
  });
  requestSessionExit(StopReason::UNMOUNTED);
}

Future<FuseChannel::StopFuture> FuseChannel::initialize() {
  // Start one worker thread which will perform the initialization,
  // and will then start the remaining worker threads and signal success
  // once initialization completes.
  return folly::makeFutureWith([&] {
    auto state = state_.wlock();
    state->workerThreads.reserve(configuredWorkerThreadCount_);
    state->workerThreads.emplace_back([this] { initWorkerThread(); });
    return initPromise_.getFuture();
  });
}

void FuseChannel::maybeSetFuseReadAhead() {
  if (fuseBdiReadAheadKb_.has_value()) {
    privHelper_->setFuseReadAhead(mountPath_.view(), *fuseBdiReadAheadKb_)
        .thenError([this](folly::exception_wrapper&& ew) {
          XLOGF(
              WARN,
              "Failed to set FUSE read-ahead for {}: {}",
              mountPath_,
              ew.what());
        });
  }
}

FuseChannel::StopFuture FuseChannel::initializeFromTakeover(
    fuse_init_out connInfo) {
  takeoverReadinessStarted_.store(true, std::memory_order_release);
  connInfo_ = connInfo;
  // The kernel may send requests as large as the max_write it negotiated with
  // the process we took over from. fuse:max-pages has no say over an
  // established connection, so a buffer sized from it would be too small to
  // read those requests whenever the two disagree.
  bufferSize_ = requestBufferSize(connInfo.max_write);
  if (negotiatedIoUringTransport(connInfo)) {
    // TODO: fuse:io-uring-pre-create-queues is deliberately not honored here.
    // io_uring was already negotiated by the process we took over from, so
    // there is no INIT reply left to withhold and nothing to fall back to: if
    // the queues do not fit, this mount is stuck either way. Recovering that
    // case needs its own mechanism (fail the takeover and let the old process
    // keep the mount), which is worth building only once the pre-create path
    // has proven itself on fresh mounts.
    transport_ = std::make_unique<IoUringFuseTransport>(
        ioUringQueueDepth_, ioUringDisableIoWait_, ioUringSkipSelfWakeup_);
    transportName_.store(transport_->getName(), std::memory_order_relaxed);
  }
  updateEffectiveWorkerThreadCount();
  dispatcher_->initConnection(connInfo);

  XLOGF(
      DBG1,
      "Takeover using transport={}, max_write={}, max_readahead={}, want={}",
      transport_->getName(),
      connInfo_->max_write,
      connInfo_->max_readahead,
      capsFlagsToLabel(fuseInitFlags(*connInfo_)));
  startWorkerThreads();
  if (!usesIoUringTransport()) {
    fulfillTakeoverReadiness();
  }
  return sessionCompletePromise_.getFuture();
}

folly::Future<folly::Unit> FuseChannel::takeoverReadyFuture() {
  return takeoverReadyPromise_.getFuture();
}

folly::SemiFuture<folly::Unit> FuseChannel::unmount(UnmountOptions options) {
  // TODO: This does not handle the situation where the mount has been moved by,
  // for example, renaming a parent directory, or `mount --move`.
  return privHelper_->fuseUnmount(mountPath_.view(), options)
      .thenTry([this](Try<Unit>&& result) {
        requestSessionExit(StopReason::UNMOUNTED);
        return std::move(result);
      });
}

void FuseChannel::startWorkerThreads() {
  auto state = state_.wlock();

  // After acquiring the state_ lock check to see if we have been asked to shut
  // down.  If so just return without doing anything.
  //
  // This can happen if the FuseChannel is destroyed very shortly after we
  // finish processing the INIT request.  In this case we don't want to start
  // the remaining worker threads if the destructor is trying to stop and join
  // them.
  if (state->stopReason != StopReason::RUNNING) {
    return;
  }

  try {
    state->workerThreads.reserve(effectiveWorkerThreadCount_);
    while (state->workerThreads.size() < effectiveWorkerThreadCount_) {
      state->workerThreads.emplace_back([this] { fuseWorkerThread(); });
    }

    invalidationQueue_.start();
  } catch (const std::exception& ex) {
    XLOGF(ERR, "Error starting FUSE worker threads: {}", exceptionStr(ex));
    // Request any threads we did start to stop now.
    const auto shouldFailTakeoverReadiness =
        requestSessionExitLocked(state, StopReason::INIT_FAILED);
    state.unlock();
    if (shouldFailTakeoverReadiness) {
      failTakeoverReadiness(
          makeTakeoverReadinessStopException(StopReason::INIT_FAILED));
    }
    stopInvalidationThread();
    throw;
  }
}

void FuseChannel::destroy() {
  std::vector<std::thread> threads;
  bool shouldFailTakeoverReadiness = false;
  {
    auto state = state_.wlock();
    shouldFailTakeoverReadiness =
        requestSessionExitLocked(state, StopReason::DESTRUCTOR);
    threads.swap(state->workerThreads);
  }
  if (shouldFailTakeoverReadiness) {
    failTakeoverReadiness(
        makeTakeoverReadinessStopException(StopReason::DESTRUCTOR));
  }

  for (auto& thread : threads) {
    if (std::this_thread::get_id() == thread.get_id()) {
      XLOG(
          FATAL,
          "cannot destroy a FuseChannel from inside one of its own worker threads");
    }
    thread.join();
  }

  // Check to see if there are still outstanding requests.
  // If so, delay actual deletion of the FuseChannel object until the
  // last request completes.
  bool allDone = false;
  {
    auto state = state_.wlock();
    if (state->pendingRequests == 0) {
      allDone = true;
    } else {
      state->destroyPending = true;
    }
  }
  if (allDone) {
    delete this;
  }
}

void FuseChannel::invalidateInode(InodeNumber ino, off_t off, off_t len) {
  invalidationQueue_.add(InvalidationEntry(ino, off, len));
}

void FuseChannel::invalidateEntry(InodeNumber parent, PathComponentPiece name) {
  invalidationQueue_.add(InvalidationEntry(parent, name));
}

bool FuseChannel::invalidateEntryWithQueueLimit(
    InodeNumber parent,
    PathComponentPiece name,
    size_t maxQueueSize,
    const folly::CancellationToken& cancellationToken) {
  bool waited = false;
  const bool added = invalidationQueue_.addWithLimit(
      InvalidationEntry(parent, name),
      maxQueueSize,
      cancellationToken,
      &waited);
  if (waited) {
    getStats()->increment(&FuseStats::invalidationQueueThrottleWait);
  }
  return added;
}

void FuseChannel::invalidateInodes(folly::Range<InodeNumber*> range) {
  invalidationQueue_.addAll(range.begin(), range.end());
}

ImmediateFuture<folly::Unit> FuseChannel::completeInvalidations() {
  return invalidationQueue_.flush();
}

folly::coro::now_task<folly::Unit> FuseChannel::co_completeInvalidations() {
  co_await invalidationQueue_.flush().semi();
  co_return folly::unit;
}

/**
 * Send an element from the invalidation queue.
 *
 * This method always runs in the invalidation thread.
 */
void FuseChannel::sendInvalidation(InvalidationEntry& entry) {
  // We catch any exceptions that occur and simply log an error message.
  // There is not much else we can do in this situation.
  XLOGF(DBG6, "sending invalidation request: {}", entry);
  try {
    switch (entry.type) {
      case InvalidationType::INODE:
        sendInvalidateInode(
            entry.inode, entry.range.offset, entry.range.length);
        return;
      case InvalidationType::DIR_ENTRY:
        sendInvalidateEntry(entry.inode, entry.name);
        return;
    }
    EDEN_BUG() << "unknown invalidation entry type "
               << static_cast<uint64_t>(entry.type);
  } catch (const std::system_error& ex) {
    // Log ENOENT errors as a debug message.  This can happen for inode numbers
    // that we allocated on our own and haven't actually told the kernel about
    // yet.
    if (isEnoent(ex)) {
      XLOGF(
          DBG3, "received ENOENT when sending invalidation request: {}", entry);
    } else {
      XLOGF(
          ERR,
          "error sending invalidation request: {}:{}",
          entry,
          folly::exceptionStr(ex));
      errorLogger_.log(
          EdenErrorInfo::fuse(ex, entry.inode.get(), mountPath_.asString()));
    }
  } catch (const std::exception& ex) {
    XLOGF(
        ERR,
        "error sending invalidation request: {}:{}",
        entry,
        folly::exceptionStr(ex));
    errorLogger_.log(
        EdenErrorInfo::fuse(ex, entry.inode.get(), mountPath_.asString()));
  }
}

/**
 * Send a FUSE_NOTIFY_INVAL_INODE message to the kernel.
 *
 * This method always runs in the invalidation thread.
 */
void FuseChannel::sendInvalidateInode(
    InodeNumber ino,
    int64_t off,
    int64_t len) {
  XLOGF(DBG3, "sendInvalidateInode(ino={}, off={}, len={})", ino, off, len);
  fuse_notify_inval_inode_out notify{};
  notify.ino = ino.get();
  notify.off = off;
  notify.len = len;

  fuse_out_header out{};
  out.unique = 0;
  out.error = FUSE_NOTIFY_INVAL_INODE;

  std::array<iovec, 2> iov{};

  iov[0].iov_base = &out;
  iov[0].iov_len = sizeof(out);

  iov[1].iov_base = &notify;
  iov[1].iov_len = sizeof(notify);

  try {
    // Invalidation are always send over devfuse (not io_uring). Then we don't
    // use transport to send the requests.
    sendRawReplyDevFuse(iov.data(), iov.size());
    XLOGF(
        DBG7, "sendInvalidateInode(ino={}, off={}, len={}) OK!", ino, off, len);
  } catch (const std::system_error& exc) {
    // Ignore ENOENT.  This can happen for inode numbers that we allocated on
    // our own and haven't actually told the kernel about yet.
    if (!isEnoent(exc)) {
      XLOGF(
          ERR,
          "sendInvalidateInode(ino={}, off={}, len={}) failed: {}",
          ino,
          off,
          len,
          exc.what());
      throwSystemErrorExplicit(
          exc.code().value(),
          fmt::format("error invalidating FUSE inode {}", ino));
    } else {
      XLOGF(
          DBG6,
          "sendInvalidateInode(ino={}, off={}, len={}) failed with ENOENT",
          ino,
          off,
          len);
    }
  }
}

/**
 * Send a FUSE_NOTIFY_INVAL_ENTRY message to the kernel.
 *
 * This method always runs in the invalidation thread.
 */
void FuseChannel::sendInvalidateEntry(
    InodeNumber parent,
    PathComponentPiece name) {
  XLOGF(DBG3, "sendInvalidateEntry(parent={}, name={})", parent, name);

  auto namePiece = name.view();

  fuse_notify_inval_entry_out notify = {};
  notify.parent = parent.get();
  notify.namelen = namePiece.size();

  fuse_out_header out{};
  out.unique = 0;
  out.error = FUSE_NOTIFY_INVAL_ENTRY;

  std::array<iovec, 4> iov{};

  iov[0].iov_base = &out;
  iov[0].iov_len = sizeof(out);

  iov[1].iov_base = &notify;
  iov[1].iov_len = sizeof(notify);

  iov[2].iov_base = const_cast<char*>(namePiece.data());
  iov[2].iov_len = namePiece.size();

  // libfuse adds an extra 1 count to the size that it sends to the kernel,
  // presumably because it is assuming that the string is already NUL
  // terminated.  That is misleading because the API provides a size parameter
  // that implies that the string doesn't require termination.  We deal with
  // this more safely here by adding a vec element holding a NUL byte.
  iov[3].iov_base = const_cast<char*>("\x00");
  iov[3].iov_len = 1;

  try {
    // Invalidation are always send over devfuse (not io_uring). Then we don't
    // use transport to send the requests.
    sendRawReplyDevFuse(iov.data(), iov.size());
  } catch (const std::system_error& exc) {
    // Ignore ENOENT.  This can happen for inode numbers that we allocated on
    // our own and haven't actually told the kernel about yet.
    if (!isEnoent(exc)) {
      throwSystemErrorExplicit(
          exc.code().value(),
          fmt::format(
              "error invalidating FUSE entry {} in directory inode {}",
              namePiece,
              parent));
    } else {
      XLOGF(
          DBG3,
          "sendInvalidateEntry(parent={}, name={}) failed with ENOENT",
          parent,
          namePiece);
    }
  }
}

std::vector<FuseChannel::OutstandingRequest>
FuseChannel::getOutstandingRequests() {
  std::vector<FuseChannel::OutstandingRequest> outstandingCalls;

  for (auto& stripe : outstandingRequests_) {
    auto state = stripe.rlock();
    for (const auto& entry : state->requests) {
      outstandingCalls.push_back(entry.second);
    }
  }
  return outstandingCalls;
}

void FuseChannel::recordRequestStart(
    uint64_t requestId,
    const fuse_in_header& header,
    std::chrono::steady_clock::time_point startTime) {
  auto state =
      outstandingRequests_[requestId % kOutstandingRequestStripes].wlock();
  auto [iter, inserted] = state->requests.emplace(
      requestId,
      OutstandingRequest{
          requestId, FuseTraceEvent::RequestHeader{header}, startTime});
  XDCHECK(inserted) << "duplicate fuse request id " << requestId;
}

void FuseChannel::recordRequestFinish(
    uint64_t requestId,
    const fuse_in_header& header,
    std::optional<int64_t> result) {
  const auto now = std::chrono::steady_clock::now();
  std::optional<std::chrono::nanoseconds> duration;
  {
    auto state =
        outstandingRequests_[requestId % kOutstandingRequestStripes].wlock();
    auto iter = state->requests.find(requestId);
    if (iter != state->requests.end()) {
      duration = now - iter->second.requestStartTime;
      state->requests.erase(iter);
    }
  }
  // A finish with no recorded start is a double finish or an id that was
  // never started; a zero-length sample would only hide that.
  XDCHECK(duration.has_value())
      << "FUSE request " << requestId << " finished without a start";
  // A subscriber may have attached after this request started and learned
  // of it through debugOutstandingFuseCalls, as `eden trace fs` does.
  if (traceBus_->hasSubscription()) {
    traceBus_->publish(FuseTraceEvent::finish(requestId, header, result));
  }
}

void FuseChannel::finishRequest(
    uint64_t requestId,
    const fuse_in_header& header,
    const std::optional<int64_t>& result) {
  recordRequestFinish(requestId, header, result);

  // We may be complete; check to see if all requests are
  // done and whether there are any threads remaining.
  auto state = state_.wlock();
  XCHECK_NE(state->pendingRequests, 0u) << "pendingRequests double decrement";
  if (--state->pendingRequests == 0) {
    // If workers are still running, wake transport-specific waits
    // so they can observe that shutdown has fully drained. If they
    // already stopped, completing the final request completes the
    // session immediately.
    if (state->stopReason != StopReason::RUNNING &&
        state->stoppedThreads != effectiveWorkerThreadCount_) {
      requestTransportStopWakeup();
    }
    if (state->stoppedThreads == effectiveWorkerThreadCount_) {
      sessionComplete(std::move(state));
    }
  }
}

TraceDetailedArgumentsHandle FuseChannel::traceDetailedArguments() const {
  // We could implement something fancier here that just copies the shared_ptr
  // into a handle struct that increments upon taking ownership and decrements
  // on destruction, but this code path is quite rare, so do the expedient
  // thing.
  auto handle =
      std::shared_ptr<void>(nullptr, [copy = traceDetailedArguments_](void*) {
        copy->fetch_sub(1, std::memory_order_acq_rel);
      });
  traceDetailedArguments_->fetch_add(1, std::memory_order_acq_rel);
  return handle;
}

void FuseChannel::requestSessionExit(StopReason reason) {
  const auto shouldFailTakeoverReadiness = [&] {
    auto state = state_.wlock();
    return requestSessionExitLocked(state, reason);
  }();
  if (shouldFailTakeoverReadiness) {
    failTakeoverReadiness(makeTakeoverReadinessStopException(reason));
  }
}

void FuseChannel::requestTransportStopWakeup() {
  try {
    transport_->requestStopWakeup();
  } catch (const std::exception& ex) {
    XLOGF(
        WARN,
        "failed to wake FUSE transport {} during shutdown for {}: {}",
        transport_->getName(),
        mountPath_,
        exceptionStr(ex));
  }
}

bool FuseChannel::requestSessionExitLocked(
    const Synchronized<State>::LockedPtr& state,
    StopReason reason) {
  // We have already been asked to stop before.
  if (state->stopReason != StopReason::RUNNING) {
    // Update state->stopReason only if the old stop reason left the FUSE
    // device in a still usable state but the new reason does not.
    if (isFuseDeviceValid(state->stopReason) && !isFuseDeviceValid(reason)) {
      state->stopReason = reason;
    }
    return false;
  }

  // This was the first time requestSessionExit has been called.
  // Record the reason we are stopping and then notify worker threads to
  // stop.
  state->stopReason = reason;
  state->stopRequestedAt = std::chrono::steady_clock::now();

  // Update stop_ so that worker threads will break out of their loop.
  stop_.store(true, std::memory_order_relaxed);

  requestTransportStopWakeup();

  // Send a signal to knock our workers out of their blocking read() syscalls
  // TODO: This code is slightly racy, since threads could receive the signal
  // immediately before entering read().  In the long run it would be nicer to
  // have the worker threads use epoll and then use an eventfd to signal them
  // to stop.
  for (auto& thr : state->workerThreads) {
    if (thr.joinable() && thr.get_id() != std::this_thread::get_id()) {
      pthread_kill(thr.native_handle(), SIGUSR2);
    }
  }
  return true;
}

void FuseChannel::setThreadSigmask() {
  // Make sure our thread will receive SIGUSR2
  sigset_t sigset;
  sigemptyset(&sigset);
  sigaddset(&sigset, SIGUSR2);

  sigset_t oldset;
  sigemptyset(&oldset);

  folly::checkPosixError(pthread_sigmask(SIG_UNBLOCK, &sigset, &oldset));
}

void FuseChannel::initWorkerThread() noexcept {
  try {
    setThreadSigmask();
    setThreadName(fmt::format("fuse{}", mountPath_.basename()));
    XLOGF(
        DBG6,
        "Fuse init worker starting for mount={} transport={} useIoUringConfig={} ioUringPreCreateQueuesConfig={}",
        mountPath_,
        transport_->getName(),
        useIoUring_,
        ioUringPreCreateQueues_);

    // Read the INIT packet
    readInitPacket();
    XLOGF(
        DBG6,
        "Fuse init handshake completed for mount={} negotiated transport={}",
        mountPath_,
        transport_->getName());

    // Start the other FUSE worker threads.
    startWorkerThreads();
    XLOGF(
        DBG6,
        "Fuse worker threads started for mount={} threadCount={} transport={}",
        mountPath_,
        effectiveWorkerThreadCount_,
        transport_->getName());
  } catch (...) {
    auto ew = folly::exception_wrapper(std::current_exception());
    XLOGF(ERR, "Error performing FUSE channel initialization: {}", ew);
    ew.with_exception([&](const std::exception& ex) {
      errorLogger_.log(
          EdenErrorInfo::fuse(ex, std::nullopt, mountPath_.asString()));
    });
    // Indicate that initialization failed.
    initPromise_.setException(std::move(ew));
    return;
  }

  // Signal that initialization is complete.
  initPromise_.setValue(sessionCompletePromise_.getSemiFuture());

  // Continue to run like a normal FUSE worker thread.
  fuseWorkerThread();
}

void FuseChannel::fuseWorkerThread() noexcept {
  disablePthreadCancellation();
  setThreadName(fmt::format("fuse{}", mountPath_.basename()));
  setThreadSigmask();
  (void)liveRequestWatches_.get();

  try {
    processSession();
  } catch (const std::exception& ex) {
    XLOGF(ERR, "unexpected error in FUSE worker thread: {}", exceptionStr(ex));
    errorLogger_.log(
        EdenErrorInfo::fuse(ex, std::nullopt, mountPath_.asString()));
    failTakeoverReadiness(folly::exception_wrapper(std::current_exception()));
    // Request that all other FUSE threads exit.
    // This will cause us to stop processing the mount and signal our session
    // complete future.
    requestSessionExit(StopReason::WORKER_EXCEPTION);
    // Fall through and continue with the normal thread exit code.
  }

  // Record that we have shut down.
  {
    auto state = state_.wlock();
    ++state->stoppedThreads;
    XDCHECK(!state->destroyPending) << "destroyPending cannot be set while "
                                       "worker threads are still running";

    // If we are the last thread to stop and there are no more requests
    // outstanding then invoke sessionComplete().  If we are the last thread
    // but there are still outstanding requests we will invoke
    // sessionComplete() when we process the final stage of the request
    // processing for the last request.
    if (state->stoppedThreads == effectiveWorkerThreadCount_ &&
        state->pendingRequests == 0) {
      sessionComplete(std::move(state));
    }
  }
}

void FuseChannel::stopInvalidationThread() {
  invalidationQueue_.stop();
}

void FuseChannel::readInitPacket() {
  struct {
    fuse_in_header header;
    fuse_init_in init;
    // Starting in kernel 5.4 in
    // https://github.com/torvalds/linux/commit/1fb027d7596464d3fad3ed59f70f43807ef926c6
    // we have to request at least 8KB even for the init request
    char padding_[FUSE_MIN_READ_BUFFER];
  } init = {};

  // Loop until we receive the INIT packet, or until we are stopped.
  while (true) {
    if (stop_.load(std::memory_order_relaxed)) {
      throw_<std::runtime_error>(
          "FuseChannel for \"",
          mountPath_,
          "\" stopped while waiting for INIT packet");
    }

    auto res =
        transport_->readInitPacket(fuseDevice_.fd(), &init, sizeof(init));
    if (res < 0) {
      int errnum = errno;
      if (stop_.load(std::memory_order_relaxed)) {
        throw_<std::runtime_error>(
            "FuseChannel for \"",
            mountPath_,
            "\" stopped while waiting for INIT packet");
      }

      if (errnum == EINTR || errnum == EAGAIN || errnum == ENOENT) {
        // These are all variations on being interrupted; let's
        // continue and retry.
        continue;
      }
      if (errnum == ENODEV) {
        throw FuseDeviceUnmountedDuringInitialization(mountPath_);
      }
      throw_<std::runtime_error>(
          "error reading from FUSE device for \"",
          mountPath_,
          "\" while expecting INIT request: ",
          folly::errnoStr(errnum));
    }
    if (res == 0) {
      // This is generally caused by the unit tests closing a fake fuse
      // channel.  When we are actually connected to the kernel we normally
      // expect to see an ENODEV error rather than EOF.
      throw FuseDeviceUnmountedDuringInitialization(mountPath_);
    }

    // Error out if the kernel sends less data than the minimum INIT packet.
    // On Linux, use the compat size (16 bytes) for the original fuse_init_in
    // before flags2 was added. Newer kernels may send a larger fuse_init_in
    // with flags2, which we accept but don't require.
    if (static_cast<size_t>(res) <
        sizeof(init.header) + FUSE_COMPAT_INIT_IN_SIZE) {
      throw_<std::runtime_error>(
          "received partial FUSE_INIT packet on mount \"",
          mountPath_,
          "\": size=",
          res);
    }

    break;
  }

  if (init.header.opcode != FUSE_INIT) {
    replyError(*transport_, init.header, EPROTO);
    throw_<std::runtime_error>(
        "expected to receive FUSE_INIT for \"",
        mountPath_,
        "\" but got ",
        fuseOpcodeName(init.header.opcode),
        " (",
        init.header.opcode,
        ")");
  }

  fuse_init_out connInfo = {};
  connInfo.major = init.init.major;
  connInfo.minor = init.init.minor;
  connInfo.max_write = bufferSize_ - 4096;
  connInfo.max_readahead = init.init.max_readahead;

  int32_t max_background = maximumBackgroundRequests_;
  if (max_background > 65535) {
    max_background = 65535;
  } else if (max_background < 0) {
    max_background = 0;
  }
  // The libfuse documentation says this only applies to background
  // requests like readahead prefetches and direct I/O, but we have
  // empirically observed that, on Linux, without setting this value,
  // `rg -j 200` limits the number of active FUSE requests to 16.
  connInfo.max_background = static_cast<uint32_t>(max_background);
  // Allow the kernel to default connInfo.congestion_threshold. Linux
  // picks 3/4 of max_background.

  uint64_t capable = init.init.flags;
#ifdef FUSE_INIT_EXT
  if (capable & FUSE_INIT_EXT) {
    capable |= uint64_t{init.init.flags2} << 32;
  }
#endif
  uint64_t want = 0;

  // TODO: follow up and look at the new flags; particularly
  // FUSE_DO_READDIRPLUS, FUSE_READDIRPLUS_AUTO. FUSE_SPLICE_XXX are interesting
  // too, but may not directly benefit eden today.
  //
  // FUSE_ATOMIC_O_TRUNC is a nice optimization when the kernel supports it
  // and the FUSE daemon requires handling open/release for stateful file
  // handles. But FUSE_NO_OPEN_SUPPORT is superior, so edenfs has no need for
  // FUSE_ATOMIC_O_TRUNC. Also, on older kernels, it triggers a kernel bug.
  // See test_mmap_is_null_terminated_after_truncate_and_write_to_overlay
  // in mmap_test.py.

  // We can handle reads concurrently with any other type of request.
  want |= FUSE_ASYNC_READ;
  // We handle writes of any size.
  want |= FUSE_BIG_WRITES;

  // We don't support setuid and setgid mode bits anyway.
  want |= FUSE_HANDLE_KILLPRIV;
#ifdef FUSE_HANDLE_KILLPRIV_V2
  if (handleKillPrivV2_) {
    // Files never carry setuid, setgid or sticky bits here (setattr rejects
    // them and create/mknod strip them), so the kill requests this flag makes
    // the kernel send on write, truncate and chown have nothing to do. What
    // the flag buys is SB_NOSEC on the superblock: after the first write to a
    // file the kernel marks it S_NOSEC and stops asking for the
    // security.capability xattr before every later write.
    want |= FUSE_HANDLE_KILLPRIV_V2;
  }
#endif
  // Allow the kernel to cache ACL xattrs, even though we will fail all setxattr
  // calls.
  want |= FUSE_POSIX_ACL;
  // We're happy to let the kernel cache readlink responses.
  want |= FUSE_CACHE_SYMLINKS;
  // We can handle almost any request in parallel.
  want |= FUSE_PARALLEL_DIROPS;

#ifdef FUSE_MAX_PAGES
  if (fuseMaxPages_ > 0) {
    // Allow the kernel to send larger read requests (up to max_pages *
    // PAGE_SIZE). Default max_pages=32 caps reads at 128KB. Setting this flag
    // lets us configure max_pages in the FUSE_INIT response.
    want |= FUSE_MAX_PAGES;
  }
#endif

#ifdef FUSE_WRITEBACK_CACHE
  if (useWriteBackCache_) {
    // Writes go to the cache then write back to the underlying fs.
    want |= FUSE_WRITEBACK_CACHE;
  }
#else
  (void)useWriteBackCache_;
#endif

#ifdef FUSE_NO_OPEN_SUPPORT
  // File handles are stateless so the kernel does not need to send open() and
  // release().
  want |= FUSE_NO_OPEN_SUPPORT;
#endif
#ifdef FUSE_NO_OPENDIR_SUPPORT
  // File handles are stateless so the kernel does not need to send
  // open() and release().
  want |= FUSE_NO_OPENDIR_SUPPORT;
#endif
#ifdef FUSE_ALLOW_IDMAP
  // Allow processes whose uid/gid doesn't map into our user namespace to
  // access this mount. Without this, the kernel FUSE layer rejects requests
  // from such processes with EOVERFLOW.
  want |= FUSE_ALLOW_IDMAP;
#endif

  // Carries a pre-created io_uring transport across the INIT reply. Assigned
  // just below when the queues are created up front, and consumed at the
  // transport switch further down (after sendReply()); stays null otherwise.
  //
  // The two halves cannot be merged. The queues have to exist *before* the
  // reply, because that reply is the last point at which io_uring can be
  // declined: once it advertises FUSE_OVER_IO_URING the kernel blocks request
  // allocation until a ring is ready, so a failure afterwards hangs the mount
  // instead of falling back. But transport_ can only be swapped *after* the
  // reply, both because that is where the negotiated flags are known and
  // because the reply itself is still sent over devfuse. Hence a local that
  // spans the two.
  std::unique_ptr<IoUringFuseTransport> preparedIoUringTransport;

  // Only return the capabilities the kernel supports.
#if EDEN_HAVE_FUSE_IO_URING
  if (isIoUringTransportAvailable() && (capable & FUSE_OVER_IO_URING)) {
    bool negotiateIoUring = true;
    if (ioUringPreCreateQueues_) {
      auto transport = std::make_unique<IoUringFuseTransport>(
          ioUringQueueDepth_, ioUringDisableIoWait_);
      if (auto error = transport->prepareAllQueues(*this)) {
        XLOGF(
            WARN,
            "Not negotiating FUSE io_uring on mount \"{}\": {}",
            mountPath_,
            *error);
        getStats()->increment(&FuseStats::ioUringPreCreateQueuesFailure);
        negotiateIoUring = false;
      } else {
        getStats()->increment(&FuseStats::ioUringPreCreateQueuesSuccess);
        preparedIoUringTransport = std::move(transport);
      }
    }
    if (negotiateIoUring) {
      want |= FUSE_OVER_IO_URING;
    }
  }
#else
  (void)useIoUring_;
#endif

#ifdef FUSE_INIT_EXT
  if (want & 0xffffffff00000000ULL) {
    want |= FUSE_INIT_EXT;
  }
#endif

  want &= capable;
  connInfo.flags = static_cast<uint32_t>(want);
#ifdef FUSE_INIT_EXT
  connInfo.flags2 = static_cast<uint32_t>(want >> 32);
#endif

#ifdef FUSE_MAX_PAGES
  // Set max_pages only if the kernel actually supports FUSE_MAX_PAGES
  // (the flag survived the want &= capable check above).
  if (fuseMaxPages_ > 0 && (want & FUSE_MAX_PAGES)) {
    connInfo.max_pages =
        static_cast<uint16_t>(std::min(fuseMaxPages_, uint32_t(256)));
  }
#endif

#ifdef FUSE_MAX_PAGES
  const auto logMaxPages = connInfo.max_pages;
#else
  const auto logMaxPages = 0;
#endif

  if (init.init.major != FUSE_KERNEL_VERSION) {
    replyError(*transport_, init.header, EPROTO);
    throw_<std::runtime_error>(
        "Unsupported FUSE kernel version ",
        init.init.major,
        ".",
        init.init.minor,
        " while initializing \"",
        mountPath_,
        "\"");
  }

  // Update connInfo_
  // We have not started the other worker threads yet, so this is safe
  // to update without synchronization.
  connInfo_ = connInfo;

  // Send the INIT reply before informing the FuseDispatcher or signalling
  // initPromise_, so that the kernel will put the mount point in use and will
  // not block further filesystem access on us while running the FuseDispatcher
  // callback code.
  static_assert(
      FUSE_KERNEL_MINOR_VERSION > 22,
      "Your kernel headers are too old to build Eden.");
  if (init.init.minor > 22) {
    sendReply(*transport_, init.header, connInfo);
  } else {
    // If the protocol version predates the expansion of fuse_init_out, only
    // send the start of the packet.
    static_assert(FUSE_COMPAT_22_INIT_OUT_SIZE <= sizeof(connInfo));
    sendReply(
        *transport_,
        init.header,
        ByteRange{
            reinterpret_cast<const uint8_t*>(&connInfo),
            FUSE_COMPAT_22_INIT_OUT_SIZE});
  }

  if (negotiatedIoUringTransport(connInfo)) {
    XLOGF(
        DBG1,
        "Switching mount={} transport from {} to io_uring queueDepth={}",
        mountPath_,
        transport_->getName(),
        ioUringQueueDepth_);
    // Adopt the transport prepared before the INIT reply, if there is one.
    // Constructing a fresh one here instead would strand the queues it
    // already created and silently rebuild them per worker, undoing the
    // pre-creation. It is null whenever the queues were not pre-created, in
    // which case each worker creates its own queue as it starts, as before.
    transport_ = preparedIoUringTransport
        ? std::move(preparedIoUringTransport)
        : std::make_unique<IoUringFuseTransport>(
              ioUringQueueDepth_,
              ioUringDisableIoWait_,
              ioUringSkipSelfWakeup_);
    transportName_.store(transport_->getName(), std::memory_order_relaxed);
  }
  updateEffectiveWorkerThreadCount();

  XLOGF(
      DBG1,
      "Speaking fuse protocol kernel={}.{} local={}.{} on mount \"{}\" via {}, max_write={}, max_readahead={}, max_pages={}, capable={}, want={}",
      init.init.major,
      init.init.minor,
      FUSE_KERNEL_VERSION,
      FUSE_KERNEL_MINOR_VERSION,
      mountPath_,
      transport_->getName(),
      connInfo.max_write,
      connInfo.max_readahead,
      logMaxPages,
      capsFlagsToLabel(capable),
      capsFlagsToLabel(want));

  dispatcher_->initConnection(connInfo);
  maybeSetFuseReadAhead();
}

void FuseChannel::processSession() {
  transport_->processSession(*this);
}

bool FuseChannel::usesIoUringTransport() const {
  return dynamic_cast<IoUringFuseTransport*>(transport_.get()) != nullptr;
}

void FuseChannel::fulfillTakeoverReadiness() {
  if (!takeoverReadinessStarted_.load(std::memory_order_acquire)) {
    return;
  }
  if (takeoverReadyFinished_.exchange(true, std::memory_order_acq_rel)) {
    return;
  }

  XLOGF(
      DBG6,
      "FUSE transport ready mount={} transport={} readyWorkers={}",
      mountPath_,
      transport_->getName(),
      takeoverReadyWorkerCount_.load(std::memory_order_acquire));
  takeoverReadyPromise_.setValue();
}

void FuseChannel::failTakeoverReadiness(folly::exception_wrapper&& ew) {
  if (!takeoverReadinessStarted_.load(std::memory_order_acquire)) {
    return;
  }
  if (takeoverReadyFinished_.exchange(true, std::memory_order_acq_rel)) {
    return;
  }

  XLOGF(
      WARN,
      "FUSE transport readiness failed mount={} transport={}: {}",
      mountPath_,
      transport_->getName(),
      ew.what());
  takeoverReadyPromise_.setException(std::move(ew));
}

void FuseChannel::updateEffectiveWorkerThreadCount() {
  effectiveWorkerThreadCount_ =
      transport_->getWorkerThreadCount(configuredWorkerThreadCount_);
}

void FuseChannel::dispatchRequest(
    const FuseTransport& source,
    const fuse_in_header& header,
    ByteRange arg,
    pid_t myPid) {
  XLOGF(
      DBG7,
      "fuse request opcode={} {} unique={} len={} nodeid={} uid={} gid={} pid={}",
      header.opcode,
      fuseOpcodeName(header.opcode),
      header.unique,
      header.len,
      header.nodeid,
      header.uid,
      header.gid,
      header.pid);

  // On Linux, if security caps are enabled and the FUSE filesystem implements
  // xattr support, every FUSE_WRITE opcode is preceded by FUSE_GETXATTR for
  // "security.capability". Until we discover a way to tell the kernel that
  // they will always return nothing in an Eden mount, short-circuit that path
  // as efficiently and as early as possible.
  //
  // On some systems, the kernel also frequently requests
  // POSIX ACL xattrs, so fast track those too, if only to make strace
  // logs easier to follow.
  if (header.opcode == FUSE_GETXATTR) {
    const auto getxattr = reinterpret_cast<const fuse_getxattr_in*>(arg.data());

    // Evaluate strlen before the comparison loop below.
    const StringPiece namePiece{reinterpret_cast<const char*>(getxattr + 1)};
    static constexpr StringPiece kFastTracks[] = {
        "security.capability",
        "security.selinux",
        "system.posix_acl_access",
        "system.posix_acl_default"};

    // Unclear whether one strlen and matching compares is better than
    // strcmps, but it's probably in the noise.
    bool matched = false;
    for (auto fastTrack : kFastTracks) {
      if (namePiece == fastTrack) {
        replyError(source, header, ENODATA);
        matched = true;
        break;
      }
    }
    if (matched) {
      return;
    }
  }

  // Sanity check to ensure that the request wasn't from ourself.
  //
  // We should never make requests to ourself via normal filesystem
  // operations going through the kernel.  Otherwise we risk deadlocks if the
  // kernel calls us while holding an inode lock, and we then end up making a
  // filesystem call that need the same inode lock.  We will then not be able
  // to resolve this deadlock on kernel inode locks without rebooting the
  // system.
  if (UNLIKELY(static_cast<pid_t>(header.pid) == myPid)) {
    replyError(source, header, EIO);
    XLOGF(
        CRITICAL,
        "Received FUSE request from our own pid: opcode={} nodeid={} pid={}",
        header.opcode,
        header.nodeid,
        header.pid);
    return;
  }

  auto* handlerEntry = lookupFuseHandlerEntry(header.opcode);
  processAccessLog_->recordAccess(
      header.pid,
      handlerEntry ? handlerEntry->accessType : AccessType::FsChannelOther);

  switch (header.opcode) {
    case FUSE_INIT:
      replyError(source, header, EPROTO);
      throw std::runtime_error(
          "received FUSE_INIT after we have been initialized!?");

    case FUSE_GETLK:
    case FUSE_SETLK:
    case FUSE_SETLKW:
      // Deliberately not handling locking; this causes
      // the kernel to do it for us
      XLOG(DBG7, fuseOpcodeName(header.opcode));
      replyError(source, header, ENOSYS);
      break;

    case FUSE_LSEEK:
      // We only support stateless file handles, so lseek() is meaningless
      // for us.  Returning ENOSYS causes the kernel to implement it for us,
      // and will cause it to stop sending subsequent FUSE_LSEEK requests.
      XLOG(DBG7, "FUSE_LSEEK");
      replyError(source, header, ENOSYS);
      break;

    case FUSE_POLL:
      // We do not currently implement FUSE_POLL.
      XLOG(DBG7, "FUSE_POLL");
      replyError(source, header, ENOSYS);
      break;

    case FUSE_INTERRUPT:
      // no reply is required
      XLOG(DBG7, "FUSE_INTERRUPT");
      // Ignore it: we don't have a reliable way to guarantee
      // that interrupting functions correctly.
      // In addition, the kernel (certainly on macOS) may recycle
      // ids too quickly for us to safely track by `unique` id.
      source.replyNone(*this, header);
      break;

    case FUSE_DESTROY:
      XLOG(DBG7, "FUSE_DESTROY");
      dispatcher_->destroy();
      // FUSE on linux doesn't care whether we reply to FUSE_DESTROY
      // but the macOS implementation blocks the unmount syscall until
      // we have responded, which in turn blocks our attempt to gracefully
      // unmount, so we respond here.  It doesn't hurt Linux to respond
      // so we do it for both platforms.
      replyError(source, header, 0);
      break;

    case FUSE_NOTIFY_REPLY:
      XLOG(DBG7, "FUSE_NOTIFY_REPLY");
      // Don't strictly need to do anything here, but may want to
      // turn the kernel notifications in Futures and use this as
      // a way to fulfil the promise
      source.replyNone(*this, header);
      break;

    case FUSE_IOCTL:
      // Rather than the default ENOSYS, we need to return ENOTTY
      // to indicate that the requested ioctl is not supported
      replyError(source, header, ENOTTY);
      break;

    default: {
      if (handlerEntry && handlerEntry->handler) {
        auto requestId = generateUniqueID();
        // Trace events only feed live `eden trace fs` streams, so they are
        // not built or published while nothing is subscribed.
        const bool traced = traceBus_->hasSubscription();
        if (traced) {
          if (handlerEntry->argRenderer &&
              traceDetailedArguments_->load(std::memory_order_acquire)) {
            traceBus_->publish(
                FuseTraceEvent::start(
                    requestId, header, handlerEntry->argRenderer(arg)));
          } else {
            traceBus_->publish(FuseTraceEvent::start(requestId, header));
          }
        }
        const auto now = std::chrono::steady_clock::now();
        recordRequestStart(requestId, header, now);

        // Acquire a RequestPermit before executing the FUSE request. This
        // function will block if there are too many inflight requests. This
        // needs to be captured in the ensure block to ensure the permit is
        // only destroyed after the request has finished
        auto requestPermit = acquireFsRequestPermit();

        // This is a shared_ptr because, due to timeouts, the internal request
        // lifetime may not match the FUSE request lifetime, so we capture it
        // in both. I'm sure this could be improved with some cleverness.
        auto request =
            std::make_shared<FuseRequestContext>(this, source, header);

        auto should_log = false;
        {
          auto state = state_.wlock();
          ++state->pendingRequests;

          // TODO(helsel): Use the rate limiter to log when the maximum number
          // of inflight requests are hit instead of keeping track of pending
          // requests separately.
          if (maximumInFlightRequests_ > 0 &&
              state->pendingRequests == maximumInFlightRequests_ &&
              now >= state->lastHighFuseRequestsLog_ +
                      highFuseRequestsLogInterval_) {
            should_log = true;
            state->lastHighFuseRequestsLog_ = now;
          }
        }

        if (should_log && this->edenFsEventsLogger_) {
          this->edenFsEventsLogger_->logEvent(ManyLiveFsChannelRequests{});
        }

        auto headerCopy = header;

        FB_LOG(*straceLogger_, DBG7, ([&]() -> std::string {
          std::string rendered;
          if (handlerEntry->argRenderer) {
            rendered = handlerEntry->argRenderer(arg);
          }
          return fmt::format(
              "{}({}{}{})",
              handlerEntry->getShortName(),
              headerCopy.nodeid,
              rendered.empty() ? "" : ", ",
              rendered);
        })());

        auto stats = dispatcher_->getStats().copy();
        auto fut = [&]() -> ImmediateFuture<folly::Unit> {
          try {
            request->startRequest(
                stats.copy(),
                handlerEntry->duration,
                *(liveRequestWatches_.get()));
            return (this->*handlerEntry->handler)(
                *request, request->getReq(), arg);
          } catch (...) {
            return folly::Try<folly::Unit>{
                folly::exception_wrapper{std::current_exception()}};
          }
        }();
        auto incrementDispatchCounter =
            [&stats](
                FuseStats::CounterPtr aggregateCounter,
                FuseStats::CounterPtr opcodeCounter) {
              stats->increment(aggregateCounter);
              if (opcodeCounter) {
                stats->increment(opcodeCounter);
              }
            };
        if (fut.isReady()) {
          incrementDispatchCounter(
              &FuseStats::dispatchImmediate, handlerEntry->dispatchImmediate);
          // Most requests complete inline, so finish them here rather than
          // through the folly::Future chain the deferred path needs for its
          // executor hop and timeout. The request permit is released when it
          // goes out of scope below, after the request has finished.
          try {
            request->handleResult(
                std::move(fut).getTry(),
                notifier_.get(),
                stats,
                handlerEntry->countSuccessful,
                handlerEntry->countFailure);
          } catch (...) {
            // Replying failed, typically because the kernel already dropped
            // an interrupted request. handleResult counted the failure
            // before it replied; the request must still be accounted for
            // below so the session can drain.
            XLOGF(
                DBG3,
                "failed to reply to FUSE request: {}",
                folly::exceptionStr(std::current_exception()));
          }
          finishRequest(requestId, headerCopy, request->getResult());
        } else {
          incrementDispatchCounter(
              &FuseStats::dispatchDeferred, handlerEntry->dispatchDeferred);
          request
              ->catchErrors(
                  std::move(fut)
                      .semi()
                      .via(threadPool_.get())
                      .ensure([request] {})
                      .within(requestTimeout_),
                  notifier_.get(),
                  stats.copy(),
                  handlerEntry->countSuccessful,
                  handlerEntry->countFailure)
              .ensure([this,
                       request,
                       requestId,
                       headerCopy,
                       requestPermit = std::move(requestPermit)]() mutable {
                // finishRequest() deletes this channel when it completes the
                // last request of a destroyed session, and the request's
                // RequestContext records into the channel's ProcessAccessLog
                // when it is destroyed, so it has to go first.
                auto result = request->getResult();
                request.reset();
                finishRequest(requestId, headerCopy, result);
              });
        }
        break;
      }

      const auto opcode = header.opcode;
      tryRlockCheckBeforeUpdate<folly::Unit>(
          unhandledOpcodes_,
          [&](const auto& unhandledOpcodes) -> std::optional<folly::Unit> {
            if (unhandledOpcodes.find(opcode) != unhandledOpcodes.end()) {
              return folly::unit;
            }
            return std::nullopt;
          },
          [&](auto& unhandledOpcodes) -> folly::Unit {
            XLOGF(
                WARN,
                "unhandled fuse opcode {}({})",
                opcode,
                fuseOpcodeName(opcode));
            unhandledOpcodes->insert(opcode);
            return folly::unit;
          });

      try {
        replyError(source, header, ENOSYS);
      } catch (const std::system_error& exc) {
        XLOGF(ERR, "Failed to write error response to fuse: {}", exc.what());
        errorLogger_.log(
            EdenErrorInfo::fuse(exc, std::nullopt, mountPath_.asString()));
        requestSessionExit(StopReason::FUSE_WRITE_ERROR);
        return;
      }
      break;
    }
  }
}

void FuseChannel::sessionComplete(folly::Synchronized<State>::LockedPtr state) {
  // Check to see if we should delete ourself after fulfilling
  // sessionCompletePromise_
  bool destroy = state->destroyPending;

  // Build the StopData to return

  auto data = std::make_unique<StopData>();
  data->reason = state->stopReason;
  data->stopRequestedAt = state->stopRequestedAt;
  // Unlock the state before the remaining steps
  state.unlock();

  // Stop the invalidation threads. We do not do this when requestSessionExit()
  // is called since we want to continue to allow invalidation requests to be
  // processed until all outstanding requests complete.
  stopInvalidationThread();

  if (isFuseDeviceValid(data->reason) && connInfo_.has_value()) {
    data->fuseDevice = std::move(fuseDevice_);
    data->fuseSettings = connInfo_.value();
  }

  // Fulfill sessionCompletePromise
  sessionCompletePromise_.setValue(std::move(data));

  // Destroy ourself if desired
  if (destroy) {
    delete this;
  }
}

ImmediateFuture<folly::Unit> FuseChannel::fuseRead(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto read = reinterpret_cast<const fuse_read_in*>(arg.data());

  XLOG(DBG7, "FUSE_READ");

  auto ino = InodeNumber{header.nodeid};
  return dispatcher_
      ->read(ino, read->size, read->offset, request.getObjectFetchContext())
      .thenValue([&request](BufVec&& buf) { request.sendReply(*buf); });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseWrite(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto write = reinterpret_cast<const fuse_write_in*>(arg.data());
  auto bufPtr = reinterpret_cast<const char*>(write + 1);
  if (connInfo_->minor < 9) {
    bufPtr =
        reinterpret_cast<const char*>(arg.data()) + FUSE_COMPAT_WRITE_IN_SIZE;
  }
  XLOGF(DBG7, "FUSE_WRITE {} @{}", write->size, write->offset);

  auto ino = InodeNumber{header.nodeid};
  return dispatcher_
      ->write(
          ino,
          folly::StringPiece{bufPtr, write->size},
          write->offset,
          request.getObjectFetchContext())
      .thenValue([&request](size_t written) {
        fuse_write_out out = {};
        out.size = written;
        request.sendReply(out);
      });
}

namespace {
PathComponentPiece extractPathComponent(StringPiece s, bool requireUtf8Path) {
  try {
    return PathComponentPiece(s);
  } catch (const PathComponentNotUtf8& ex) {
    if (requireUtf8Path) {
      throw std::system_error(EILSEQ, std::system_category(), ex.what());
    }

    return PathComponentPiece(s, detail::SkipPathSanityCheck());
  }
}
} // namespace

ImmediateFuture<folly::Unit> FuseChannel::fuseLookup(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto name = extractPathComponent(
      reinterpret_cast<const char*>(arg.data()), requireUtf8Path_);
  const auto parent = InodeNumber{header.nodeid};

  XLOGF(DBG7, "FUSE_LOOKUP parent={} name={}", parent, name);

  return dispatcher_
      ->lookup(header.unique, parent, name, request.getObjectFetchContext())
      .thenValue([&request](fuse_entry_out entry) {
        request.sendReplyWithInode(entry.nodeid, entry);
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseForget(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  auto forget = reinterpret_cast<const fuse_forget_in*>(arg.data());
  XLOGF(
      DBG7, "FUSE_FORGET inode={} nlookup={}", header.nodeid, forget->nlookup);
  dispatcher_->forget(InodeNumber{header.nodeid}, forget->nlookup);
  request.replyNone();
  return folly::unit;
}

ImmediateFuture<folly::Unit> FuseChannel::fuseGetAttr(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange /*arg*/) {
  XLOGF(DBG7, "FUSE_GETATTR inode={}", header.nodeid);
  return dispatcher_
      ->getattr(InodeNumber{header.nodeid}, request.getObjectFetchContext())
      .thenValue([&request](FuseDispatcher::Attr attr) {
        request.sendReply(attr.asFuseAttr());
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseSetAttr(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto setattr = reinterpret_cast<const fuse_setattr_in*>(arg.data());
  XLOGF(DBG7, "FUSE_SETATTR inode={}", header.nodeid);
  return dispatcher_
      ->setattr(
          InodeNumber{header.nodeid}, *setattr, request.getObjectFetchContext())
      .thenValue([&request](FuseDispatcher::Attr attr) {
        request.sendReply(attr.asFuseAttr());
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseReadLink(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange /*arg*/) {
  XLOGF(DBG7, "FUSE_READLINK inode={}", header.nodeid);
  bool kernelCachesReadlink = false;
#ifdef FUSE_CACHE_SYMLINKS
  kernelCachesReadlink = connInfo_->flags & FUSE_CACHE_SYMLINKS;
#endif
  InodeNumber ino{header.nodeid};
  return dispatcher_
      ->readlink(ino, kernelCachesReadlink, request.getObjectFetchContext())
      .thenValue([&request](std::string&& str) {
        request.sendReply(folly::StringPiece(str));
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseSymlink(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto nameStr = reinterpret_cast<const char*>(arg.data());
  XLOG(DBG7, "FUSE_SYMLINK");
  const auto name = extractPathComponent(nameStr, requireUtf8Path_);
  const StringPiece link{nameStr + name.view().size() + 1};

  InodeNumber parent{header.nodeid};
  return dispatcher_
      ->symlink(parent, name, link, request.getObjectFetchContext())
      .thenValue([&request](fuse_entry_out entry) {
        request.sendReplyWithInode(entry.nodeid, entry);
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseMknod(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto nod = reinterpret_cast<const fuse_mknod_in*>(arg.data());
  auto nameStr = reinterpret_cast<const char*>(nod + 1);

  if (connInfo_->minor >= 12) {
    // Kernel passes umask in fuse_mknod_in, but unless FUSE_CAP_DONT_MASK is
    // set, the kernel has already masked it out in mode.
    // https://sourceforge.net/p/fuse/mailman/message/22844100/
  } else {
    // Else: no umask or padding fields available
    nameStr =
        reinterpret_cast<const char*>(arg.data()) + FUSE_COMPAT_MKNOD_IN_SIZE;
  }

  const auto name = extractPathComponent(nameStr, requireUtf8Path_);
  XLOGF(DBG7, "FUSE_MKNOD {}", name);

  InodeNumber parent{header.nodeid};
  return dispatcher_
      ->mknod(
          parent, name, nod->mode, nod->rdev, request.getObjectFetchContext())
      .thenValue([&request](fuse_entry_out entry) {
        request.sendReplyWithInode(entry.nodeid, entry);
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseMkdir(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto dir = reinterpret_cast<const fuse_mkdir_in*>(arg.data());
  const auto nameStr = reinterpret_cast<const char*>(dir + 1);
  const auto name = extractPathComponent(nameStr, requireUtf8Path_);

  XLOGF(DBG7, "FUSE_MKDIR {}", name);

  // Kernel passes umask in fuse_mkdir_in, but unless FUSE_CAP_DONT_MASK is
  // set, the kernel has already masked it out in mode.
  // https://sourceforge.net/p/fuse/mailman/message/22844100/

  XLOGF(DBG7, "mode = {}; umask = {}", dir->mode, dir->umask);

  InodeNumber parent{header.nodeid};
  mode_t mode = dir->mode & ~dir->umask;
  return dispatcher_->mkdir(parent, name, mode, request.getObjectFetchContext())
      .thenValue([&request](fuse_entry_out entry) {
        request.sendReplyWithInode(entry.nodeid, entry);
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseUnlink(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto nameStr = reinterpret_cast<const char*>(arg.data());
  const auto name = extractPathComponent(nameStr, requireUtf8Path_);

  XLOGF(DBG7, "FUSE_UNLINK {}", name);

  InodeNumber parent{header.nodeid};
  return dispatcher_->unlink(parent, name, request.getObjectFetchContext())
      .thenValue([&request](auto&&) { request.replyError(0); });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseRmdir(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto nameStr = reinterpret_cast<const char*>(arg.data());
  const auto name = extractPathComponent(nameStr, requireUtf8Path_);

  XLOGF(DBG7, "FUSE_RMDIR {}", name);
  InodeNumber parent{header.nodeid};
  return dispatcher_->rmdir(parent, name, request.getObjectFetchContext())
      .thenValue([&request](auto&&) { request.replyError(0); });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseRename(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto rename = reinterpret_cast<const fuse_rename_in*>(arg.data());
  auto oldNameStr = reinterpret_cast<const char*>(rename + 1);
  StringPiece oldName{oldNameStr};
  StringPiece newName{oldNameStr + oldName.size() + 1};

  InodeNumber parent{header.nodeid};
  InodeNumber newParent{rename->newdir};
  XLOGF(DBG7, "FUSE_RENAME {} -> {}", oldName, newName);
  return dispatcher_
      ->rename(
          parent,
          extractPathComponent(oldName, requireUtf8Path_),
          newParent,
          extractPathComponent(newName, requireUtf8Path_),
          request.getObjectFetchContext())
      .thenValue([&request](auto&&) { request.replyError(0); });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseRename2(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto rename = reinterpret_cast<const fuse_rename2_in*>(arg.data());
  auto oldNameStr = reinterpret_cast<const char*>(rename + 1);
  StringPiece oldName{oldNameStr};
  StringPiece newName{oldNameStr + oldName.size() + 1};

  InodeNumber parent{header.nodeid};
  InodeNumber newParent{rename->newdir};
  XLOGF(
      DBG7,
      "FUSE_RENAME2 {} -> {}, flags={:#x}",
      oldName,
      newName,
      rename->flags);
  return dispatcher_
      ->rename2(
          parent,
          extractPathComponent(oldName, requireUtf8Path_),
          newParent,
          extractPathComponent(newName, requireUtf8Path_),
          rename->flags,
          request.getObjectFetchContext())
      .thenValue([&request](auto&&) { request.replyError(0); });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseLink(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto link = reinterpret_cast<const fuse_link_in*>(arg.data());
  const auto nameStr = reinterpret_cast<const char*>(link + 1);
  const auto newName = extractPathComponent(nameStr, requireUtf8Path_);

  XLOGF(DBG7, "FUSE_LINK {}", newName);

  InodeNumber ino{link->oldnodeid};
  InodeNumber newParent{header.nodeid};
  return dispatcher_->link(ino, newParent, newName)
      .thenValue([&request](fuse_entry_out entry) {
        request.sendReplyWithInode(entry.nodeid, entry);
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseOpen(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto open = reinterpret_cast<const fuse_open_in*>(arg.data());
  XLOG(DBG7, "FUSE_OPEN");
  auto ino = InodeNumber{header.nodeid};
  return dispatcher_->open(ino, open->flags).thenValue([&request](uint64_t fh) {
    fuse_open_out out = {};
    out.open_flags |= FOPEN_KEEP_CACHE;
    out.fh = fh;
    request.sendReply(out);
  });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseStatFs(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange /*arg*/) {
  XLOG(DBG7, "FUSE_STATFS");
  return dispatcher_->statfs(InodeNumber{header.nodeid})
      .thenValue([&request](struct fuse_kstatfs&& info) {
        fuse_statfs_out out = {};
        out.st = info;
        request.sendReply(out);
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseRelease(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  XLOG(DBG7, "FUSE_RELEASE");
  auto ino = InodeNumber{header.nodeid};
  auto release = reinterpret_cast<const fuse_release_in*>(arg.data());
  return dispatcher_->release(ino, release->fh)
      .thenValue([&request](folly::Unit) { request.replyError(0); });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseFsync(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto fsync = reinterpret_cast<const fuse_fsync_in*>(arg.data());
  // There's no symbolic constant for this :-/
  const bool datasync = fsync->fsync_flags & 1;

  XLOG(DBG7, "FUSE_FSYNC");

  auto ino = InodeNumber{header.nodeid};
  return dispatcher_->fsync(ino, datasync, request.getObjectFetchContext())
      .thenValue([&request](auto&&) { request.replyError(0); });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseSetXAttr(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto setxattr = reinterpret_cast<const fuse_setxattr_in*>(arg.data());
  // Use the compat size (8 bytes) because EdenFS does not negotiate
  // FUSE_SETXATTR_EXT, so the kernel sends the old 2-field struct.
  // sizeof(fuse_setxattr_in) is 16 bytes in FUSE 7.33+, which would
  // overshoot the name offset.
  const auto nameStr =
      reinterpret_cast<const char*>(arg.data()) + FUSE_COMPAT_SETXATTR_IN_SIZE;
  const StringPiece attrName{nameStr};
  const auto bufPtr = nameStr + attrName.size() + 1;
  const StringPiece value(bufPtr, setxattr->size);

  XLOG(DBG7, "FUSE_SETXATTR");

  return dispatcher_
      ->setxattr(InodeNumber{header.nodeid}, attrName, value, setxattr->flags)
      .thenValue([&request](auto&&) { request.replyError(0); });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseGetXAttr(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto getxattr = reinterpret_cast<const fuse_getxattr_in*>(arg.data());
  const auto nameStr = reinterpret_cast<const char*>(getxattr + 1);
  const StringPiece attrName{nameStr};
  XLOG(DBG7, "FUSE_GETXATTR");
  InodeNumber ino{header.nodeid};
  return dispatcher_->getxattr(ino, attrName, request.getObjectFetchContext())
      .thenValue([&request, size = getxattr->size](const std::string& attr) {
        if (size == 0) {
          fuse_getxattr_out out = {};
          out.size = attr.size();
          request.sendReply(out);
        } else if (size < attr.size()) {
          request.replyError(ERANGE);
        } else {
          request.sendReply(StringPiece(attr));
        }
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseListXAttr(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto listattr = reinterpret_cast<const fuse_getxattr_in*>(arg.data());
  XLOG(DBG7, "FUSE_LISTXATTR");
  InodeNumber ino{header.nodeid};
  return dispatcher_->listxattr(ino, request.getObjectFetchContext())
      .thenValue([&request, size = listattr->size](
                     const std::vector<std::string>& attrs) {
        // Initialize count to include the \0 for each
        // entry.
        size_t count = attrs.size();
        for (const auto& attr : attrs) {
          count += attr.size();
        }

        if (size == 0) {
          // caller is asking for the overall size
          fuse_getxattr_out out = {};
          out.size = count;
          request.sendReply(out);
        } else if (size < count) {
          XLOGF(
              DBG7, "LISTXATTR input size is {} and count is {}", size, count);
          request.replyError(ERANGE);
        } else {
          std::string buf;
          buf.reserve(count);
          for (const auto& attr : attrs) {
            buf.append(attr);
            buf.push_back(0);
          }
          XLOGF(DBG7, "LISTXATTR: {}", buf);
          request.sendReply(folly::StringPiece(buf));
        }
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseRemoveXAttr(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto nameStr = reinterpret_cast<const char*>(arg.data());
  const StringPiece attrName{nameStr};
  XLOG(DBG7, "FUSE_REMOVEXATTR");
  return dispatcher_->removexattr(InodeNumber{header.nodeid}, attrName)
      .thenValue([&request](auto&&) { request.replyError(0); });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseFlush(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto flush = reinterpret_cast<const fuse_flush_in*>(arg.data());
  XLOG(DBG7, "FUSE_FLUSH");

  auto ino = InodeNumber{header.nodeid};
  return dispatcher_->flush(ino, flush->lock_owner)
      .thenValue([&request](auto&&) { request.replyError(0); });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseOpenDir(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto open = reinterpret_cast<const fuse_open_in*>(arg.data());
  XLOG(DBG7, "FUSE_OPENDIR");
  auto ino = InodeNumber{header.nodeid};
  auto minorVersion = connInfo_->minor;
  return dispatcher_->opendir(ino, open->flags)
      .thenValue([&request, minorVersion](uint64_t fh) {
        fuse_open_out out = {};
#ifdef FOPEN_CACHE_DIR
        if (minorVersion >= 28) {
          // Opt into readdir caching.
          out.open_flags |= FOPEN_KEEP_CACHE | FOPEN_CACHE_DIR;
        }
#else
        (void)minorVersion;
#endif
        out.fh = fh;
        request.sendReply(out);
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseReadDir(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  auto read = reinterpret_cast<const fuse_read_in*>(arg.data());
  XLOG(DBG7, "FUSE_READDIR");
  auto ino = InodeNumber{header.nodeid};
  return dispatcher_
      ->readdir(
          ino,
          FuseDirList{read->size},
          read->offset,
          read->fh,
          request.getObjectFetchContext())
      .thenValue([&request](FuseDirList&& list) {
        const auto buf = list.getBuf();
        request.sendReply(StringPiece{buf});
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseReleaseDir(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  XLOG(DBG7, "FUSE_RELEASEDIR");
  auto ino = InodeNumber{header.nodeid};
  auto release = reinterpret_cast<const fuse_release_in*>(arg.data());
  return dispatcher_->releasedir(ino, release->fh)
      .thenValue([&request](folly::Unit) { request.replyError(0); });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseFsyncDir(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto fsync = reinterpret_cast<const fuse_fsync_in*>(arg.data());
  // There's no symbolic constant for this :-/
  const bool datasync = fsync->fsync_flags & 1;

  XLOG(DBG7, "FUSE_FSYNCDIR");

  auto ino = InodeNumber{header.nodeid};
  return dispatcher_->fsyncdir(ino, datasync).thenValue([&request](auto&&) {
    request.replyError(0);
  });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseAccess(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto access = reinterpret_cast<const fuse_access_in*>(arg.data());
  XLOG(DBG7, "FUSE_ACCESS");
  InodeNumber ino{header.nodeid};
  return dispatcher_->access(ino, access->mask).thenValue([&request](auto&&) {
    request.replyError(0);
  });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseCreate(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto create = reinterpret_cast<const fuse_create_in*>(arg.data());
  const auto name = extractPathComponent(
      reinterpret_cast<const char*>(create + 1), requireUtf8Path_);
  XLOGF(DBG7, "FUSE_CREATE {}", name);
  auto ino = InodeNumber{header.nodeid};
  return dispatcher_
      ->create(
          ino,
          name,
          create->mode,
          create->flags,
          request.getObjectFetchContext())
      .thenValue([&request](fuse_entry_out entry) {
        fuse_open_out out = {};
        out.open_flags |= FOPEN_KEEP_CACHE;

        XLOGF(DBG7, "CREATE fh={} flags={}", out.fh, out.open_flags);

        folly::fbvector<iovec> vec;

        // 3 to avoid realloc when sendReply prepends a header to the iovec
        vec.reserve(3);
        vec.push_back(make_iovec(entry));
        vec.push_back(make_iovec(out));

        request.sendReplyWithInode(entry.nodeid, std::move(vec));
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseBmap(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto bmap = reinterpret_cast<const fuse_bmap_in*>(arg.data());
  XLOG(DBG7, "FUSE_BMAP");
  return dispatcher_
      ->bmap(InodeNumber{header.nodeid}, bmap->blocksize, bmap->block)
      .thenValue([&request](uint64_t resultIdx) {
        fuse_bmap_out out{};
        out.block = resultIdx;
        request.sendReply(out);
      });
}

ImmediateFuture<folly::Unit> FuseChannel::fuseBatchForget(
    FuseRequestContext& request,
    const fuse_in_header& /*header*/,
    ByteRange arg) {
  const auto forgets =
      reinterpret_cast<const fuse_batch_forget_in*>(arg.data());
  auto item = reinterpret_cast<const fuse_forget_one*>(forgets + 1);
  const auto end = item + forgets->count;
  XLOG(DBG7, "FUSE_BATCH_FORGET");

  while (item != end) {
    dispatcher_->forget(InodeNumber{item->nodeid}, item->nlookup);
    ++item;
  }
  request.replyNone();
  return folly::unit;
}

ImmediateFuture<folly::Unit> FuseChannel::fuseFallocate(
    FuseRequestContext& request,
    const fuse_in_header& header,
    ByteRange arg) {
  const auto* allocate = reinterpret_cast<const fuse_fallocate_in*>(arg.data());
  XLOG(DBG7, "FUSE_FALLOCATE");

  // We only care to avoid the glibc fallback implementation for
  // posix_fallocate, so don't even pretend to support all the fancy extra modes
  // in Linux's fallocate(2).
  if (allocate->mode != 0) {
    request.replyError(ENOSYS);
    return folly::unit;
  }

  // ... but definitely don't let glibc fall back on its posix_fallocate
  // emulation, which writes one byte per 512 byte chunk in the entire file,
  // which is extremely expensive in an EdenFS checkout.
  return dispatcher_
      ->fallocate(
          InodeNumber{header.nodeid},
          allocate->offset,
          allocate->length,
          request.getObjectFetchContext())
      .thenValue([&request](auto) { request.replyError(0); });
}

FuseDeviceUnmountedDuringInitialization::
    FuseDeviceUnmountedDuringInitialization(AbsolutePathPiece mountPath)
    : std::runtime_error{fmt::format(
          "FUSE mount \"{}\" was unmounted before we received the INIT packet",
          mountPath)} {}

size_t FuseChannel::getRequestMetric(
    RequestMetricsScope::RequestMetric metric) const {
  std::vector<size_t> counters;
  for (auto& thread_watches : liveRequestWatches_.accessAllThreads()) {
    counters.emplace_back(
        RequestMetricsScope::getMetricFromWatches(metric, *thread_watches));
  }
  return RequestMetricsScope::aggregateMetricCounters(metric, counters);
}

} // namespace facebook::eden

#endif
