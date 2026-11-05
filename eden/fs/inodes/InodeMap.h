/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/Synchronized.h>
#include <folly/futures/Future.h>
#include <list>
#include <memory>
#include <optional>
#include <unordered_map>
#include <vector>

#include "eden/common/utils/ImmediateFuture.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/InodeNumber.h"
#include "eden/fs/inodes/InodePtr.h"
#include "eden/fs/inodes/InodeTimestamps.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/model/ObjectId.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/takeover/gen-cpp2/takeover_types.h"

namespace folly {
class exception_wrapper;
}

namespace facebook::eden {

class EdenFsEventsLogger;
class EdenMount;
class FileInode;
class InodeBase;
class TreeInode;
class ParentInodeInfo;
class ReloadableConfig;

class EdenStats;
template <typename T>
class RefPtr;
using EdenStatsPtr = RefPtr<EdenStats>;

class InodeMapLock;

/**
 * InodeMap allows looking up Inode objects based on a inode number.
 *
 * All operations on this class are thread-safe.
 *
 * Note that InodeNumber values and Inode objects have separate lifetimes:
 * - Inode numbers are allocated when we need to return a InodeNumber value to
 *   the FUSE/ProjectedFS APIs. These are generally allocated by lookup()
 *   calls. FUSE will call forget() to let us know when we can forget about a
 *   InodeNumber value. For ProjectedFS, we can forget about an InodeNumber as
 *   soon as it is invalidated from the FS.  (We may not necessarily forget
 *   about the InodeNumber value immediately, though.)
 *
 * - InodeBase objects are needed when we have to actually operate on a file or
 *   directory.  Any operation more than looking up a file name (or some of its
 *   basic attributes that can be found in its parent directory's entry for it)
 *   requires loading its Inode.
 *
 *   After we load an InodeBase object we keep it loaded in memory, since it is
 *   likely to be needed again in the future if the user keeps using the
 *   file/directory.  However, we can unload InodeBase objects on demand when
 *   they are not being used by other parts of the code.  This helps us reduce
 *   our memory footprint.  (For instance, if a user runs a command that walks
 *   the entire repository we don't want to keep Inode objects loaded for all
 *   files forever.)
 *
 * We can unload an InodeBase object even when the InodeNumber value is still
 * valid.  Therefore this class contains two separate maps:
 * - (InodeNumber --> InodeBase)
 *   This map stores all currently loaded Inodes.
 * - (InodeNumber --> (parent InodeNumber, name))
 *   This map contains information about Inodes that are not currently loaded.
 *   This map contains enough information to identify the file or directory in
 *   question if we do need to load the inode.  The parent directory's
 *   InodeNumber may not be loaded either; we have to load it first in order to
 *   load the inode in question.
 *
 * Additional Notes about InodeNumber allocation:
 * - InodeNumber numbers are primarily allocated via the
 *   EdenDispatcher::lookup() call.
 *
 *   Rather than only allocate a InodeNumber in this case, we go ahead and load
 *   the actual TreeInode/FileInode, since FUSE is very likely to make another
 *   call for this inode next.  Therefore, in this case the newly allocated
 *   inode number is inserted directly into loadedInodes_, without ever being
 *   in unloadedInodes_.
 *
 *   The unloadedInodes_ map is primarily for inodes that were loaded and have
 *   since been unloaded due to inactivity.
 *
 * - Inode objects can either be allocated via EdenDispatcher::lookup(), or via
 *   an operation on a TreeInode looking up a child entry.
 *
 *   We currently always allocate a InodeNumber value for any new Inode object
 *   even if it is not needed yet by the FUSE APIs.
 */
class InodeMap {
 public:
  using PromiseVector = std::vector<folly::Promise<InodePtr>>;

  explicit InodeMap(
      EdenMount* mount,
      std::shared_ptr<ReloadableConfig> config,
      EdenStatsPtr stats,
      std::shared_ptr<EdenFsEventsLogger> logger);
  virtual ~InodeMap();

