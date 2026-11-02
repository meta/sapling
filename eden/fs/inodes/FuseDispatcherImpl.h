/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include "eden/fs/fuse/FuseDispatcher.h"

namespace facebook::eden {

class EdenMount;
class InodeMap;

/**
 * Implement the FuseDispatcher interface.
 *
 * For unsupported operations, the corresponding methods are explicitly not
 * overridden and will directly fail in FuseDispatcher.
 */
class FuseDispatcherImpl : public FuseDispatcher {
 public:
  explicit FuseDispatcherImpl(EdenMount* mount);

  ImmediateFuture<struct fuse_kstatfs> statfs(InodeNumber ino) override;
  ImmediateFuture<Attr> getattr(
      InodeNumber ino,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<Attr> setattr(
      InodeNumber ino,
      const fuse_setattr_in& attr,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<uint64_t> opendir(InodeNumber ino, int flags) override;
  ImmediateFuture<folly::Unit> releasedir(InodeNumber ino, uint64_t fh)
      override;
  ImmediateFuture<fuse_entry_out> lookup(
      uint64_t requestID,
      InodeNumber parent,
      PathComponentPiece name,
      const ObjectFetchContextPtr& context) override;

  void forget(InodeNumber ino, unsigned long nlookup) override;
  ImmediateFuture<uint64_t> open(InodeNumber ino, int flags) override;
  ImmediateFuture<folly::Unit> release(InodeNumber ino, uint64_t fh) override;
  ImmediateFuture<std::string> readlink(
      InodeNumber ino,
      bool kernelCachesReadlink,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<fuse_entry_out> mknod(
      InodeNumber parent,
      PathComponentPiece name,
      mode_t mode,
      dev_t rdev,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<fuse_entry_out> mkdir(
      InodeNumber parent,
      PathComponentPiece name,
      mode_t mode,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<folly::Unit> unlink(
      InodeNumber parent,
      PathComponentPiece name,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<folly::Unit> rmdir(
      InodeNumber parent,
      PathComponentPiece name,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<fuse_entry_out> symlink(
      InodeNumber parent,
      PathComponentPiece name,
      folly::StringPiece link,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<folly::Unit> rename(
      InodeNumber parent,
      PathComponentPiece name,
      InodeNumber newparent,
      PathComponentPiece newname,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<folly::Unit> rename2(
      InodeNumber parent,
      PathComponentPiece name,
      InodeNumber newparent,
      PathComponentPiece newname,
      uint32_t flags,
      const ObjectFetchContextPtr& context) override;

  ImmediateFuture<fuse_entry_out> link(
      InodeNumber ino,
      InodeNumber newparent,
      PathComponentPiece newname) override;

  ImmediateFuture<fuse_entry_out> create(
      InodeNumber parent,
      PathComponentPiece name,
      mode_t mode,
      int flags,
      const ObjectFetchContextPtr& context) override;

  ImmediateFuture<BufVec> read(
      InodeNumber ino,
      size_t size,
      off_t off,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<size_t> write(
      InodeNumber ino,
      folly::StringPiece data,
      off_t off,
      const ObjectFetchContextPtr& context) override;

  ImmediateFuture<folly::Unit> flush(InodeNumber ino, uint64_t lock_owner)
      override;
  ImmediateFuture<folly::Unit> fallocate(
      InodeNumber ino,
      uint64_t offset,
      uint64_t length,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<folly::Unit> fsync(
      InodeNumber ino,
      bool datasync,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<folly::Unit> fsyncdir(InodeNumber ino, bool datasync)
      override;

  ImmediateFuture<FuseDirList> readdir(
      InodeNumber ino,
      FuseDirList&& dirList,
      off_t offset,
      uint64_t fh,
      const ObjectFetchContextPtr& context) override;

  ImmediateFuture<std::string> getxattr(
      InodeNumber ino,
      folly::StringPiece name,
      const ObjectFetchContextPtr& context) override;
  ImmediateFuture<std::vector<std::string>> listxattr(
      InodeNumber ino,
      const ObjectFetchContextPtr& context) override;

 private:
  ImmediateFuture<folly::Unit> renameImpl(
      InodeNumber parent,
      PathComponentPiece name,
      InodeNumber newparent,
      PathComponentPiece newname,
      bool noReplace,
      const ObjectFetchContextPtr& context);

  /**
   * Compute the FUSE cache TTL based on current inode pressure.
   * Returns dynamic TTL when pressure-based GC is enabled, or the
   * legacy infinite TTL otherwise.
   */
  uint64_t computeTtl() const;

  /**
   * Compute the FUSE cache TTL for the 000-mode stat returned for restricted
   * source-control ACL directories.
   */
  uint64_t computeRestrictedTreeTtl() const;

  /**
   * Compute the FUSE cache TTL for a specific stat result.
   */
  uint64_t computeTtlForStat(const struct stat& st) const;

  /**
   * Compute the TTL for negative dentry cache entries returned from ENOENT
   * lookups.
   */
  uint64_t computeNegativeEntryTtl() const;

  /**
   * The reply for a name that does not exist: inode number 0 with the
   * negative-entry TTL, which the kernel caches as a negative dentry.
   */
  fuse_entry_out negativeLookupEntry() const;

  /**
   * The reply for every lookup that resolves to a missing name. Runs the
   * FuseDispatcherImpl::negativeLookup fault checkpoint, keyed by `name`,
   * after ENOENT is decided and before the kernel caches the negative entry,
   * then returns negativeLookupEntry().
   */
  ImmediateFuture<fuse_entry_out> negativeLookupReply(
      PathComponentPiece name) const;

  // The EdenMount associated with this dispatcher.
  EdenMount* const mount_;

  // The EdenMount's InodeMap.
  // We store this pointer purely for convenience. We need it on pretty much
  // every FUSE request, and having it locally avoids having to dereference
  // mount_ first.
  InodeMap* const inodeMap_;
};

} // namespace facebook::eden
