/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <atomic>

#include <folly/Portability.h>
#include <folly/Range.h>

#include "eden/common/utils/ImmediateFuture.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/fs/inodes/InodeNumber.h"
#include "eden/fs/store/IObjectStore.h"
#include "eden/fs/utils/BufVec.h"
#include "eden/fs/utils/FsChannelTypes.h"

#ifndef _WIN32
#include <sys/statvfs.h>
#endif

namespace facebook::eden {

#ifndef _WIN32

#define FUSELL_NOT_IMPL()                                         \
  do {                                                            \
    static std::atomic<bool> logged{false};                       \
    bool expected = false;                                        \
    if (logged.compare_exchange_strong(                           \
            expected, true, std::memory_order_relaxed)) {         \
      LOG(WARNING) << __PRETTY_FUNCTION__ << " not implemented";  \
    }                                                             \
    folly::throwSystemErrorExplicit(ENOSYS, __PRETTY_FUNCTION__); \
  } while (0)

class FuseDirList;
class EdenStats;

using EdenStatsPtr = RefPtr<EdenStats>;

class FuseDispatcher {
  fuse_init_out connInfo_{};
  EdenStatsPtr stats_;

 public:
  virtual ~FuseDispatcher();

  explicit FuseDispatcher(EdenStatsPtr stats);
  const EdenStatsPtr& getStats() const;

  const fuse_init_out& getConnInfo() const;

  /**
   * Called during filesystem mounting.  It informs the filesystem
   * of kernel capabilities and provides an opportunity to poke some
   * flags and limits in the conn_info to report capabilities back
   * to the kernel
   */
  virtual void initConnection(const fuse_init_out& out);

  /**
   * Called when fuse is tearing down the session
   */
  virtual void destroy();

  /**
   * Lookup a directory entry by name and get its attributes.
   *
   * requestID is given here to assert invariants in tests.
   */
  virtual ImmediateFuture<fuse_entry_out> lookup(
      uint64_t requestID,
      InodeNumber parent,
      PathComponentPiece name,
      const ObjectFetchContextPtr& context);

  /**
   * Forget about an inode
   *
   * The nlookup parameter indicates the number of lookups
   * previously performed on this inode.
   *
   * If the filesystem implements inode lifetimes, it is recommended
   * that inodes acquire a single reference on each lookup, and lose
   * nlookup references on each forget.
   *
   * The filesystem may ignore forget calls, if the inodes don't
   * need to have a limited lifetime.
   *
   * On unmount, it is not guaranteed that all referenced inodes
   * will receive a forget message.
   *
   * @param ino the inode number
   * @param nlookup the number of lookups to forget
   */
  virtual void forget(InodeNumber ino, unsigned long nlookup);

  /**
   * The stat information and the cache TTL for the kernel
   *
   * The timeout value is measured in seconds and indicates how long
   * the kernel side of the FUSE will cache the values in the
   * struct stat before calling getattr() again to refresh it.
   */
  struct Attr {
    struct stat st;
    uint64_t timeout_seconds;

    explicit Attr(
        const struct stat& st,
        // We want an ostensibly infinite TTL for the attributes
        // we send to the kernel, but need to take care as the
        // macOS fuse kext implementation casts this to a signed
        // value and adds it to another timespec to compute the
        // absolute deadline.  If we make the value the maximum
        // possible unsigned 64 bit value the deadline overflows
        // and we never achieve a cache hit.  Limiting ourselves
        // to the maximum possible signed 32 bit value gives us
        // ta large and effective timeout
        uint64_t timeout = std::numeric_limits<int32_t>::max());

    fuse_attr_out asFuseAttr() const;
  };

  /**
   * Get file attributes
   *
   * @param ino the inode number
   */
  virtual ImmediateFuture<Attr> getattr(
      InodeNumber ino,
      const ObjectFetchContextPtr& context);

  /**
   * Set file attributes
   *
   * In the 'attr' argument only members indicated by the 'to_set'
   * bitmask contain valid values.  Other members contain undefined
   * values.
   *
   * @param ino the inode number
   * @param attr the attributes
   * @param to_set bit mask of attributes which should be set
   *
   * Changed in version 2.5:
   *     file information filled in for ftruncate
   */
  virtual ImmediateFuture<Attr> setattr(
      InodeNumber ino,
      const fuse_setattr_in& attr,
      const ObjectFetchContextPtr& context);

  /**
   * Read symbolic link
   *
   * @param ino the inode number
   * @param kernelCachesReadlink whether the kernel supports caching readlink
   * calls.
   */
  virtual ImmediateFuture<std::string> readlink(
      InodeNumber ino,
      bool kernelCachesReadlink,
      const ObjectFetchContextPtr& context);

