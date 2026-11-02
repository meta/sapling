/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/inodes/FuseDispatcherImpl.h"
#include <folly/logging/xlog.h>
#include <exception>

#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/SystemError.h"
#include "eden/fs/fuse/FuseDirList.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/InodeMap.h"
#include "eden/fs/inodes/InodePtr.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/utils/Clock.h"
#include "eden/fs/utils/LinuxKernelVersion.h"

using namespace folly;
using std::string;
using std::vector;

namespace facebook::eden {

namespace {

fuse_entry_out computeEntryParam(const FuseDispatcher::Attr& attr) {
  XDCHECK(attr.st.st_ino) << "We should never return a 0 inode to FUSE";
  fuse_entry_out entry = {};
  entry.nodeid = attr.st.st_ino;
  entry.generation = 0;
  auto fuse_attr = attr.asFuseAttr();
  entry.attr = fuse_attr.attr;
  entry.attr_valid = fuse_attr.attr_valid;
  entry.attr_valid_nsec = fuse_attr.attr_valid_nsec;
  entry.entry_valid = fuse_attr.attr_valid;
  entry.entry_valid_nsec = fuse_attr.attr_valid_nsec;
  return entry;
}

constexpr int64_t kBrokenInodeCacheSeconds = 5;

FuseDispatcher::Attr attrForInodeWithCorruptOverlay(
    const InodeBase& inode) noexcept {
  return FuseDispatcher::Attr{
      inode.getStatForCorruptOverlay(), kBrokenInodeCacheSeconds};
}

bool isRestrictedTreeStat(const struct stat& st) {
  return S_ISDIR(st.st_mode) && (st.st_mode & 07777) == 0;
}

} // namespace

FuseDispatcherImpl::FuseDispatcherImpl(EdenMount* mount)
    : FuseDispatcher(mount->getStats().copy()),
      mount_(mount),
      inodeMap_(mount_->getInodeMap()) {}

uint64_t FuseDispatcherImpl::computeTtl() const {
  auto config = mount_->getEdenConfig();
  if (!config->enablePressureBasedGc.getValue()) {
    return std::numeric_limits<int32_t>::max();
  }
  auto policy = mount_->getInodePressurePolicy();
  auto inodeCount = inodeMap_->getTotalInodeCountFast();
  return static_cast<uint64_t>(policy->getFuseTtl(inodeCount).count());
}

uint64_t FuseDispatcherImpl::computeRestrictedTreeTtl() const {
  return mount_->getEdenConfig()->restrictedTreeTtlSeconds.getValue() / 2;
}

uint64_t FuseDispatcherImpl::computeTtlForStat(const struct stat& st) const {
  if (isRestrictedTreeStat(st)) {
    return computeRestrictedTreeTtl();
  }
  return computeTtl();
}

uint64_t FuseDispatcherImpl::computeNegativeEntryTtl() const {
#ifdef __linux__
  // Workaround Linux regression where INVAL_ENTRY cannot invalidate negative
  // dentries by setting TTL to 0.
  // First bad commit: c9ba789dad15ba65662bba17595c0aeaa0cfcf1c, included
  // since 6.19.
  // https://lore.kernel.org/linux-fsdevel/CAFbF8N7++zopZuEcsKRxBV_sgOGCbzCY0hOyMw1SiGAtuzGhyQ@mail.gmail.com/
  // TODO: Check upstream fix version and disable the mitigation accordingly.
  static const bool hasBrokenKernel = [] {
    try {
      auto version = getRunningLinuxKernelVersion();
      if (version.major > 6 || (version.major == 6 && version.minor >= 19)) {
        XLOG(INFO, "Mitigate kernel INVAL_ENTRY bug. Negative dentry TTL=0.");
        return true;
      }
    } catch (const std::exception& ex) {
      XLOGF(
          WARN,
          "Failed to detect kernel version for INVAL_ENTRY bug mitigation: {}",
          ex.what());
    }
    return false;
  }();
  if (hasBrokenKernel) {
    return 0;
  }
#endif

  return mount_->getEdenConfig()->fuseNegativeDcacheTtlSeconds.getValue();
}

fuse_entry_out FuseDispatcherImpl::negativeLookupEntry() const {
  fuse_entry_out entry = {};
  auto ttl = computeNegativeEntryTtl();
  entry.attr_valid = ttl;
  entry.entry_valid = ttl;
  return entry;
}

ImmediateFuture<fuse_entry_out> FuseDispatcherImpl::negativeLookupReply(
    PathComponentPiece name) const {
  return mount_->getServerState()
      ->getFaultInjector()
      .checkAsync("FuseDispatcherImpl::negativeLookup", name)
      .thenValue([this](auto&&) { return negativeLookupEntry(); });
}

ImmediateFuture<FuseDispatcher::Attr> FuseDispatcherImpl::getattr(
    InodeNumber ino,
    const ObjectFetchContextPtr& context) {
  return mount_->getServerState()
      ->getFaultInjector()
      .checkAsync("FuseDispatcherImpl::getattr", ino)
      .thenValue([this, ino, context = context.copy()](auto&&) {
        return inodeMap_->lookupInode(ino, context);
      })
      .thenValue([context = context.copy()](const InodePtr& inode) {
        inode->markGcRef();
        return inode->stat(context);
      })
      .thenValue([this](const struct stat& st) {
        return FuseDispatcher::Attr{st, computeTtlForStat(st)};
      });
}

ImmediateFuture<uint64_t> FuseDispatcherImpl::opendir(
    InodeNumber /*ino*/,
    int /*flags*/) {
#ifdef FUSE_NO_OPENDIR_SUPPORT
  if (getConnInfo().flags & FUSE_NO_OPENDIR_SUPPORT) {
    // If the kernel understands FUSE_NO_OPENDIR_SUPPORT, then returning ENOSYS
    // means that no further opendir() nor releasedir() calls will make it into
    // Eden.
    folly::throwSystemErrorExplicit(
        ENOSYS, "Eden opendir() calls are stateless and not required");
  }
#endif
  return 0ull;
}

ImmediateFuture<folly::Unit> FuseDispatcherImpl::releasedir(
    InodeNumber /*ino*/,
    uint64_t /*fh*/) {
  return folly::unit;
}

ImmediateFuture<fuse_entry_out> FuseDispatcherImpl::lookup(
    uint64_t /*requestID*/,
    InodeNumber parent,
    PathComponentPiece namepiece,
    const ObjectFetchContextPtr& context) {
  return mount_->getServerState()
      ->getFaultInjector()
      .checkAsync("FuseDispatcherImpl::lookup", namepiece)
      .thenValue([this, parent, context = context.copy()](auto&&) {
        return inodeMap_->lookupTreeInode(parent, context);
      })
      .thenTry([name = PathComponent{namepiece}, context = context.copy()](
                   folly::Try<TreeInodePtr> tree) mutable {
        // The name is built once and moved along with the child lookup result.
        auto child = makeImmediateFutureWith([&] {
          return tree.value()->getOrLoadChildIfExists(name, context);
        });
        return std::move(child).thenTry(
            [name = std::move(name)](folly::Try<InodePtr> inode) mutable {
              return std::make_pair(std::move(inode), std::move(name));
            });
      })
      .thenValue(
          [this, context = context.copy()](
              std::pair<folly::Try<InodePtr>, PathComponent> child)
              -> ImmediateFuture<fuse_entry_out> {
            auto& maybeInode = child.first;
            const auto& name = child.second;
            if (auto* err =
                    maybeInode.tryGetExceptionObject<std::system_error>()) {
              if (isEnoent(*err)) {
                // A missing name is normally answered above without an error;
                // this covers the paths that still report ENOENT, such as a
                // restricted directory or the .eden entry.
                //
                // Note: if this negative dcache entry becomes incorrect for a
                // name that is still returned by readdir, the kernel will stop
                // asking EdenFS for that name and return ENOENT directly to
                // user-space `stat`; `ls -l` will then show "?????????" because
                // the name is visible but its metadata cannot be refreshed.
                //
                // This has happened in production, may be caused by a race, and
                // is tricky to reproduce. A shorter TTL mitigates the issue by
                // reducing how long a negative dcache entry can remain
                // incorrect.
                //
                // Example report: https://fburl.com/workplace/329ni6ek
                return negativeLookupReply(name);
              }
            }
            if (maybeInode.hasException()) {
              return ImmediateFuture<fuse_entry_out>{folly::Try<fuse_entry_out>{
                  std::move(maybeInode).exception()}};
            }
            const InodePtr& inode = maybeInode.value();
            if (!inode) {
              return negativeLookupReply(name);
            }
            inode->markGcRef();
            return makeImmediateFutureWith(
                       [&]() { return inode->stat(context); })
                .thenTry([this, inode](folly::Try<struct stat> maybeStat) {
                  if (maybeStat.hasValue()) {
                    inode->incFsRefcount();
                    const auto& st = maybeStat.value();
                    return computeEntryParam(
                        FuseDispatcher::Attr{st, computeTtlForStat(st)});
                  } else {
                    // The most common case for stat() failing is if this file
                    // is materialized but the data for it in the overlay is
                    // missing or corrupt.  This can happen after a hard reboot
                    // where the overlay data was not synced to disk first.
                    //
                    // We intentionally want to return a result here rather than
                    // failing; otherwise we can't return the inode number to
                    // the kernel at all.  This blocks other operations on the
                    // file, like FUSE_UNLINK.  By successfully returning from
                    // the lookup we allow clients to remove this corrupt file
                    // with an unlink operation.  (Even though FUSE_UNLINK does
                    // not require the child inode number, the kernel does not
                    // appear to send a FUSE_UNLINK request to us if it could
                    // not get the child inode number first.)
                    XLOGF(
                        WARN,
                        "error getting attributes for inode {} ({}): {}",
                        inode->getNodeId(),
                        inode->getLogPath(),
                        maybeStat.exception().what());
                    inode->incFsRefcount();
                    return computeEntryParam(
                        attrForInodeWithCorruptOverlay(*inode));
                  }
                });
          });
}

ImmediateFuture<FuseDispatcher::Attr> FuseDispatcherImpl::setattr(
    InodeNumber ino,
    const fuse_setattr_in& attr,
    const ObjectFetchContextPtr& context) {
  // Even though mounts are created with the nosuid flag, explicitly disallow
  // setting suid, sgid, and sticky bits on any inodes. Together with create
  // and mknod stripping them, this lets us avoid explicitly clearing these
  // bits on write, truncate and chown, which FUSE_HANDLE_KILLPRIV and
  // FUSE_HANDLE_KILLPRIV_V2 otherwise require.
  if ((attr.valid & FATTR_MODE) &&
      (attr.mode & (S_ISUID | S_ISGID | S_ISVTX))) {
    folly::throwSystemErrorExplicit(EPERM, "Extra mode bits are disallowed");
  }

  return inodeMap_->lookupInode(ino, context)
      .thenValue([this, attr, context = context.copy()](const InodePtr& inode) {
        auto fuseTimeToTimespec = [](uint64_t time, uint64_t ntime) {
          timespec spec;
          spec.tv_sec = time;
          spec.tv_nsec = ntime;
          return spec;
        };

        auto now = mount_->getClock().getRealtime();

        DesiredMetadata desired;
        if (attr.valid & FATTR_SIZE) {
          desired.size = attr.size;
        }
        if (attr.valid & FATTR_MODE) {
          desired.mode = attr.mode;
        }
        if (attr.valid & FATTR_UID) {
          desired.uid = attr.uid;
        }
        if (attr.valid & FATTR_GID) {
          desired.gid = attr.gid;
        }
        if (attr.valid & FATTR_ATIME) {
          desired.atime = fuseTimeToTimespec(attr.atime, attr.atimensec);
        } else if (attr.valid & FATTR_ATIME_NOW) {
          desired.atime = now;
        }
        if (attr.valid & FATTR_MTIME) {
          desired.mtime = fuseTimeToTimespec(attr.mtime, attr.mtimensec);
        } else if (attr.valid & FATTR_MTIME_NOW) {
          desired.mtime = now;
        }

        return inode->setattr(desired, context);
      })
      .thenValue([](struct stat&& stat) {
        return FuseDispatcher::Attr{std::move(stat)};
      });
}

void FuseDispatcherImpl::forget(InodeNumber ino, unsigned long nlookup) {
  inodeMap_->decFsRefcount(ino, nlookup);
}

ImmediateFuture<uint64_t> FuseDispatcherImpl::open(
    InodeNumber /*ino*/,
    int /*flags*/) {
#ifdef FUSE_NO_OPEN_SUPPORT
  if (getConnInfo().flags & FUSE_NO_OPEN_SUPPORT) {
    // If the kernel understands FUSE_NO_OPEN_SUPPORT, then returning ENOSYS
    // means that no further open() nor release() calls will make it into Eden.
    folly::throwSystemErrorExplicit(
        ENOSYS, "Eden open() calls are stateless and not required");
  }
#endif
  return 0ull;
}

ImmediateFuture<folly::Unit> FuseDispatcherImpl::release(
    InodeNumber /*ino*/,
    uint64_t /*fh*/) {
  // A file handle from create is still released by the kernel even though
  // open is stateless, and the kernel ignores the reply.
  return folly::unit;
}

namespace {
/**
 * The mode bits a new file never gets: setattr refuses to set them, and the
 * kernel is told (FUSE_HANDLE_KILLPRIV_V2) that files never carry them.
 */
mode_t stripPrivilegeBits(mode_t mode) {
  return mode & ~static_cast<mode_t>(S_ISUID | S_ISGID | S_ISVTX);
}
} // namespace

ImmediateFuture<fuse_entry_out> FuseDispatcherImpl::create(
    InodeNumber parent,
    PathComponentPiece name,
    mode_t mode,
    int /*flags*/,
    const ObjectFetchContextPtr& context) {
  // force 'mode' to be regular file, in which case rdev arg to mknod is ignored
  // (and thus can be zero)
  mode = S_IFREG | stripPrivilegeBits(mode & 07777);
  return inodeMap_->lookupTreeInode(parent, context)
      .thenValue([this,
                  mode,
                  childName = PathComponent{name},
                  context = context.copy()](const TreeInodePtr& inode) {
        auto child = inode->mknod(childName, mode, 0, InvalidationRequired::No);
        FuseDispatcher::Attr attr{
            child->statNewlyCreated(*context), computeTtl()};
        // The kernel holds a reference only once the reply reaches it.
        child->incFsRefcount();
        return computeEntryParam(attr);
      });
}

ImmediateFuture<BufVec> FuseDispatcherImpl::read(
    InodeNumber ino,
    size_t size,
    off_t off,
    const ObjectFetchContextPtr& context) {
  return mount_->getServerState()
      ->getFaultInjector()
      .checkAsync("FuseDispatcherImpl::read", ino)
      .thenValue([this, ino, context = context.copy()](auto&&) {
        return inodeMap_->lookupFileInode(ino, context);
      })
      .thenValue([context = context.copy(), size, off](FileInodePtr&& inode) {
        inode->markGcRef();
        return inode->read(size, off, context)
            .thenValue([](std::tuple<BufVec, bool>&& readRes) {
              return std::get<BufVec>(std::move(readRes));
            });
      });
}

ImmediateFuture<size_t> FuseDispatcherImpl::write(
    InodeNumber ino,
    folly::StringPiece data,
    off_t off,
    const ObjectFetchContextPtr& context) {
  auto checkpoint = mount_->getServerState()->getFaultInjector().checkAsync(
      "FuseDispatcherImpl::write", ino);
  if (!checkpoint.isReady()) {
    // `data` points into the kernel request buffer, which is only valid while
    // this call runs synchronously, so a parked request must copy it now.
    return std::move(checkpoint)
        .thenValue([this, ino, context = context.copy()](auto&&) {
          return inodeMap_->lookupFileInode(ino, context);
        })
        .thenValue([copy = data.str(), off, context = context.copy()](
                       FileInodePtr&& inode) {
          return inode->write(copy, off, context);
        });
  }

  return std::move(checkpoint)
      .thenValue(
          [this, ino, data, off, context = context.copy()](
              auto&&) -> ImmediateFuture<size_t> {
            auto inodeFuture = inodeMap_->lookupFileInode(ino, context);
            if (inodeFuture.isReady() &&
                mount_->getEdenConfig()
                    ->experimentalFuseAvoidWriteCopy.getValue()) {
              return std::move(inodeFuture)
                  .thenValue([data, off, context = context.copy()](
                                 FileInodePtr&& inode) {
                    // FileInode copies data if it has to materialize
                    // asynchronously.
                    return inode->write(data, off, context);
                  });
            }

            return std::move(inodeFuture)
                .thenValue([copy = data.str(), off, context = context.copy()](
                               FileInodePtr&& inode) {
                  return inode->write(copy, off, context);
                });
          });
}

ImmediateFuture<Unit> FuseDispatcherImpl::flush(
    InodeNumber /* ino */,
    uint64_t /* lock_owner */) {
  // Return ENOSYS from flush.
  // This will cause the kernel to stop sending future flush() calls.
  return ImmediateFuture<Unit>(makeSystemErrorExplicit(ENOSYS, "flush"));
}

ImmediateFuture<folly::Unit> FuseDispatcherImpl::fallocate(
    InodeNumber ino,
    uint64_t offset,
    uint64_t length,
    const ObjectFetchContextPtr& context) {
  return inodeMap_->lookupFileInode(ino, context)
      .thenValue(
          [offset, length, context = context.copy()](FileInodePtr inode) {
            return inode->fallocate(offset, length, context);
          });
}

ImmediateFuture<folly::Unit> FuseDispatcherImpl::fsync(
    InodeNumber ino,
    bool datasync,
    const ObjectFetchContextPtr& context) {
  return inodeMap_->lookupFileInode(ino, context)
      .thenValue(
          [datasync](FileInodePtr inode) { return inode->fsync(datasync); });
}

ImmediateFuture<Unit> FuseDispatcherImpl::fsyncdir(
    InodeNumber /* ino */,
    bool /* datasync */) {
  // Return ENOSYS from fsyncdir. The kernel will stop sending them.
  //
  // In a possible future where the tree structure is stored in a SQLite
  // database, we could handle this request by waiting for SQLite's
  // write-ahead-log to be flushed.
  return ImmediateFuture<Unit>(makeSystemErrorExplicit(ENOSYS, "fsyncdir"));
}

ImmediateFuture<std::string> FuseDispatcherImpl::readlink(
    InodeNumber ino,
    bool kernelCachesReadlink,
    const ObjectFetchContextPtr& context) {
  return inodeMap_->lookupFileInode(ino, context)
      .thenValue([kernelCachesReadlink,
                  context = context.copy()](const FileInodePtr& inode) {
        inode->markGcRef();
        // Only release the symlink blob after it's loaded if we can assume the
        // FUSE will cache the result in the kernel's page cache.
        return inode->readlink(
            context,
            kernelCachesReadlink ? CacheHint::NotNeededAgain
                                 : CacheHint::LikelyNeededAgain);
      });
}

ImmediateFuture<FuseDirList> FuseDispatcherImpl::readdir(
    InodeNumber ino,
    FuseDirList&& dirList,
    off_t offset,
    uint64_t /*fh*/,
    const ObjectFetchContextPtr& context) {
  return inodeMap_->lookupTreeInode(ino, context)
      .thenValue([dirList = std::move(dirList),
                  offset,
                  context = context.copy()](TreeInodePtr inode) mutable {
        inode->markGcRef();
        return inode->fuseReaddir(std::move(dirList), offset, context);
      });
}

ImmediateFuture<fuse_entry_out> FuseDispatcherImpl::mknod(
    InodeNumber parent,
    PathComponentPiece name,
    mode_t mode,
    dev_t rdev,
    const ObjectFetchContextPtr& context) {
  return inodeMap_->lookupTreeInode(parent, context)
      .thenValue([this,
                  childName = PathComponent{name},
                  mode = S_ISDIR(mode) ? mode : stripPrivilegeBits(mode),
                  rdev,
                  context = context.copy()](const TreeInodePtr& inode) {
        auto child =
            inode->mknod(childName, mode, rdev, InvalidationRequired::No);
        FuseDispatcher::Attr attr{
            child->statNewlyCreated(*context), computeTtl()};
        // The kernel holds a reference only once the reply reaches it.
        child->incFsRefcount();
        return computeEntryParam(attr);
      });
}

ImmediateFuture<fuse_entry_out> FuseDispatcherImpl::mkdir(
    InodeNumber parent,
    PathComponentPiece name,
    mode_t mode,
    const ObjectFetchContextPtr& context) {
  return inodeMap_->lookupTreeInode(parent, context)
      .thenValue([this,
                  childName = PathComponent{name},
                  mode,
                  context = context.copy()](const TreeInodePtr& inode) {
        auto child = inode->mkdir(childName, mode, InvalidationRequired::No);
        FuseDispatcher::Attr attr{
            child->statNewlyCreated(*context), computeTtl()};
        // The kernel holds a reference only once the reply reaches it.
        child->incFsRefcount();
        return computeEntryParam(attr);
      });
}

ImmediateFuture<folly::Unit> FuseDispatcherImpl::unlink(
    InodeNumber parent,
    PathComponentPiece name,
    const ObjectFetchContextPtr& context) {
  return inodeMap_->lookupTreeInode(parent, context)
      .thenValue([context = context.copy(),
                  childName = PathComponent{name}](const TreeInodePtr& inode) {
        // No need to flush the kernel cache because FUSE will do that for us.
        return inode->unlink(childName, InvalidationRequired::No, context);
      });
}

ImmediateFuture<folly::Unit> FuseDispatcherImpl::rmdir(
    InodeNumber parent,
    PathComponentPiece name,
    const ObjectFetchContextPtr& context) {
  return inodeMap_->lookupTreeInode(parent, context)
      .thenValue([context = context.copy(),
                  childName = PathComponent{name}](const TreeInodePtr& inode) {
        // No need to flush the kernel cache because FUSE will do that for us.
        return inode->rmdir(childName, InvalidationRequired::No, context);
      });
}

ImmediateFuture<fuse_entry_out> FuseDispatcherImpl::symlink(
    InodeNumber parent,
    PathComponentPiece name,
    StringPiece link,
    const ObjectFetchContextPtr& context) {
  return inodeMap_->lookupTreeInode(parent, context)
      .thenValue([this,
                  linkContents = link.str(),
                  childName = PathComponent{name},
                  context = context.copy()](const TreeInodePtr& inode) {
        auto symlinkInode =
            inode->symlink(childName, linkContents, InvalidationRequired::No);
        FuseDispatcher::Attr attr{
            symlinkInode->statNewlyCreated(*context), computeTtl()};
        // The kernel holds a reference only once the reply reaches it.
        symlinkInode->incFsRefcount();
        return computeEntryParam(attr);
      });
}

ImmediateFuture<folly::Unit> FuseDispatcherImpl::rename(
    InodeNumber parent,
    PathComponentPiece namePiece,
    InodeNumber newParent,
    PathComponentPiece newNamePiece,
    const ObjectFetchContextPtr& context) {
  return renameImpl(parent, namePiece, newParent, newNamePiece, false, context);
}

ImmediateFuture<folly::Unit> FuseDispatcherImpl::rename2(
    InodeNumber parent,
    PathComponentPiece namePiece,
    InodeNumber newParent,
    PathComponentPiece newNamePiece,
    uint32_t flags,
    const ObjectFetchContextPtr& context) {
  if (!mount_->getEdenConfig()->experimentalFuseRenameNoReplace.getValue()) {
    // ENOSYS is deliberate: the kernel then stops sending FUSE_RENAME2 for
    // the life of the mount and fails flagged renames with EINVAL itself,
    // which is what callers expect from a filesystem without flag support.
    // Enabling the setting takes effect on the next mount.
    FUSELL_NOT_IMPL();
  }

  constexpr uint32_t kRenameNoReplace = 1;
  if ((flags & ~kRenameNoReplace) != 0) {
    folly::throwSystemErrorExplicit(EINVAL, "unsupported FUSE_RENAME2 flags");
  }

  return renameImpl(
      parent,
      namePiece,
      newParent,
      newNamePiece,
      (flags & kRenameNoReplace) != 0,
      context);
}

ImmediateFuture<folly::Unit> FuseDispatcherImpl::renameImpl(
    InodeNumber parent,
    PathComponentPiece namePiece,
    InodeNumber newParent,
    PathComponentPiece newNamePiece,
    bool noReplace,
    const ObjectFetchContextPtr& context) {
  // Start looking up both parents
  auto parentFuture = inodeMap_->lookupTreeInode(parent, context);
  auto newParentFuture = inodeMap_->lookupTreeInode(newParent, context);
  // Do the rename once we have looked up both parents.
  return std::move(parentFuture)
      .thenValue(
          [npFuture = std::move(newParentFuture),
           name = PathComponent{namePiece},
           newName = PathComponent{newNamePiece},
           noReplace,
           context = context.copy()](const TreeInodePtr& parent) mutable {
            return std::move(npFuture).thenValue(
                [parent, name, newName, noReplace, context = context.copy()](
                    const TreeInodePtr& newParent) {
                  return parent->rename(
                      name,
                      newParent,
                      newName,
                      InvalidationRequired::No,
                      context,
                      noReplace);
                });
          });
}

ImmediateFuture<fuse_entry_out> FuseDispatcherImpl::link(
    InodeNumber /*ino*/,
    InodeNumber /*newParent*/,
    PathComponentPiece newName) {
  validatePathComponentLength(newName);

  // We intentionally do not support hard links.
  // These generally cannot be tracked in source control (git or mercurial)
  // and are not portable to non-Unix platforms.
  folly::throwSystemErrorExplicit(
      EPERM, "hard links are not supported in eden mount points");
}

ImmediateFuture<string> FuseDispatcherImpl::getxattr(
    InodeNumber ino,
    StringPiece name,
    const ObjectFetchContextPtr& context) {
  return inodeMap_->lookupInode(ino, context)
      .thenValue([attrName = name.str(),
                  context = context.copy()](const InodePtr& inode) {
        inode->markGcRef();
        return inode->getxattr(attrName, context);
      });
}

ImmediateFuture<vector<string>> FuseDispatcherImpl::listxattr(
    InodeNumber ino,
    const ObjectFetchContextPtr& context) {
  return inodeMap_->lookupInode(ino, context)
      .thenValue([](const InodePtr& inode) {
        inode->markGcRef();
        return inode->listxattr();
      });
}

ImmediateFuture<struct fuse_kstatfs> FuseDispatcherImpl::statfs(
    InodeNumber /*ino*/) {
  struct fuse_kstatfs info = {};

  // Pass through the overlay free space stats; this gives a more reasonable
  // estimation of available storage space than the zeroes that we'd report
  // otherwise.  This is important because eg: Finder on macOS inspects disk
  // space prior to initiating a copy and will refuse to start a copy if
  // the disk appears to be full.
  // The only exception is bfree as users are misinterpreting the data and
  // think that EdenFS is consumming 100+ GB of disk space. We thus expose that
  // EdenFS free space is the same as the overlay capacity.
  auto overlayStats = mount_->getOverlay()->statFs();
  info.blocks = overlayStats.f_blocks;
  info.bfree = overlayStats.f_blocks;
  info.bavail = overlayStats.f_bavail;
  info.files = overlayStats.f_files;
  info.ffree = overlayStats.f_ffree;

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
} // namespace facebook::eden

#endif
