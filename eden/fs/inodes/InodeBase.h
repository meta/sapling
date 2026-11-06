/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/Synchronized.h>
#include <folly/coro/safe/NowTask.h>
#include <folly/futures/Future.h>
#include <atomic>
#include <memory>
#include <optional>
#include <vector>

#include "eden/common/utils/DirType.h"
#include "eden/common/utils/ImmediateFuture.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/fs/inodes/InodeMetadata.h"
#include "eden/fs/inodes/InodeNumber.h"
#include "eden/fs/inodes/InodePtr.h"
#include "eden/fs/inodes/InodeTimestamps.h"
#include "eden/fs/model/ObjectId.h"

#ifndef _WIN32
#include "eden/fs/fuse/FuseDispatcher.h"
#endif

namespace facebook::eden {

class EdenMount;
class InodeMap;
class ObjectFetchContext;
class ObjectStore;
class ParentInodeInfo;
class RenameLock;
class SharedRenameLock;
class TreeInode;

using ObjectFetchContextPtr = RefPtr<ObjectFetchContext>;

class InodeBase {
 public:
  /**
   * Constructor for the root TreeInode of an EdenMount.
   * type is set to dtype_t::Dir.
   */
  explicit InodeBase(EdenMount* mount);

  /**
   * Constructor for all non-root inodes.
   */
  InodeBase(
      InodeNumber ino,
      mode_t initialMode,
      const std::optional<InodeTimestamps>& initialTimestamps,
      TreeInodePtr parent,
      PathComponentPiece name);

  virtual ~InodeBase();

  InodeNumber getNodeId() const {
    return ino_;
  }

  dtype_t getType() const {
    return mode_to_dtype(initialMode_);
  }

  /**
   * Have the destructor remove this inode's overlay data. Only for InodeMap,
   * when unloading an unlinked inode that nothing references any more.
   */
  void removeOverlayDataOnDestruction() {
    removeOverlayDataOnDestruction_ = true;
  }

  mode_t getInitialMode() const {
    return initialMode_;
  }

  bool isDir() const {
    return getType() == dtype_t::Dir;
  }

  bool isSymlink() const {
    return getType() == dtype_t::Symlink;
  }

  /**
   * Increment the number of references to this inode by its inode number.
   *
   * While the channel reference count is non-zero, the inode number will be
   * remembered, and InodeMap::lookupInode() can be used to look up the inode
   * object using its inode number.  Once the channel reference count drops to
   * zero the inode number may be forgotten, and it is no longer valid to call
   * InodeMap::lookupInode() with this inode's number.
   *
   * This is generally intended for use by FUSE APIs that return an inode
   * number to the kernel: lookup(), create(), mkdir(), symlink(), link(), or
   * by ProjectedFS APIs that write a placeholder to disk.
   * On PrjFS and NFS, we treat the fs refcount as a flag to be 0 or 1.
   * We should revisit this behavior on macOS if we migrate to a FSChannel
   * implementation that sends us FORGET calls
   */
  void incFsRefcount(uint32_t count = 1) {
    if (folly::kIsLinux) {
      numFsReferences_.fetch_add(count, std::memory_order_acq_rel);
    } else {
      XDCHECK_EQ(count, 1u);
      numFsReferences_.store(1u, std::memory_order_release);
    }
  }

  /**
   * Decrement the number of outstanding references to this inode's number.
   *
   * This should be used to release inode number references obtained via
   * incFsRefcount().  The primary use case is for FUSE forget() calls, or
   * when a ProjectedFS placeholder is manually invalidated.
   * On PrjFS and NFS, we treat the fs refcount as a flag to be 0 or 1.
   * We should revisit this behavior on macOS if we migrate to a FSChannel
   * implementation that sends us FORGET calls
   */
  void decFsRefcount(uint32_t count = 1) {
    if (folly::kIsLinux) {
      auto prevValue =
          numFsReferences_.fetch_sub(count, std::memory_order_acq_rel);
      XDCHECK_GE(prevValue, count);
      if (prevValue == count) {
        fsReleasedLastReference();
      }
    } else {
      XDCHECK_GE(count, 1u);
      clearFsRefcount();
    }
  }