  InodeMap(InodeMap&&) = delete;
  InodeMap& operator=(InodeMap&&) = delete;

  /**
   * Initialize the InodeMap
   *
   * This method must be called shortly after constructing an InodeMap object,
   * before it is visible to other threads.  This method is not thread safe.
   *
   * This is provided as a separate method from the constructor purely to
   * provide callers with slightly more flexibility in ordering of events when
   * constructing an InodeMap.  This generally should be thought of as part of
   * the InodeMap construction process, though.
   *
   * @param root The root TreeInode.
   */
  void initialize(TreeInodePtr root);

  /**
   * Initialize the InodeMap from data handed over from a process being taken
   * over.
   *
   * This method has the same constraints and concerns as initialize().
   */
  void initializeFromTakeover(
      TreeInodePtr root,
      const SerializedInodeMap& takeover);

  /**
   * Initialize the InodeMap from the content of the overlay.
   *
   * This should be called on platforms where the working copy is persistent in
   * between restarts so that the InodeMap is populated with all the inodes
   * that are already present on disk. This needs to be called before the
   * FsChannel is initialized for the mount.
   *
   * The only platform where this applies is Windows.
   */
  void initializeFromOverlay(TreeInodePtr root, Overlay& overlay);

  /**
   * Get the root inode.
   */
  const TreeInodePtr& getRootInode() const {
    return root_;
  }

  /**
   * Lookup an Inode object by inode number.
   *
   * Inode objects can only be looked up by number if the inode number
   * reference count is non-zero.  The inode number refcount is incremented by
   * calling incFsRefcount() on the Inode object.  The initial access that
   * first creates an Inode is always by name.  After the initial access,
   * incFsRefcount() can be called to allow it to be retrieved by inode
   * number later.  InodeMap::decFsRefcount() can be used to drop an inode
   * number reference count.
   *
   * If the InodeBase object is not currently loaded it will be loaded and a
   * new InodeBase object returned.
   *
   * Loading an Inode requires retrieving data about it from the ObjectStore,
   * which may take some time.  Therefore lookupInode() returns a Future, which
   * will be fulfilled when the loaded Inode is ready.  The Future may be
   * invoked immediately in the calling thread (if the Inode is already
   * available), or it may be invoked later in a different thread.
   *
   * The context is used for the loads this call starts, including those of
   * unloaded ancestors. A lookup that joins a load already in progress does
   * not change that load's context.
   */
  ImmediateFuture<InodePtr> lookupInode(
      InodeNumber number,
      const ObjectFetchContextPtr& context);

  /**
   * Lookup a TreeInode object by inode number.
   *
   * This creates the TreeInode object if it is not currently loaded.
   * The returned Future throws ENOTDIR if this inode number does not refer to
   * a directory.
   */
  ImmediateFuture<TreeInodePtr> lookupTreeInode(
      InodeNumber number,
      const ObjectFetchContextPtr& context);

  /**
   * Lookup a FileInode object by inode number.
   *
   * This creates the FileInode object if it is not currently loaded.
   * The returned Future throws EISDIR if this inode number refers to a
   * directory.
   */
  ImmediateFuture<FileInodePtr> lookupFileInode(
      InodeNumber number,
      const ObjectFetchContextPtr& context);

  /**
   * Lookup an Inode object by inode number, if it already exists.
   *
   * This returns an existing InodeBase object if one is currently loaded,
   * but nullptr if one is not loaded.
   */
  InodePtr lookupLoadedInode(InodeNumber number);

  /**
   * Lookup a loaded TreeInode object by inode number, if it already exists.
   *
   * Returns nullptr if this inode object is not currently loaded.
   * Throws ENOTDIR if this inode is loaded but does not refer to a directory.
   */
  TreeInodePtr lookupLoadedTree(InodeNumber number);