  /**
   * Create file node
   *
   * Create a regular file, character device, block device, fifo or
   * socket node.
   *
   * @param parent inode number of the parent directory
   * @param name to create
   * @param mode file type and mode with which to create the new file
   * @param rdev the device number (only valid if created file is a device)
   */
  virtual ImmediateFuture<fuse_entry_out> mknod(
      InodeNumber parent,
      PathComponentPiece name,
      mode_t mode,
      dev_t rdev,
      const ObjectFetchContextPtr& context);

  /**
   * Create a directory
   *
   * @param parent inode number of the parent directory
   * @param name to create
   * @param mode with which to create the new file
   */
  virtual ImmediateFuture<fuse_entry_out> mkdir(
      InodeNumber parent,
      PathComponentPiece name,
      mode_t mode,
      const ObjectFetchContextPtr& context);

  /**
   * Remove a file
   *
   * @param parent inode number of the parent directory
   * @param name to remove
   */
  [[nodiscard]] virtual ImmediateFuture<folly::Unit> unlink(
      InodeNumber parent,
      PathComponentPiece name,
      const ObjectFetchContextPtr& context);

  /**
   * Remove a directory
   *
   * @param parent inode number of the parent directory
   * @param name to remove
   */
  [[nodiscard]] virtual ImmediateFuture<folly::Unit> rmdir(
      InodeNumber parent,
      PathComponentPiece name,
      const ObjectFetchContextPtr& context);

  /**
   * Create a symbolic link
   *
   * @param parent inode number of the parent directory
   * @param name to create
   * @param link the contents of the symbolic link
   */
  virtual ImmediateFuture<fuse_entry_out> symlink(
      InodeNumber parent,
      PathComponentPiece name,
      folly::StringPiece link,
      const ObjectFetchContextPtr& context);

  /**
   * Rename a file
   *
   * @param parent inode number of the old parent directory
   * @param name old name
   * @param newparent inode number of the new parent directory
   * @param newname new name
   */
  [[nodiscard]] virtual ImmediateFuture<folly::Unit> rename(
      InodeNumber parent,
      PathComponentPiece name,
      InodeNumber newparent,
      PathComponentPiece newname,
      const ObjectFetchContextPtr& context);

  [[nodiscard]] virtual ImmediateFuture<folly::Unit> rename2(
      InodeNumber parent,
      PathComponentPiece name,
      InodeNumber newparent,
      PathComponentPiece newname,
      uint32_t flags,
      const ObjectFetchContextPtr& context);

  /**
   * Create a hard link
   *
   * @param ino the old inode number
   * @param newparent inode number of the new parent directory
   * @param newname new name to create
   */
  virtual ImmediateFuture<fuse_entry_out>
  link(InodeNumber ino, InodeNumber newparent, PathComponentPiece newname);

  /**
   * Open a file
   *
   * open(2) flags (with the exception of O_CREAT, O_EXCL, O_NOCTTY and
   * O_TRUNC) are available in the flags parameter.
   *
   * The returned fh value will be passed to release.
   */
  virtual ImmediateFuture<uint64_t> open(InodeNumber ino, int flags);

  /**
   * Release an open file
   *
   * Release is called when there are no more references to an open file: all
   * file descriptors are closed and all memory mappings are unmapped.
   *
   * For every open call there will be exactly one release call.
   *
   * The filesystem may reply with an error, but error values are not returned
   * to close() or munmap() which triggered the release.
   *
   * fh will contain the value returned by the open method.
   */
  virtual ImmediateFuture<folly::Unit> release(InodeNumber ino, uint64_t fh);

  /**
   * Open a directory
   *
   * open(2) flags are available in the flags parameter.
   *
   * The return value will be given to releasedir and readdir.
   */
  virtual ImmediateFuture<uint64_t> opendir(InodeNumber ino, int flags);

  /**
   * Release an open directory
   *
   * For every opendir call there will be exactly one releasedir call. (Except
   * during unmount - further releasedir calls are not sent.) The fh parameter
   * contains the result of opendir.
   */
  virtual ImmediateFuture<folly::Unit> releasedir(InodeNumber ino, uint64_t fh);

  /**
   * Read data
   *
   * Read should send exactly the number of bytes requested except
   * on EOF or error, otherwise the rest of the data will be
   * substituted with zeroes.  An exception to this is when the file
   * has been opened in 'direct_io' mode, in which case the return
   * value of the read system call will reflect the return value of
   * this operation.
   *
   * @param size number of bytes to read
   * @param off offset to read from
   */
  virtual ImmediateFuture<BufVec> read(
      InodeNumber ino,
      size_t size,
      off_t off,
      const ObjectFetchContextPtr& context);

  /**
   * Write data
   *
   * Write should return exactly the number of bytes requested
   * except on error.  An exception to this is when the file has
   * been opened in 'direct_io' mode, in which case the return value
   * of the write system call will reflect the return value of this
   * operation.
   */
  [[nodiscard]] virtual ImmediateFuture<size_t> write(
      InodeNumber ino,
      folly::StringPiece data,
      off_t off,
      const ObjectFetchContextPtr& context);