  /**
   * Decrement the FSReference count to zero. This is used on platforms
   * where the kernel does not tell us when an inode has been dereferenced.
   * (NFS and Windows).
   */
  bool clearFsRefcount() {
    bool wasReferenced =
        numFsReferences_.exchange(0u, std::memory_order_acq_rel) != 0;
    if (wasReferenced) {
      fsReleasedLastReference();
    }
    return wasReferenced;
  }

  /**
   * Get an InodePtr to ourself.
   *
   * This uses InodePtr::newPtrFromExisting() internally.
   *
   * This should only be called in contexts where we know an external caller
   * already has an existing reference to us.  (Which is most places--a caller
   * has to have a reference to us in order to call any of our APIs.)
   */
  InodePtr inodePtrFromThis() {
    return InodePtr::newPtrFromExisting(this);
  }

  /**
   * Get the EdenMount that this inode belongs to
   *
   * The EdenMount is guaranteed to remain valid for at least the lifetime of
   * this InodeBase object.
   */
  EdenMount* getMount() const {
    return mount_;
  }

  // See EdenDispatcher::getattr
  virtual ImmediateFuture<struct stat> stat(
      const ObjectFetchContextPtr& context) = 0;

  virtual folly::coro::now_task<struct stat> co_stat(
      const ObjectFetchContextPtr& context) = 0;

  // See Dispatcher::setattr
  virtual ImmediateFuture<struct stat> setattr(
      const DesiredMetadata& desired,
      const ObjectFetchContextPtr& fetchContext) = 0;

#ifndef _WIN32
  /**
   * Return type-only attributes to let clients address and remove an inode
   * whose overlay data is unreadable. Permission bits are omitted to prevent
   * clients from interpreting the zero size as readable, empty contents.
   */
  struct stat getStatForCorruptOverlay() const noexcept;

  [[nodiscard]] folly::Future<folly::Unit>
  setxattr(folly::StringPiece name, folly::StringPiece value, int flags);
  [[nodiscard]] folly::Future<folly::Unit> removexattr(folly::StringPiece name);

  virtual ImmediateFuture<std::vector<std::string>> listxattr() = 0;
  virtual ImmediateFuture<std::string> getxattr(
      folly::StringPiece name,
      const ObjectFetchContextPtr& context) = 0;

  [[nodiscard]] virtual folly::Future<folly::Unit> access(int mask);
#endif // !_WIN32

  /**
   * If this inode is not materialized, return its underlying Object ID. If it
   * is materialized, return nullptr.
   *
   * Never call getObjectId() while the inode's lock is held.
   */
  virtual std::optional<ObjectId> getObjectId() const = 0;

  /**
   * Returns whether the file is materialized in the overlay.
   *
   * Never call isMaterialized() while the inode's lock is held.
   */
  virtual bool isMaterialized() const = 0;

  /**
   * Check if this Inode has been unlinked from its parent TreeInode.
   *
   * Once an inode is unlinked it is no longer part of the file system tree.
   * It can still be accessed by existing FileHandles or other internal
   * InodePtrs referring to it, but it can no longer be accessed by a path.
   *
   * An unlinked Inode can never be re-linked back into the file system.
   * It will be destroyed when the last reference to it goes away.
   *
   * TreeInodes can only be unlinked when they have no children.  It is
   * therefore not possible to have an Inode object that is not marked unlinked
   * but has a parent tree that is unlinked.
   */
  bool isUnlinked() const;

  /**
   * Compute the path to this inode, from the root of the mount point.
   *
   * This will return the path to the file, or std::nullopt if the file has
   * been unlinked.
   *
   * BEWARE: Unless you are holding the mount-point's global rename lock when
   * you call this function, the file may have been renamed or unlinked by the
   * time you actually use the return value.
   */
  std::optional<RelativePath> getPath() const;

  /**
   * Compute the path to this inode, from the root of the mount point.
   *
   * This is similar to getPath with one major difference: unlinked inodes will
   * have their full path returned while getPath would return std::nullopt in
   * that case.
   *
   * This should not be used unless your code needs to deal with potentially
   * unlinked paths. Documenting why getPath cannot be used is strongly
   * recommended.
   */
  RelativePath getUnsafePath() const;