  /**
   * Lookup a loaded FileInode object by inode number, if it already exists.
   *
   * Returns nullptr if this inode object is not currently loaded.
   * Throws EISDIR if this inode is loaded but refers to a directory.
   */
  FileInodePtr lookupLoadedFile(InodeNumber number);

  /**
   * Recursively determines the path for a loaded or unloaded inode. If the
   * inode is unloaded, it appends the name of the unloaded inode to the path
   * of the parent inode (which is determined recursively). If the inode is
   * loaded, it returns InodeBase::getPath() (which also recursively
   * queries the parent nodes).
   *
   * If there is an unlinked inode in the path, this function returns
   * std::nullopt. If the inode is invalid, it throws EINVAL.
   */
  std::optional<RelativePath> getPathForInode(InodeNumber inodeNumber);

  /**
   * Decrement the number of outstanding FS references to an inode number.
   *
   * Note that there is no corresponding incFsRefcount() function:
   * increments are always done directly on a loaded InodeBase object.
   *
   * However, decrements may happen after we have decided to unload the Inode
   * object.  Therefore decrements are performed on the InodeMap so that we can
   * avoid loading an Inode object just to decrement its reference count.
   */
  void decFsRefcount(InodeNumber number, uint32_t count = 1);

  /**
   * Clear the FS refcount for an inode number.
   * This is used when we are deleting an inode and want to clear the refcount.
   *
   * Returns true if the inode had a non-zero FS refcount.
   */
  bool clearFsRefcount(InodeNumber number);

  /**
   * See EdenMount::forgetStaleInodes
   */
  void forgetStaleInodes();

  /**
   * Indicate that the mount point has been unmounted.
   *
   * Calling this before shutdown() will inform the InodeMap that it no longer
   * needs to remember inodes with outstanding FS refcounts when shutting
   * down.
   */
  void setUnmounted();

  /**
   * Shutdown the InodeMap.
   *
   * The shutdown process must wait for all Inode objects in this InodeMap to
   * become unreferenced and be unloaded.  Returns a Future that will be
   * fulfilled once the shutdown has completed.  If doTakeover is true, the
   * resulting SerializedInodeMap will include data sufficient for
   * reconstructing all inode state in the new process.
   *
   * This function should generally only be invoked by EdenMount::shutdown().
   * Other callers should use EdenMount::shutdown() instead of invoking this
   * function directly.
   */
  [[nodiscard]] folly::Future<SerializedInodeMap> shutdown(bool doTakeover);

  /**
   * Returns true if we have stored information about this inode that may
   * need to be updated if the inode's state changes.
   *
   * This returns true if we are currently in the process of loading this
   * inode, or if we previously had this inode loaded and are remembering
   * it because the kernel still remembers its inode number or some of its
   * children's inode numbers.
   */
  bool isInodeRemembered(InodeNumber ino) const;

  /**
   * Returns if the inode is either loaded, unloaded or in the process of being
   * loaded.
   */
  bool isInodeLoadedOrRemembered(InodeNumber ino) const;

  /**
   * onInodeUnreferenced() will be called when an Inode's InodePtr reference
   * count drops to zero.
   *
   * This is an internal API that should not be called by most users.
   */
  void onInodeUnreferenced(InodeBase* inode, ParentInodeInfo&& parentInfo);
  bool onLinkedInodeUnreferenced(InodeBase* inode);

  /**
   * Acquire the InodeMap lock while performing Inode unloading.
   *
   * This method is called by TreeInode when scanning its children for
   * unloading.  It should only be called *after* acquiring the TreeInode
   * contents lock.
   *
   * This is an internal API that should not be used by most callers.
   */
  InodeMapLock lockForUnload();

