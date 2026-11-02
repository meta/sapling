/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include "eden/common/utils/FileOffset.h"
#include "eden/common/utils/ImmediateFuture.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/fs/inodes/InodeMetadata.h"
#include "eden/fs/inodes/InodeNumber.h"
#include "eden/fs/nfs/NfsDirList.h"
#include "eden/fs/store/ObjectFetchContext.h"

#include <optional>

#include <sys/stat.h>

#ifdef __APPLE__
#include <sys/mount.h>
#include <sys/param.h>
#elif __linux__
#include <sys/vfs.h>
#endif

namespace folly {
template <class T>
class Future;
}

namespace facebook::eden {

class EdenStats;
class Clock;

using EdenStatsPtr = RefPtr<EdenStats>;

class NfsDispatcher {
 public:
  explicit NfsDispatcher(EdenStatsPtr stats, const Clock& clock);

  virtual ~NfsDispatcher();

  const EdenStatsPtr& getStats() const {
    return stats_;
  }

  const Clock& getClock() const {
    return clock_;
  }

  /**
   * Get file attribute for the passed in InodeNumber.
   */
  virtual ImmediateFuture<struct stat> getattr(
      InodeNumber ino,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * Return value of the setattr method.
   */
  struct SetattrRes {
    /** Attributes of the file prior to changing its attributes */
    std::optional<struct stat> preStat;
    /** Attributes of the file after changing its attributes */
    std::optional<struct stat> postStat;
    /**
     * Whether the request asked only for attributes the file already had.
     * Such a request changes nothing and is what the chmod EdenFS issues to
     * invalidate the NFS client's cache of a directory looks like.
     */
    bool noop{false};
  };

  /**
   * Change the attributes of the file referenced by the InodeNumber ino.
   *
   * See comment on the create method for the meaning of the returned pre and
   * post stat.
   */
  virtual ImmediateFuture<SetattrRes> setattr(
      InodeNumber ino,
      DesiredMetadata desired,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * Racily obtain the parent directory of the passed in directory.
   *
   * Can be used to handle a ".." filename.
   */
  virtual ImmediateFuture<InodeNumber> getParent(
      InodeNumber ino,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * Record a reference to the inode for inode garbage collection. Parents
   * won't be updated.
   */
  virtual ImmediateFuture<folly::Unit> markGcRef(
      InodeNumber ino,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * Find the given name in the passed in directory. Its InodeNumber and
   * attributes are returned, or nullopt when the directory has no such entry.
   * Clients look a name up before every create and mkdir, so a missing name
   * is the common outcome and is a value rather than an ENOENT error.
   */
  virtual ImmediateFuture<std::optional<std::tuple<InodeNumber, struct stat>>>
  lookup(
      InodeNumber dir,
      PathComponent name,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * For a symlink, return its destination, fail otherwise.
   */
  virtual ImmediateFuture<std::string> readlink(
      InodeNumber ino,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * Return value of the read method.
   */
  struct ReadRes {
    /** Data successfully read */
    std::unique_ptr<folly::IOBuf> data;
    /** Has the read reached the end of file */
    bool isEof;
  };

  /**
   * Read data from the file referenced by the InodeNumber ino.
   */
  virtual ImmediateFuture<ReadRes> read(
      InodeNumber ino,
      size_t size,
      FileOffset offset,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * Return value of the write method.
   */
  struct WriteRes {
    /** Number of bytes written */
    size_t written;

    /** Attributes of the directory prior to creating the file */
    std::optional<struct stat> preStat;
    /** Attributes of the directory after creating the file */
    std::optional<struct stat> postStat;
  };

  /**
   * Write data at offset to the file referenced by the InodeNumber ino.
   *
   * See the comment on the create method below for the meaning of the returned
   * pre and post stat.
   */
  virtual ImmediateFuture<WriteRes> write(
      InodeNumber ino,
      std::unique_ptr<folly::IOBuf> data,
      FileOffset offset,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * Return value of the create method.
   */
  struct CreateRes {
    /** InodeNumber of the created file */
    InodeNumber ino;
    /** Attributes of the created file */
    struct stat stat;

    /** Attributes of the directory prior to creating the file */
    std::optional<struct stat> preDirStat;
    /** Attributes of the directory after creating the file */
    std::optional<struct stat> postDirStat;
  };

  /**
   * Create a regular file in the directory referenced by the InodeNumber dir.
   *
   * Both the pre and post stat for that directory needs to be collected in an
   * atomic manner: no other operation on the directory needs to be allowed in
   * between them. This is to ensure that the NFS client can properly detect if
   * its cache needs to be invalidated. Setting them both to std::nullopt is an
   * acceptable approach if the stat cannot be collected atomically.
   */
  virtual ImmediateFuture<CreateRes> create(
      InodeNumber dir,
      PathComponent name,
      mode_t mode,
      createhow3 how,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * Return value of the mkdir method.
   */
  struct MkdirRes {
    /** InodeNumber of the created directory */
    InodeNumber ino;
    /** Attributes of the created directory */
    struct stat stat;

    /** Attributes of the directory prior to creating the subdirectory */
    std::optional<struct stat> preDirStat;
    /** Attributes of the directory after creating the subdirectory */
    std::optional<struct stat> postDirStat;
  };

  /**
   * Create a subdirectory in the directory referenced by the InodeNumber dir.
   *
   * For the pre and post dir stat, refer to the documentation of the create
   * method above.
   */
  virtual ImmediateFuture<MkdirRes> mkdir(
      InodeNumber dir,
      PathComponent name,
      mode_t mode,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * Return value of the symlink method.
   */
  struct SymlinkRes {
    /** InodeNumber of the created symlink */
    InodeNumber ino;
    /** Attributes of the created symlink */
    struct stat stat;

    /** Attributes of the directory prior to creating the symlink */
    std::optional<struct stat> preDirStat;
    /** Attributes of the directory after creating the symlink */
    std::optional<struct stat> postDirStat;
  };

  /**
   * Add a symlink in the directory referenced by the InodeNumber dir. The
   * symlink will have the name passed in, and will store data. From EdenFS
   * perspective the data is an opaque value that will be interpreted by the
   * client.
   *
   * For the pre and post dir stat, refer to the documentation of the create
   * method above.
   */
  virtual ImmediateFuture<SymlinkRes> symlink(
      InodeNumber dir,
      PathComponent name,
      std::string data,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * Return value of the mknod method.
   */
  struct MknodRes {
    /** InodeNumber of the created special file */
    InodeNumber ino;
    /** Attributes of the created special file */
    struct stat stat;

    /** Attributes of the directory prior to creating the special file */
    std::optional<struct stat> preDirStat;
    /** Attributes of the directory after creating the special file */
    std::optional<struct stat> postDirStat;
  };

  /**
   * Create a special file in the directory referenced by the InodeNumber dir.
   * The special file will have the name passed in.
   *
   * For the pre and post dir stat, refer to the documentation of the create
   * method above.
   */
  virtual ImmediateFuture<MknodRes> mknod(
      InodeNumber ino,
      PathComponent name,
      mode_t mode,
      dev_t rdev,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * Return value of the unlink method.
   */
  struct UnlinkRes {
    /** Attributes of the directory prior to removing the file */
    std::optional<struct stat> preDirStat;
    /** Attributes of the directory after removing the file */
    std::optional<struct stat> postDirStat;
  };

  /**
   * Remove the file/directory name from the directory referenced by the
   * InodeNumber dir.
   *
   * For the pre and post dir stat, refer to the documentation of the create
   * method above.
   */
  virtual ImmediateFuture<UnlinkRes> unlink(
      InodeNumber dir,
      PathComponent name,
      const ObjectFetchContextPtr& context) = 0;

  struct RmdirRes {
    /** Attributes of the directory prior to removing the directory */
    std::optional<struct stat> preDirStat;
    /** Attributes of the directory after removing the directory */
    std::optional<struct stat> postDirStat;
  };

  /**
   * Remove the directory name from the directory referenced by the InodeNumber
   * dir.
   *
   * For the pre and post dir stat, refer to the documentation of the create
   * method above.
   */
  virtual ImmediateFuture<RmdirRes> rmdir(
      InodeNumber dir,
      PathComponent name,
      const ObjectFetchContextPtr& context) = 0;

  struct RenameRes {
    /** Attributes of the from directory prior to renaming the file. */
    std::optional<struct stat> fromPreDirStat;
    /** Attributes of the from directory after renaming the file. */
    std::optional<struct stat> fromPostDirStat;
    /** Attributes of the to directory prior to renaming the file. */
    std::optional<struct stat> toPreDirStat;
    /** Attributes of the to directory after renaming the file. */
    std::optional<struct stat> toPostDirStat;
  };

  /**
   * Rename a file/directory from the directory referenced by fromIno to the
   * directory referenced by toIno. The file/directory fromName will be renamed
   * onto toName.
   *
   * For the pre and post dir stat, refer to the documentation of the create
   * method above.
   */
  virtual ImmediateFuture<RenameRes> rename(
      InodeNumber fromIno,
      PathComponent fromName,
      InodeNumber toIno,
      PathComponent toName,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * Return value of the readdir method.
   */
  struct ReaddirRes {
    /** List of directory entries */
    NfsDirList entries;
    /** Has the readdir reached the end of the directory */
    bool isEof;
  };

  /**
   * Read the content of the directory referenced by the InodeNumber dir. A
   * maximum of count bytes will be added to the returned NfsDirList.
   *
   * For very large directories, it is possible that more than count bytes are
   * necessary to return all the directory entries. In this case, a subsequent
   * readdir call will be made by the NFS client to restart the enumeration at
   * offset. The first readdir will have an offset of 0.
   */
  virtual ImmediateFuture<ReaddirRes> readdir(
      InodeNumber dir,
      FileOffset offset,
      uint32_t count,
      const ObjectFetchContextPtr& context) = 0;

  /**
   * Variant of readdir that reads the content of the directory referenced by
   * the InodeNumber dir and also reads stat data for each file. As with
   * readdir, a maximum of count bytes will be added to the returned NfsDirList.
   *
   * Readdirplus behaves similarly to readdir for very large directories. See
   * the comment above for more info.
   *
   */
  virtual ImmediateFuture<ReaddirRes> readdirplus(
      InodeNumber dir,
      FileOffset offset,
      uint32_t count,
      const ObjectFetchContextPtr& context) = 0;

  virtual ImmediateFuture<struct statfs> statfs(
      InodeNumber dir,
      const ObjectFetchContextPtr& context) = 0;

 private:
  EdenStatsPtr stats_;
  const Clock& clock_;
};

} // namespace facebook::eden