  /**
   * Get a string to use to refer to this file in a log message.
   *
   * This will usually return the path to the file, but if the file has been
   * unlinked it will return a string with data about where the file used to
   * exist.  The result is human-readable and is not designed for consumption
   * or parsing by other code.
   */
  std::string getLogPath() const;

  /**
   * markUnlinked() should only be invoked by TreeInode.
   *
   * This method is called when a child inode is unlinked from its parent.
   * This can happen in a few different ways:
   *
   * - By TreeInode::unlink() (for FileInode objects)
   * - By TreeInode::rmdir() (for TreeInode objects)
   * - By TreeInode::rename() for the destination of the rename,
   *   (which may be a file or an empty tree inode)
   *
   * This must be called while holding the parent's contents_ lock.
   *
   * Unlinking an inode may cause it to be immediately unloaded.  If this
   * occurs, this method returns a unique_ptr to itself.  The calling TreeInode
   * is then responsible for actually deleting the inode (which will happen
   * automatically when the unique_ptr is destroyed or reset) in their calling
   * context after they release their contents lock.  If unlinking this inode
   * does not cause it to be immediately unloaded then this method will return
   * a null pointer.
   */
  std::unique_ptr<InodeBase> markUnlinked(
      TreeInode* parent,
      PathComponentPiece name,
      const RenameLock& renameLock);

  /**
   * This method should only be called by TreeInode::loadUnlinkedChildInode().
   * Its purpose is to set the unlinked flag to true for inodes that have
   * been unlinked and passed over to the current process as part of a
   * graceful restart procedure.
   */
  void markUnlinkedAfterLoad();

  /**
   * updateLocation() should only be invoked by TreeInode.
   *
   * This is called when an inode is renamed to a new location.
   */
  void updateLocation(
      TreeInodePtr newParent,
      PathComponentPiece newName,
      const RenameLock& renameLock);

  /**
   * Check to see if the ptrAcquire reference count is zero.
   *
   * This method is intended for internal use by the InodeMap/TreeInode code,
   * so it can tell when it is safe to unload an inode.
   *
   * This method should only be called while holding both the parent
   * TreeInode's contents lock and the InodeMap lock.  (Otherwise the reference
   * count may be incremented by another thread before you can examine the
   * return value.)
   */
  bool isPtrAcquireCountZero() const {
    return ptrAcquireCount_.load(std::memory_order_acquire) == 0;
  }

  /**
   * Decrement the ptrAcquire reference count, and return its previous value.
   *
   * This method is intended for internal use by the InodeMap/TreeInode code,
   * so it can tell when it is safe to unload an inode.
   *
   * This method should only be called while holding both the parent
   * TreeInode's contents lock and the InodeMap lock.  (Otherwise the reference
   * count may be incremented by another thread before you can examine the
   * return value.)
   */
  uint32_t decPtrAcquireCount() const {
    return ptrAcquireCount_.fetch_sub(1, std::memory_order_acq_rel);
  }

  /**
   * Get the channel reference count.
   *
   * This is intended only to be checked when an Inode is being unloaded,
   * while holding both its parent TreeInode's contents_ lock and the InodeMap
   * lock.
   *
   * The channel reference count is only incremented or decremented while
   * holding a pointer reference on the Inode.  Checking the channel reference
   * count is therefore safe during unload, when we are sure there are no
   * outstanding pointer references to the inode.
   *
   * Checking the channel reference count at any other point in time may be
   * racy, since other threads may be changing the reference count concurrently.
   */
  uint32_t getFsRefcount() const {
    // XDCHECK that the caller is only calling us while the inode is being
    // unloaded
    XDCHECK_EQ(0u, ptrAcquireCount_.load(std::memory_order_acquire));

    return numFsReferences_.load(std::memory_order_acquire);
  }

  /**
   * Get the channel refcount for debugging or diagnostic purposes.
   *
   * This method should only be used during unit tests or diagnostic utilities.
   * The channel refcount may change as soon as this function returns (before
   * the caller has a chance to examine the result), so this should never be
   * used for any real decision making purposes.
   */
  uint32_t debugGetFsRefcount() const {
    return numFsReferences_.load(std::memory_order_acquire);
  }