  /**
   * unloadedInode() should be called to unload an unreferenced inode.
   *
   * The caller is responsible for ensuring that this inode is unreferenced and
   * safe to unload.  The caller must have previously locked the parent
   * TreeInode's contents map  (unless the inode is unlinked, in which case it
   * no longer has a parent).  The caller must have also locked the InodeMap
   * using lockForUnload().
   *
   * This is an internal API that should not be used by most callers.
   *
   * The caller is still responsible for deleting the InodeBase.
   * The InodeBase should not be deleted until releasing the InodeMap and
   * TreeInode locks (since deleting it may cause its parent Inode to become
   * unreferenced, triggering another immediate call to onInodeUnreferenced(),
   * which will acquire these locks).
   */
  void unloadInode(
      InodeBase* inode,
      TreeInode* parent,
      PathComponentPiece name,
      bool isUnlinked,
      const InodeMapLock& lock);

  /**
   * Return whether unloading this tree would preserve it in unloadedInodes_.
   *
   * This reads the tree's contents without acquiring its lock, so the caller
   * must guarantee that no other thread can acquire the inode: its pointer
   * acquire count is zero and its parent's contents lock is held.
   */
  bool hasRememberedChildForUnload(
      const TreeInode& inode,
      const InodeMapLock& lock) const;

  /////////////////////////////////////////////////////////////////////////
  // The following public APIs should only be used by TreeInode
  /////////////////////////////////////////////////////////////////////////

  enum class ChildLoadResult { Started, Pending, Conflict };

  /**
   * startLoadingChildIfNotLoading() should only be called by TreeInode.
   *
   * startLoadingChildIfNotLoading() will be called when TreeInode wants to load
   * one of its child entries that already has an allocated inode number.  It
   * returns Started if the TreeInode should start loading the inode now,
   * Pending if it is already being loaded, or Conflict if the entry conflicts
   * with another reference to this inode. On Conflict the promise is untouched,
   * so the caller can repair the entry and retry or fulfill it with an error.
   * If it will return Started, the function
   * publishes an event to tracebus marking the start of the inode load and
   * attaches an initial promise to the inode to resolve when the load ends.
   *
   * An inode number that is already loaded under another directory entry is
   * not loaded a second time: the function returns Conflict. Two entries
   * share a number only when the overlay is
   * damaged, for example by a rename that was interrupted after its first
   * write.
   *
   * If startLoadingChildIfNotLoading() returns Started, the TreeInode will then
   * continue loading the child inode.  It must then call inodeLoadComplete() or
   * inodeLoadFailed() when it finishes loading the inode.
   *
   * The TreeInode must be holding its contents lock when calling this method.
   *
   * @param parent The TreeInode calling this function to check if it should
   *   load a child.
   * @param name The name of the child inode.
   * @param childInode The inode number of the child.
   * @param mode The st_mode of the child.
   * @param promise A promise to fulfill when this inode is finished loading.
   *   The InodeMap takes ownership unless it returns Conflict.
   *
   * @return Whether to start loading, wait for an existing load, or handle a
   *   conflicting binding.
   */
  ChildLoadResult startLoadingChildIfNotLoading(
      TreeInode* parent,
      PathComponentPiece name,
      InodeNumber childInode,
      mode_t mode,
      folly::Promise<InodePtr>& promise);

  /**
   * Report the two paths when startLoadingChildIfNotLoading() finds an inode
   * number already loaded under another directory entry.
   */
  void reportDuplicateEntry(
      TreeInode* parent,
      PathComponentPiece name,
      InodeNumber childInode,
      const std::string& loadedAs);

  /**
   * inodeLoadComplete() should only be called by TreeInode.
   *
   * The TreeInode must be holding its contents lock when calling this method.
   *
   * We update both the parent's contents map and the InodeMap while holding
   * the contents lock.  This ensures that if you lock a TreeInode and see that
   * an inode isn't present in its contents, it cannot have finished loading
   * yet in the InodeMap.
   *
   * Returns a vector of Promises waiting on this TreeInode to be loaded.  The
   * TreeInode must fulfill these promises after releasing its contents lock.
   */
  PromiseVector inodeLoadComplete(InodeBase* inode);