  /**
   * This is called on each close() of the opened file.
   *
   * Since file descriptors can be duplicated (dup, dup2, fork), for
   * one open call there may be many flush calls.
   *
   * Filesystems shouldn't assume that flush will always be called
   * after some writes, or that if will be called at all.
   *
   * NOTE: the name of the method is misleading, since (unlike
   * fsync) the filesystem is not forced to flush pending writes.
   * One reason to flush data, is if the filesystem wants to return
   * write errors.
   *
   * If the filesystem supports file locking operations (setlk,
   * getlk) it should remove all locks belonging to 'lock_owner'.
   */
  [[nodiscard]] virtual ImmediateFuture<folly::Unit> flush(
      InodeNumber ino,
      uint64_t lock_owner);

  /**
   * Provide an approximate implementation of fallocate(2) with mode=0 or
   * posix_fallocate. This is not generalized to all fallocate(2) modes, but
   * could be done so in the future if necessary.
   *
   * Only used on Linux.
   */
  [[nodiscard]] virtual ImmediateFuture<folly::Unit> fallocate(
      InodeNumber ino,
      uint64_t offset,
      uint64_t length,
      const ObjectFetchContextPtr& context);

  /**
   * Ensure file content changes are flushed to disk.
   *
   * If the datasync parameter is true, then only the user data should be
   * flushed, not the meta data.
   */
  [[nodiscard]] virtual ImmediateFuture<folly::Unit>
  fsync(InodeNumber ino, bool datasync, const ObjectFetchContextPtr& context);

  /**
   * Ensure directory content changes are flushed to disk.
   *
   * If the datasync parameter is true, then only the directory contents should
   * be flushed, not the aux data.
   */
  [[nodiscard]] virtual ImmediateFuture<folly::Unit> fsyncdir(
      InodeNumber ino,
      bool datasync);

  /**
   * Read directory.
   *
   * Send a FuseDirList filled using FuseDirList::add().
   * Send an empty FuseDirList on end of stream.
   *
   * The fh parameter contains opendir's result.
   */
  virtual ImmediateFuture<FuseDirList> readdir(
      InodeNumber ino,
      FuseDirList&& dirList,
      off_t offset,
      uint64_t fh,
      const ObjectFetchContextPtr& context);

  /**
   * Get file system statistics
   *
   * @param ino the inode number, zero means "undefined"
   */
  virtual ImmediateFuture<struct fuse_kstatfs> statfs(InodeNumber ino);

  /**
   * Set an extended attribute
   */
  [[nodiscard]] virtual ImmediateFuture<folly::Unit> setxattr(
      InodeNumber ino,
      folly::StringPiece name,
      folly::StringPiece value,
      int flags);
  /**
   * Get an extended attribute
   */
  virtual ImmediateFuture<std::string> getxattr(
      InodeNumber ino,
      folly::StringPiece name,
      const ObjectFetchContextPtr& context);
  static const int kENOATTR;

  /**
   * List extended attribute names
   */
  virtual ImmediateFuture<std::vector<std::string>> listxattr(
      InodeNumber ino,
      const ObjectFetchContextPtr& context);

  /**
   * Remove an extended attribute
   *
   * @param ino the inode number
   * @param name of the extended attribute
   */
  [[nodiscard]] virtual ImmediateFuture<folly::Unit> removexattr(
      InodeNumber ino,
      folly::StringPiece name);

  /**
   * Check file access permissions
   *
   * This will be called for the access() system call.  If the
   * 'default_permissions' mount option is given, this method is not
   * called.
   *
   * This method is not called under Linux kernel versions 2.4.x
   *
   * Introduced in version 2.5
   *
   * @param ino the inode number
   * @param mask requested access mode
   */
  [[nodiscard]] virtual ImmediateFuture<folly::Unit> access(
      InodeNumber ino,
      int mask);

  /**
   * Create and open a file
   *
   * If the file does not exist, first create it with the specified
   * mode, and then open it.
   *
   * Open flags (with the exception of O_NOCTTY) are available in
   * fi->flags.
   *
   * If this method is not implemented or under Linux kernel
   * versions earlier than 2.6.15, the mknod() and open() methods
   * will be called instead.
   *
   * Introduced in version 2.5
   *
   * @param parent inode number of the parent directory
   * @param name to create
   * @param mode file type and mode with which to create the new file
   */
  virtual ImmediateFuture<fuse_entry_out> create(
      InodeNumber parent,
      PathComponentPiece name,
      mode_t mode,
      int flags,
      const ObjectFetchContextPtr& context);

  /**
   * Map block index within file to block index within device
   *
   * Note: This makes sense only for block device backed filesystems
   * mounted with the 'blkdev' option
   *
   * Introduced in version 2.6
   *
   * @param ino the inode number
   * @param blocksize unit of block index
   * @param idx block index within file
   */
  virtual ImmediateFuture<uint64_t>
  bmap(InodeNumber ino, size_t blocksize, uint64_t idx);
};

#endif

} // namespace facebook::eden