  /**
   * Set the channel reference count.
   *
   * This method should only be called by InodeMap when first loading an Inode,
   * before the Inode object has been returned to any users.
   */
  void setChannelRefcount(uint32_t count) {
    if (folly::kIsWindows) {
      XDCHECK_GE(1u, count);
    }
    return numFsReferences_.store(count, std::memory_order_release);
  }

  /**
   * Get the parent directory of this inode.
   *
   * This returns nullptr only if the current inode is the root of the mount.
   * If this inode has been unlinked this returns the TreeInode that this inode
   * used to be a child of.  Use getParentInfo() if you also want to tell if
   * this file is unlinked.
   *
   * This must be called while holding the rename lock, to ensure the parent
   * does not change before the return value can be used.
   */
  TreeInodePtr getParent(const RenameLock&) const {
    return location_.rlock()->parent;
  }
  TreeInodePtr getParent(const SharedRenameLock&) const {
    return location_.rlock()->parent;
  }

  /**
   * Returns this inode's parent at this exact point in time.  Note that, unless
   * the rename lock is held, the parent can change between the call and the
   * return value being used.  If the rename lock is held, call getParent()
   * instead.
   *
   * Used in TreeInode::readdir.
   */
  TreeInodePtr getParentRacy() {
    return location_.rlock()->parent;
  }

  /**
   * When this inode was last referenced for the purposes of inode garbage
   * collection; see lastGcRefTime_.
   */
  EdenTimestamp getLastGcRefTime() const {
    return lastGcRefTime_.load(std::memory_order_relaxed);
  }

  struct LocationInfo {
    LocationInfo(TreeInodePtr p, PathComponentPiece n)
        : parent(std::move(p)), name(n) {}

    TreeInodePtr parent;
    /**
     * unlinked will be set to true if the Inode has been unlinked from the
     * filesystem.
     *
     * The Inode object may continue to exist for some time after being
     * unlinked, but it can no longer be referred to by name.  For example, the
     * Inode object will continue to exist for at least as long as there are
     * open file handles referring to it.
     *
     * The name member will still track the file's old name, but it should only
     * be used for debugging/logging purposes at that point.
     */
    bool unlinked{false};
    PathComponent name;
  };

  /**
   * Get information about the path to this Inode in the mount point.
   *
   * This must be called while holding the rename lock, to ensure the location
   * does not change before the return value can be used.
   */
  LocationInfo getLocationInfo(const RenameLock&) const {
    auto loc = location_.rlock();
    return *loc;
  }

  /**
   * Returns this inode's name at this exact point in time.  Note that, unless
   * the rename lock is held, the name can change between the call and the
   * return value being used.  If the rename lock is held, call
   * getLocationInfo()->name instead.
   *
   * Used in FileInode.cpp when getting filenames for telemetry
   */
  PathComponent getNameRacy() {
    return location_.rlock()->name;
  }
#ifndef _WIN32
  /**
   * Acquire this inode's contents lock and return its metadata.
   */
  virtual InodeMetadata getMetadata() const = 0;
#endif

  /**
   * Acquire the content lock and update the inode metadata. This method is used
   * to make the kernel refresh its caches on NFS.
   */
  virtual void forceMetadataUpdate() = 0;

#ifndef _WIN32
  /**
   * Force materialize a file or a tree and rely on the overlay as the source of
   * the files. If the inode is a symlink and followSymlink is true, its target
   * will be materialized if possible. If the inode is a tree, every child in
   * this node will be recursively materialized. Meant for debugging and
   * benchmarking through the `ensureMaterialized` Thrift endpoint.
   */
  [[nodiscard]] virtual ImmediateFuture<folly::Unit> ensureMaterialized(
      const ObjectFetchContextPtr& context,
      bool followSymlink) = 0;
#endif

 protected:
  /**
   * Returns current time from EdenMount's clock.
   */
  EdenTimestamp getNow() const;

  /**
   * Convenience method to return the mount point's Clock.
   */
  const Clock& getClock() const;