  /**
   * inodeLoadFailed() should only be called by TreeInode (or startChildLookup)
   *
   * This should be called when an attempt to load a child inode fails.
   * A failure that is the expected outcome of a race, such as a lookup of an
   * inode that was removed meanwhile, passes logAsError=false to skip the
   * error log and the error logger. The inode trace event is published
   * either way.
   */
  void inodeLoadFailed(
      InodeNumber number,
      const folly::exception_wrapper& exception,
      bool logAsError = true);

  void inodeCreated(const InodePtr& inode);

  struct InodeCounts {
    size_t fileCount = 0;
    size_t treeCount = 0;
    size_t unloadedInodeCount = 0;
    size_t periodicUnlinkedUnloadInodeCount = 0;
    size_t periodicLinkedUnloadInodeCount = 0;
    /** Cumulative remembered records removed when FS references reach zero. */
    size_t forgottenInodeCount = 0;
  };

  /**
   * Get stats about how many Inode objects are loaded in memory, and how many
   * are unloaded but still tracked.
   */
  InodeCounts getInodeCounts() const;

  /**
   * Get the total number of inodes (loaded + unloaded) without acquiring the
   * data_ lock. This is suitable for hot paths like computing FUSE TTLs.
   */
  size_t getTotalInodeCountFast() const {
    return totalInodeCount_.load(std::memory_order_relaxed);
  }

  void recordPeriodicInodeUnload(size_t numInodesToUnload);
  /*
   * Return all referenced inodes (loaded and unloaded inodes whose
   * fs references is greater than zero).
   */
  std::vector<InodeNumber> getReferencedInodes() const;

  struct UnloadedInodeGcCandidate {
    InodeNumber inodeNumber;
    PathComponent name;
  };

  struct UnloadedInodeGcEntry {
    PathComponent name;
    EdenTimestamp lastGcRefTime;
    uint32_t numFsReferences;
  };

  /** Return copies of unloaded child state for one inode GC scan batch. */
  std::vector<UnloadedInodeGcEntry> getUnloadedChildrenForGc(
      InodeNumber parent,
      const std::vector<UnloadedInodeGcCandidate>& candidates) const;

 private:
  friend class InodeMapLock;

  /**
   * Data about an unloaded inode.
   */
  struct UnloadedInode {
    /**
     * Constructor only using parentNum, entryName, and mode. Only used by
     * InodeMap::startLoadingChildIfNotLoading. Note, when this is used, the
     * id (which gets set to a default empty string) may not be accurate for
     * unmaterialized inodes. Also, the mode, which is based off of the
     * initial_mode retrieved from a Directory Entry, should have correct
     * file/directory bits. However, it may not have fully accurate permission
     * bits as these might have changed from the Directory Entry's initial_mode.
     */
    UnloadedInode(
        InodeNumber parentNum,
        PathComponentPiece entryName,
        mode_t mode,
        EdenTimestamp lastGcRefTime);

    UnloadedInode(
        InodeNumber parentNum,
        PathComponentPiece entryName,
        bool isUnlinked,
        mode_t mode,
        std::optional<ObjectId> id,
        uint32_t fsRefcount,
        EdenTimestamp lastGcRefTime);
    UnloadedInode(
        FileInode* inode,
        TreeInode* parent,
        PathComponentPiece entryName,
        bool isUnlinked,
        uint32_t fsRefcount);

    InodeNumber const parent;
    PathComponent const name;

    /**
     * A boolean indicating if this inode is unlinked.
     */
    bool const isUnlinked{false};

    /**
     * The complete st_mode value for this entry. Note, while the file/directory
     * bits should be correct, the permission bits might be wrong as they may
     * have been changed from when the mode was first created.
     */
    mode_t const mode{0};

    /**
     * Helper function to get an InodeType from the inode's mode attribute
     */
    InodeType getInodeType() const;

    // Match the linked binding and file type; permission bits may have changed
    // since the parent's DirEntry recorded its initial mode.
    bool matchesLinkedChild(
        InodeNumber parentNumber,
        PathComponentPiece entryName,
        mode_t initialMode) const;

    /**
     * If the entry is not materialized, this contains the id
     * identifying the source control Tree (if this is a directory) or Blob
     * (if this is a file) that contains the entry contents.
     *
     * If the entry is materialized, this field is not set.
     */
    std::optional<ObjectId> const id;

    EdenTimestamp const lastGcRefTime;

    /**
     * A list of promises waiting on this inode to be loaded.
     *
     * If this list is non-empty then the inode is currently in the process of
     * being loaded.
     *
     * (We could use folly::SharedPromise here instead, but it has extra
     * overhead that we don't really need.  It performs its own locking, but we
     * are already protected by the data_ lock.)
     */
    PromiseVector promises;

    /**
     * Contains the timestamp for when an unloaded inode started loading. If the
     * promises list is non-empty, this time will be the timestamp for when the
     * first promise was added. Used for telemetry by the inode tracing CLI and
     * ActivityBuffer.
     */
    std::chrono::system_clock::time_point loadStartTime;

    /**
     * The number of times we have returned this inode number to FUSE via
     * lookup() calls that have not yet been released with a corresponding
     * forget(). For ProjectedFS, a positive value indicates an on-disk
     * placeholder for that inode that hasn't been invalided.
     */
    uint32_t numFsReferences{0};
  };

  struct LoadedInode {
    LoadedInode() = default;

    /* implicit */ LoadedInode(InodeBase* inode) : inode_(inode) {}

    LoadedInode(LoadedInode&&) = default;
    LoadedInode& operator=(LoadedInode&&) = default;

    InodeBase* get() const {
      return inode_;
    }

    InodePtr getPtr() const {
      // Calling InodePtr::newPtrLocked is safe because interacting with
      // LoadedInode implies the data_ lock is held.
      return InodePtr::newPtrLocked(inode_);
    }

    InodeBase* operator->() const {
      return inode_;
    }

    InodeBase& operator*() const {
      return *inode_;
    }

   private:
    LoadedInode(const LoadedInode&) = delete;
    LoadedInode& operator=(const LoadedInode&) = delete;

    InodeBase* inode_{nullptr};
  };

  struct Members {
    /**
     * The map of loaded inodes
     *
     * This map stores raw pointers rather than InodePtr objects.  The InodeMap
     * itself does not hold a reference to the Inode objects.  When an Inode is
     * looked up the InodeMap will wrap the Inode in an InodePtr so that the
     * caller acquires a reference.
     */
    std::unordered_map<InodeNumber, LoadedInode> loadedInodes_;

    /**
     * The map of currently unloaded inodes
     */
    std::unordered_map<InodeNumber, UnloadedInode> unloadedInodes_;

    /**
     * Indicates if the FS mount point has been unmounted.
     *
     * If this is true then the FS refcount on all inodes should be treated
     * as 0, and we can forget all inodes while shutting down.
     */
    bool isUnmounted_{false};

    /**
     * The number of loaded TreeInode objects
     */
    size_t numTreeInodes_{0};

    /**
     * The number of loaded FileInode objects
     * Note: We could remove this counter: numTreeInodes_ + numFileInodes_
     * should always equal loadedInodes_.size(), so we could compute it.
     * For now we track it just to allow us to assert that this invariant does
     * hold true to make sure our calculations are correct.
     */
    size_t numFileInodes_{0};

    /**
     * Running total of inodes forgotten outright by clearFsRefcount() while
     * unloaded, which the GC sweep therefore never counts as unloaded.
     */
    size_t numForgottenInodes_{0};