  /**
   * Convenience method to return the mount point's ObjectStore.
   *
   * The ObjectStore is owned by the EdenMount. and is guaranteed to remain
   * valid for at least the lifetime of the InodeBase object.
   */
  ObjectStore& getObjectStore() const;

  /**
   * Helper function to update Journal in FileInode and TreeInode.
   */
  void updateJournal();

  /**
   * Log accesses via the ServerState's InodeAccessLogger.
   */
  void logAccess(const ObjectFetchContext& fetchContext);

 private:
#ifndef _WIN32
  // The caller (which is always InodeBaseMetadata) must be holding the
  // inode state lock for this type of inode when calling
  // getMetadataLocked().
  InodeMetadata getMetadataLocked() const;
#endif

  /**
   * Updates atime for this inode to now. The value is only updated for this
   * inode, it is not propagated to its parents.
   */
  void updateAtime();

  /**
   * Updates mtime and ctime for this inode. The value is only updated for this
   * inode, it is not propagated to its parents.
   */
  void updateMtimeAndCtime(EdenTimestamp now);

  template <typename InodeType>
  friend class InodePtrImpl;
  friend class InodeMap;
  friend class InodePtrTestHelper;

  void restoreLastGcRefTime(EdenTimestamp timestamp) {
    lastGcRefTime_.store(timestamp, std::memory_order_relaxed);
  }

  // Forbid copies and moves (we cannot be moved since we contain mutexes)
  InodeBase(InodeBase const&) = delete;
  InodeBase& operator=(InodeBase const&) = delete;
  InodeBase(InodeBase&&) = delete;
  InodeBase& operator=(InodeBase&&) = delete;

  /**
   * Helper function for get*Path()
   *
   * Populates the names vector with the list of PathComponents from the root
   * down to this inode.
   *
   * This method should not be called on the root inode.  The caller is
   * responsible for checking that before calling getPathHelper().
   *
   * Returns true if the file exists at the given path, or false if the file
   * has been unlinked.
   *
   * If stopOnUnlinked is true, it breaks immediately when it finds that the
   * file has been unlinked.  The contents of the names vector are then
   * undefined if the function returns false.
   *
   * If stopOnUnlinked is false it continues building the names vector even if
   * the file is unlinked, which will then contain the path that the file used
   * to exist at.  (This path should be used only for logging purposes at that
   * point.)
   */
  bool getPathHelper(std::vector<PathComponent>& names, bool stopOnUnlinked)
      const;

  // incrementPtrRef() is called by InodePtr whenever an InodePtr is copied.
  void incrementPtrRef() const {
    auto prevValue = ptrRefcount_.fetch_add(1, std::memory_order_acq_rel);
    // Calls to incrementPtrRef() are not allowed to increment the reference
    // count from 0 to 1.
    //
    // The refcount is only allowed to go from 0 to 1 when holding the InodeMap
    // lock or our parent TreeInode's contents lock.  Those two situations call
    // newInodeRefConstructed() instead
    XDCHECK_NE(0u, prevValue);
  }

  // newInodeRefConstructed() is called any time we construct a brand new
  // InodePtr in response to a request to access or load an Inode.  The only
  // APIs that hand out new InodePtrs are InodeMap::lookupInode() and
  // TreeInode::getOrLoadChild().
  void newInodeRefConstructed() const {
    auto prevValue = ptrRefcount_.fetch_add(1, std::memory_order_acq_rel);
    if (prevValue == 0) {
      ptrAcquireCount_.fetch_add(1, std::memory_order_acq_rel);
    }
  }
  void decrementPtrRef() const {
    auto prevValue = ptrRefcount_.fetch_sub(1, std::memory_order_acq_rel);
    if (prevValue == 1) {
      onPtrRefZero();
    }
  }
  void onPtrRefZero() const;

 public:
  /**
   * Read the refcount of this inode.
   *
   * This is racy and should only be called in debug/X*CHECK.
   */
  uint32_t debugGetPtrRef() const {
    return ptrRefcount_.load(std::memory_order_acquire);
  }