    /**
     * A promise to fulfill once shutdown() completes.
     *
     * This is only initialized when shutdown() is called, and will be
     * std::nullopt until we are shutting down.
     *
     * In the future we could update this to just use an empty promise to
     * indicate that we are not shutting down yet.  However, currently
     * folly::Promise does not have a simple API to check if it is empty or not,
     * so we have to wrap it in a std::optional.
     */
    std::optional<folly::Promise<folly::Unit>> shutdownPromise;
  };

  InodeMap(InodeMap const&) = delete;
  InodeMap& operator=(InodeMap const&) = delete;

  void shutdownComplete(folly::Synchronized<Members>::LockedPtr&& data);

  void setupParentLookupPromise(
      folly::Promise<InodePtr>& promise,
      PathComponentPiece childName,
      bool isUnlinked,
      InodeNumber childInodeNumber,
      std::optional<ObjectId> id,
      mode_t mode,
      const ObjectFetchContextPtr& context);
  void startChildLookup(
      const InodePtr& parent,
      PathComponentPiece childName,
      bool isUnlinked,
      InodeNumber childInodeNumber,
      std::optional<ObjectId> id,
      mode_t mode,
      const ObjectFetchContextPtr& context);

  /**
   * Create and return inode load start event that will later be published to
   * tracebus for telemetry. Additionally sets the unloaded inode's
   * loadStartTime timestamp for when the start event began. This function
   * should be called while holding the data_ write lock, and the event should
   * be published after releasing the lock.
   */
  InodeTraceEvent createInodeLoadStartEvent(
      InodeNumber number,
      UnloadedInode& unloadedData,
      const folly::Synchronized<Members>::WLockedPtr& data);

  /**
   * Create and return an inode load failure event that will later be published
   * to tracebus for telemetry. This method acquires a read lock on data_. It
   * should never be called while already holding the lock. The function returns
   * std::nullopt if failing to find the inode number passed in.
   */
  std::optional<InodeTraceEvent> createInodeLoadFailEvent(InodeNumber number);

  /**
   * Extract the list of promises waiting on the specified inode number to be
   * loaded.
   *
   * This method acquires the data_ lock internally.
   * It should never be called while already holding the lock.
   */
  PromiseVector extractPendingPromises(InodeNumber number);

  std::optional<RelativePath> getPathForInodeHelper(
      InodeNumber inodeNumber,
      const folly::Synchronized<Members>::RLockedPtr& data);

  /**
   * Unload an inode
   *
   * This simply removes it from the loadedInodes_ map and, if it is still
   * referenced by the FS, adds it to the unloadedInodes_ map.
   *
   * The caller is responsible for actually deleting the Inode object after
   * releasing the InodeMap lock.
   */
  void unloadInode(
      InodeBase* inode,
      TreeInode* parent,
      PathComponentPiece name,
      bool isUnlinked,
      const folly::Synchronized<Members>::LockedPtr& lock);

  /**
   * Update the overlay data for an inode before unloading it.
   * This is called as the first step of unloadInode().
   *
   * This returns an UnloadedInode if we need to remember this inode in the
   * unloadedInodes_ map, or std::nullopt if we can forget about it completely.
   */
  std::optional<UnloadedInode> updateOverlayForUnload(
      InodeBase* inode,
      TreeInode* parent,
      PathComponentPiece name,
      bool isUnlinked,
      const folly::Synchronized<Members>::LockedPtr& lock);

  bool hasRememberedChildForUnload(
      const TreeInode& inode,
      const folly::Synchronized<Members>::LockedPtr& lock) const;

  void insertLoadedInode(
      const folly::Synchronized<Members>::LockedPtr& data,
      InodeBase* inode);

  void eraseLoadedInode(
      const folly::Synchronized<Members>::LockedPtr& data,
      InodeBase* inode);

  UnloadedInode& insertUnloadedInode(
      const folly::Synchronized<Members>::LockedPtr& data,
      InodeNumber ino,
      UnloadedInode&& unloadedInode);