  /**
   * Records that this inode was just referenced, so that inode garbage
   * collection leaves it alone for another cutoff period. The value is only
   * updated for this inode, it is not propagated to its parents.
   */
  void markGcRef();

 private:
  ParentInodeInfo getParentInfo() const;

  InodeNumber const ino_;

  /**
   * The EdenMount object that this inode belongs to.
   *
   * We store this as a raw pointer since the TreeInode is part of the mount
   * point.  The EdenMount will always exist longer than any inodes it
   * contains.
   */
  EdenMount* const mount_;

  /**
   * The initial mode bits specified when this inode was first created
   * or instantiated from version control.  Primarily used when lazily
   * writing metadata into this inode's metadata storage.  The type
   * bits can never change - they can be accessed via getType().
   */
  void removeOverlayData() noexcept;

  mode_t const initialMode_;

  /**
   * Set by InodeMap when this inode is unloaded while unlinked and no longer
   * referenced by the filesystem: its overlay data is removed by the
   * destructor, which every unload path runs after releasing the InodeMap and
   * parent contents locks, rather than while holding them.
   */
  bool removeOverlayDataOnDestruction_{false};

  /**
   * A reference count tracking the outstanding lookups that the kernel's FUSE
   * API has performed on this inode.  We must remember this inode number
   * for as long as the FUSE API has references to it.  (However, we may unload
   * the Inode object itself, destroying ourself and letting the InodeMap
   * simply remember the association of the InodeNumber with our location in
   * the file system.)
   *
   * For ProjectedFS, a positive value tracks whether a placeholder has been
   * cached on disk for that inode. Fully unloading that inode will require
   * also invalidating the placeholder.
   */
  std::atomic<uint32_t> numFsReferences_{0};

  /**
   * A reference count used by InodePtr.
   *
   * A few notes about the refcount management:
   *
   * - Inode objects are not necessarily destroyed immediately when the
   *   refcount goes to 0.  They may remain in memory for a while in case they
   *   get used again relatively soon.  When necessary we can sweep the loaded
   *   inode objects and unload inodes whose refcount is 0 and who have not
   *   been accessed recently.
   *
   * - When copying or deleting InodePtr objects this reference count is
   *   updated atomically with acquire/release barriers.  No other locks need
   *   to be held during these operations.  The current thread is guaranteed to
   *   already hold a reference to the Inode in question since it already has
   *   an InodePtr.  These operations can increment a refcount from 1 or more
   *   to a higher value, but they can never increment a refcount from 0 to 1.
   *   They can also decrement a refcount from 1 to 0.
   *
   * - Either the InodeMap lock or the parent TreeInode's contents lock is
   *   always held when incrementing the refcount from 0 to 1.
   *
   *   Only two operations can increment the refcount from 0 to 1:
   *   - InodeMap::lookupInode().
   *     This acquires the InodeMap lock
   *   - TreeInode::getOrLoadChild()
   *     This acquires the parent's TreeInode lock
   *
   *   When checking to see if we can unload an inode, we acquire both it's
   *   parent TreeInode's contents lock and the InodeMap lock (in that order).
   *   We are therefore guaranteed that if the refcount is 0 when we check it,
   *   no other thread can increment it to 1 before we delete the object.
   *
   * Notes about owning vs non-owning pointers:
   * - An Inode always holds an owning TreeInodePtr to its parent.  This
   *   ensures the parent cannot be unloaded as long as it has any unloaded
   *   children.
   *
   * - The InodeMap stores raw (non-owning) pointers to the inodes.  When an
   *   Inode is unloaded we explicitly inform the InodeMap of the change.
   *
   * - Each TreeInode holds raw (non-owning) pointers to its children.  When an
   *   Inode is unloaded we explicitly reset its parent pointer to this object.
   *
   * - The numFsReferences_ variable tracks the number of users that know
   *   about this inode by its inode number.  However, this does not prevent us
   *   from destroying the Inode object.  We can unload the Inode object itself
   *   in this case, and InodeMap will retain enough information to be able to
   *   re-create the Inode object later if this inode is looked up again.
   */
  mutable std::atomic<uint32_t> ptrRefcount_{0};

  /**
   * The number of times the ptrRefcount_ has been incremented from 0 to 1,
   * minus the number of times it has been decremented from 1 to 0.
   *
   * This is necessary so we can properly synchronize destruction, and ensure
   * that only one thread tries to destroy a given Inode.
   *
   * This variable can only be incremented when holding either the parent
   * TreeInode's contents_ lock or the InodeMap lock.  It can only be
   * decremented when holding both the parent TreeInode's contents_ lock and
   * the InodeMap lock.  When ptrAcquireCount_ drops to 0 it is safe to delete
   * the Inode.
   *
   * It isn't safe to delete the Inode purely based on ptrRefcount_ alone,
   * since ptrRefcount_ is decremented without holding any other locks.  It's
   * possible that thread A ptrRefcount_ drops to 0 and then thread B
   * immediately increments ptrRefcount_ back to 1.  If thread B then drops the
   * refcount back to 0 we need to make sure that only one of thread A and
   * thread B try to destroy the inode.
   *
   * By tracking ptrRefcount_ and ptrAcquireCount_ separately we allow
   * ptrRefcount_ to be manipulated with a single atomic operation in most
   * cases (when not transitioning between 0 and 1).  Only when transitioning
   * from 0 to 1 or vice-versa do we need to acquire additional locks and
   * perform more synchronization.
   */
  mutable std::atomic<uint32_t> ptrAcquireCount_{0};

  /**
   * Information about this Inode's location in the file system path.
   * Eden does not support hard links, so each Inode has exactly one location.
   *
   * To read the location data you only need to acquire the Synchronized
   * object's read lock.
   *
   * However, to update location data you must acquire both the mount point's
   * global rename lock and acquire this Synchronized object's write lock.
   * (acquire the mount-point rename lock first).
   */
  folly::Synchronized<LocationInfo> location_;

  /**
   * The filesystem released its last reference to this inode. As far as GC is
   * concerned the inode has not been referenced since the beginning of time:
   * it is eligible for the next sweep however recently it was used, unless
   * something references it again first.
   */
  void fsReleasedLastReference() {
    lastGcRefTime_.store(EdenTimestamp{timespec{}}, std::memory_order_relaxed);
  }

  /**
   * The last time this inode was referenced, as far as inode garbage
   * collection is concerned: every FUSE and NFS request involving the inode
   * records one, and so does a status walk that compares it. When the
   * filesystem releases its last reference to the inode the time is reset to
   * zero, so the kernel's own eviction makes the inode eligible for the next
   * sweep. GC treats an inode whose last reference is older than its cutoff
   * as unused and collects it.
   *
   * Note1: This field is distinct from atime and is not persisted to disk via
   * InodeTable.
   *
   * Note2: This field get updated for the inode itself and not propagate to its
   * parent.
   */
  std::atomic<EdenTimestamp> lastGcRefTime_{getNow()};

  template <typename InodeState>
  friend class InodeBaseMetadata;
};

/**
 * Base class for FileInode and TreeInode that ensures access to the metadata
 * only occurs while the inode's state lock is held.
 */
template <typename InodeState>
class InodeBaseMetadata : public InodeBase {
 public:
  // Forward constructors from InodeBase.
  using InodeBase::InodeBase;

 protected:
#ifndef _WIN32
  /**
   * Get this inode's metadata. The inode's state lock must be held.
   */
  InodeMetadata getMetadataLocked(const InodeState&) const {
    return InodeBase::getMetadataLocked();
  }
#endif

  /**
   * Helper function to set the atime of this inode. The inode's state lock must
   * be held.
   *
   * Note that FUSE doesn't claim to fully implement atime.
   * https://sourceforge.net/p/fuse/mailman/message/34448996/
   */
  void updateAtimeLocked(InodeState&) {
    return InodeBase::updateAtime();
  }

  /**
   * Updates this inode's mtime and ctime to the given timestamp. The inode's
   * state lock must be held.
   */
  void updateMtimeAndCtimeLocked(InodeState&, EdenTimestamp now) {
    InodeBase::updateMtimeAndCtime(now);
  }
};

} // namespace facebook::eden

#include "eden/fs/inodes/InodePtr-defs.h"