  void eraseUnloadedInode(
      const folly::Synchronized<Members>::LockedPtr& data,
      std::unordered_map<InodeNumber, UnloadedInode>::iterator iter);

  /**
   * Verify the InodeMap precondition and initialize the root_ member.
   */
  void initializeRoot(
      const folly::Synchronized<Members>::LockedPtr& data,
      TreeInodePtr root);

  /**
   * Construct an UnloadedInode and insert it onto the unloadedInodes_ map.
   *
   * Will throw a std::runtime_error if the passed in InodeNumber is already
   * known by the InodeMap.
   *
   * The argument list will be directly passed in to the UnloadedInode
   * constructor.
   */
  template <class... Args>
  void initializeUnloadedInode(
      const folly::Synchronized<Members>::LockedPtr& data,
      InodeNumber parentIno,
      InodeNumber ino,
      Args&&... args);

  /**
   * For unloaded inodes, this decrements the inode fs refcount.
   * For loaded inodes this returns the inode to decrement the FS refcount on
   * because it is not safe to decrement the refcount while holding the data
   * lock. For loaded inodes this does not decrement the fs refcount!
   * WARNING: The returned inodePtr must be destroyed OUTSIDE of the data lock!
   */
  InodePtr decFsRefcountHelper(
      folly::Synchronized<Members>::LockedPtr& data,
      InodeNumber number,
      uint32_t count = 0,
      bool clearRefCount = false);

  /**
   * The EdenMount that owns this InodeMap.
   */
  EdenMount* const mount_{nullptr};

  std::shared_ptr<ReloadableConfig> config_;
  EdenStatsPtr stats_;
  std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger_;

  /**
   * The root inode.
   *
   * This member never changes after the InodeMap is initialized.
   * It is therefore safe to access without locking.
   */
  TreeInodePtr root_;

  /**
   * The locked data.
   *
   * Note: be very careful to hold this lock only when necessary.  No other
   * locks should be acquired when holding this lock.  In particular this means
   * that we should never access any InodeBase objects while holding the lock,
   * since we should not hold our lock while an InodeBase acquires its own
   * internal lock.  (This makes it safe for InodeBase to perform operations on
   * the InodeMap while holding their own lock.)
   */
  folly::Synchronized<Members> data_;

  /**
   * Total number of inodes tracked by this InodeMap (loaded + unloaded).
   * Maintained outside data_ to allow lock-free reads on hot paths.
   * Updated by insertLoadedInode/eraseLoadedInode and
   * insertUnloadedInode/eraseUnloadedInode.
   */
  std::atomic<size_t> totalInodeCount_{0};

  /**
   * The number of inodes that we have unloaded with our periodic
   * unlinked inode unloading. Periodic unlinked inode unloading is run after
   * operations that unlink lots of inodes like checkout on NFS mounts.
   *
   * This number will only increase for the life time of this inode map.
   */
  std::atomic<size_t> numPeriodicallyUnloadedUnlinkedInodes_{0};

  /**
   * The number of inodes that we have unloaded with our periodic
   * linked inode unloading. Periodic linked inode unloading can be run
   * at regular intervals on any mount type.
   *
   * This number will only increase for the life time of this inode map.
   */
  std::atomic<size_t> numPeriodicallyUnloadedLinkedInodes_{0};
};

/**
 * An opaque class so that InodeMap can return its lock to the TreeInode
 * in order to make multiple calls to unloadInode() without releasing and
 * re-acquiring the lock.
 *
 * This mostly exists to make forward declarations simpler.
 */
class InodeMapLock {
 public:
  explicit InodeMapLock(
      folly::Synchronized<InodeMap::Members>::LockedPtr&& data)
      : data_(std::move(data)) {}

  void unlock() {
    data_.unlock();
  }

 private:
  friend class InodeMap;
  folly::Synchronized<InodeMap::Members>::LockedPtr data_;
};
} // namespace facebook::eden
