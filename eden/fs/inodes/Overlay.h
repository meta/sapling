/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once
#include <folly/File.h>
#include <folly/Function.h>
#include <folly/Range.h>
#include <folly/Synchronized.h>
#include <folly/container/F14Map.h>
#include <folly/futures/Future.h>
#include <folly/futures/Promise.h>
#include <folly/synchronization/Baton.h>
#include <folly/synchronization/LifoSem.h>
#include <array>
#include <atomic>
#include <condition_variable>
#include <functional>
#include <mutex>
#include <optional>
#include <thread>
#include <unordered_map>

#include "eden/common/utils/CaseSensitivity.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/fs/config/InodeCatalogType.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/inodes/InodeNumber.h"
#include "eden/fs/inodes/overlay/OverlayChecker.h"
#include "eden/fs/inodes/overlay/gen-cpp2/overlay_types.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"

#ifndef _WIN32
#include "eden/fs/inodes/fscatalog/EphemeralFsInodeCatalog.h"
#include "eden/fs/inodes/fscatalog/FsInodeCatalog.h"
#endif

namespace facebook::eden {

namespace overlay {
class OverlayDir;
}

struct DirContents;
class InodeMap;
class SerializedInodeMap;
class InodeCatalog;
class FileContentStore;
class DirEntry;
class EdenConfig;
class EdenStats;
class ErrorLogger;

using EdenStatsPtr = RefPtr<EdenStats>;

#ifndef _WIN32
struct InodeMetadata;
template <typename T>
class InodeTable;
using InodeMetadataTable = InodeTable<InodeMetadata>;
class OverlayFile;
#endif

/** Manages the write overlay storage area.
 *
 * The overlay is where we store files that are not yet part of a snapshot.
 *
 * The contents of this storage layer are overlaid on top of the object store
 * snapshot that is active in a given mount point.
 *
 * There is one overlay area associated with each eden client instance.
 *
 * We use the Overlay to manage mutating the structure of the checkout;
 * each time we create or delete a directory entry, we do so through
 * the overlay class.
 *
 * The Overlay class keeps track of the mutated tree; if we mutate some
 * file "foo/bar/baz" then the Overlay records metadata about the list
 * of files in the root, the list of files in "foo", the list of files in
 * "foo/bar" and finally materializes "foo/bar/baz".
 */
class Overlay : public std::enable_shared_from_this<Overlay> {
 public:
  /**
   * Create a new Overlay object.
   *
   * The caller must call initialize() after creating the Overlay and wait for
   * it to succeed before using any other methods.
   */
  static std::shared_ptr<Overlay> create(
      AbsolutePathPiece localDir,
      CaseSensitivity caseSensitive,
      InodeCatalogType inodeCatalogType,
      std::shared_ptr<EdenFsEventsLogger> logger,
      ErrorLogger& errorLogger,
      EdenStatsPtr stats,
      const EdenConfig& config);

  ~Overlay();

  Overlay(const Overlay&) = delete;
  Overlay(Overlay&&) = delete;
  Overlay& operator=(const Overlay&) = delete;
  Overlay& operator=(Overlay&&) = delete;

  /**
   * Initialize the overlay.
   *
   * This must be called after the Overlay constructor, before performing
   * operations on the overlay.
   *
   * This may be a slow operation and may perform significant amounts of
   * disk I/O.
   *
   * The initialization operation may include:
   * - Acquiring a lock to ensure no other processes are accessing the on-disk
   *   overlay state
   * - Creating the initial on-disk overlay data structures if necessary.
   * - Verifying and fixing the on-disk data if the Overlay was not shut down
   *   cleanly the last time it was opened.
   * - Upgrading the on-disk data from older formats if the Overlay was created
   *   by an older version of the software.
   */
  [[nodiscard]] folly::SemiFuture<folly::Unit> initialize(
      const std::shared_ptr<ReloadableConfig>& config,
      std::optional<AbsolutePath> mountPath = std::nullopt,
      OverlayChecker::ProgressCallback&& progressCallback = [](auto) {},
      InodeCatalog::LookupCallback&& lookupCallback =
          [](auto, auto) {
            return makeImmediateFuture<InodeCatalog::LookupCallbackValue>(
                std::runtime_error("no lookup callback"));
          });

  /**
   * Closes the overlay. It is undefined behavior to access the
   * InodeMetadataTable concurrently or call any other Overlay method
   * concurrently with or after calling close(). The Overlay will try to detect
   * this with assertions but cannot always detect concurrent access.
   *
   * Returns the next available InodeNumber to be passed to any process taking
   * over an Eden mount.
   */
  void close();

  /**
   * Returns true if the Overlay has already been closed or is in the process of
   * closing. This function is primarily for debugging.
   */
  bool isClosed();

  /**
   * True if either a new overlay was created on disk or initialize() returned
   * after opening an overlay that had been cleanly shut down. False prior to
   * initialize() being called or after an unclean shutdown, including when
   * a reservation lets startup skip the offline consistency check.
   */
  bool hadCleanStartup() const {
    return hadCleanStartup_;
  }

  /**
   * Get the maximum inode number that has ever been allocated to an inode.
   */
  InodeNumber getMaxInodeNumber();

  bool usesOnlineFsck() const {
    return useOnlineFsck_;
  }

  /**
   * allocateInodeNumber() should only be called by TreeInode.
   *
   * This can be called:
   * - To allocate an inode number for an existing tree entry that does not
   *   need to be loaded yet.
   * - To allocate an inode number for a brand new inode being created by
   *   TreeInode::create() or TreeInode::mkdir().  In this case
   *   inodeCreated() should be called immediately afterwards to register the
   *   new child Inode object.
   */
  InodeNumber allocateInodeNumber();

  /**
   * Allocate a contiguous range of inode numbers.
   *
   * Returns the first inode number in the range. The allocated range is
   * [returned, returned + count). Uses a single atomic operation instead of
   * count separate increments.
   */
  InodeNumber allocateInodeNumbers(uint64_t count);

#ifndef _WIN32

  /**
   * Returns an InodeMetadataTable for accessing and storing inode metadata.
   * Owned by the Overlay so records can be removed when the Overlay discovers
   * it no longer needs data for an inode or its children.
   */
  InodeMetadataTable* getInodeMetadataTable() const {
    return inodeMetadataTable_.get();
  }

#endif // !_WIN32

  /**
   * Save a directory to the overlay.
   *
   * When isMaterialized is false, the directory contents match source control
   * and can be reconstructed from the backing store. In this case, the write
   * may use a faster but less crash-safe code path (direct write without
   * temp+rename) since data loss on crash is recoverable.
   *
   * Callers that flush WAL state into the base file (maybeCompactWal,
   * loadOverlayDir's WAL merge, recursive removal cleanup) must pass
   * isMaterialized=true explicitly. WAL-tracked directories are by
   * definition materialized — letting a non-crash-safe O_TRUNC rewrite
   * race with a crash on those directories would leave a truncated base
   * file plus a dropped WAL = lost user data.
   */
  void saveOverlayDir(
      InodeNumber inodeNumber,
      const DirContents& dir,
      bool isMaterialized = true);

  /*
   * Load content of the directory from overlay. If the directory does not
   * exist, this function will return an empty `DirContents`.
   *
   * A pending WAL is merged into the returned DirContents. The merged result is
   * also flushed back to the base file and the WAL removed, unless checkout
   * deferral is active (see enterCheckoutDeferral), in which case that rewrite
   * is left to a later load or to saveOverlayPostCheckout.
   */
  DirContents loadOverlayDir(InodeNumber inodeNumber);

  /**
   * While checkout deferral is active, loadOverlayDir() skips its opportunistic
   * read-side WAL flush; the WAL is instead cleared by saveOverlayPostCheckout
   * or replayed idempotently on a later load. Reference-counted so concurrent
   * checkout contexts (e.g. setPathObjectId, which does not serialize via
   * beginCheckout) compose correctly.
   */
  void enterCheckoutDeferral() noexcept {
    checkoutDeferralCount_.fetch_add(1, std::memory_order_release);
  }

  void exitCheckoutDeferral() noexcept {
    checkoutDeferralCount_.fetch_sub(1, std::memory_order_acq_rel);
  }

  void removeOverlayFile(InodeNumber inodeNumber);

  /**
   * Free the InodeMetadataTable record of an inode that has no overlay file,
   * if there is one. A no-op on Windows, which has no metadata table.
   */
  void freeInodeMetadata(InodeNumber inodeNumber);

  void removeOverlayDir(InodeNumber inodeNumber);

  /**
   * Remove the overlay data for the given tree inode and recursively remove
   * everything beneath it too.
   *
   * Must only be called on trees.
   */
  void recursivelyRemoveOverlayDir(InodeNumber inodeNumber);

  /**
   * Like recursivelyRemoveOverlayDir, but performs the work on the background
   * GC thread instead of blocking the caller.
   */
  void recursivelyRemoveOverlayDirBackground(InodeNumber inodeNumber);

  /**
   * Queue the removal of an unlinked inode's overlay data to the GC thread.
   * openFile, if set, is released there before the removal. Returns false
   * without queueing when overlay:background-inode-removal is off or the GC
   * thread is too far behind, so the caller removes the data itself and the
   * queue and its descriptors stay bounded.
   */
  bool removeOverlayDataInBackground(
      InodeNumber inodeNumber,
      bool isDir,
      std::shared_ptr<void>&& openFile);

  /**
   * Returns a future that completes once all previously-issued async
   * operations, namely recursivelyRemoveOverlayDir, finish.
   */
  folly::Future<folly::Unit> flushPendingAsync();

  bool hasOverlayDir(InodeNumber inodeNumber);

#ifndef _WIN32
  bool hasOverlayFile(InodeNumber inodeNumber);

  /**
   * Helper function that opens an existing overlay file,
   * checks if the file has valid header, and returns the file.
   */
  OverlayFile openFile(InodeNumber inodeNumber, folly::StringPiece headerId);

  /**
   * Open an existing overlay file without verifying the header.
   */
  OverlayFile openFileNoVerify(InodeNumber inodeNumber);

  /**
   * Helper function that creates an overlay file for a new FileInode.
   */
  OverlayFile createOverlayFile(
      InodeNumber inodeNumber,
      folly::ByteRange contents);

  /**
   * Helper function to write an overlay file for a FileInode with existing
   * contents.
   */
  OverlayFile createOverlayFile(
      InodeNumber inodeNumber,
      const folly::IOBuf& contents);

  /**
   * call statfs(2) on the filesystem in which the overlay is located
   */
  struct statfs statFs();
#endif // !_WIN32

  void addChild(
      InodeNumber parent,
      const std::pair<PathComponent, DirEntry>& childEntry,
      const DirContents& content);

  void removeChild(
      InodeNumber parent,
      PathComponentPiece childName,
      const DirContents& content);
  void removeChildren(InodeNumber parent, const DirContents& content);

  /**
   * Persist a rename using post-rename directory contents. For snapshot-backed
   * catalogs, dstContent must contain dstName; otherwise this reports a caller
   * bug before writing either directory.
   */
  void renameChild(
      InodeNumber src,
      InodeNumber dst,
      PathComponentPiece srcName,
      PathComponentPiece dstName,
      const DirContents& srcContent,
      const DirContents& dstContent);

  /**
   * Update a child entry in the overlay to mark it as materialized
   * (clear its source control hash). When WAL is enabled, appends a
   * MATERIALIZE entry to the WAL. When WAL is disabled, falls back
   * to a full saveOverlayDir write.
   */
  void materializeChild(
      InodeNumber parent,
      PathComponentPiece childName,
      const DirContents& content);

  /*
   * Returns a raw pointer to the inode catalog. This method should only be
   * used for testing.
   */
  InodeCatalog* getRawInodeCatalog() {
    return inodeCatalog_.get();
  }

  FileContentStore* getRawFileContentStore() {
    return fileContentStore_.get();
  }

  InodeCatalogType getInodeCatalogType() const {
    return inodeCatalogType_;
  }

  /**
   * Build a Thrift `overlay::OverlayDir` from in-memory `DirContents`.
   */
  [[deprecated(
      "Prefer direct serialization via InodeCatalog::saveOverlayEntries")]]
  overlay::OverlayDir serializeOverlayDir(
      InodeNumber inodeNumber,
      const DirContents& dir);

  const AbsolutePath& getLocalDir() const {
    return localDir_;
  }

  ErrorLogger& getErrorLogger() const {
    return errorLogger_;
  }

  void setFsckSemaphore(folly::LifoSem* sem) {
    fsckSemaphore_ = sem;
  }

  /**
   * Optional callback invoked inside the fsck critical section
   * (after semaphore acquire, before fsck work). Used by tests
   * to observe concurrency behavior.
   */
  void setFsckCallback(folly::Function<void()> cb) {
    fsckCallback_ = std::move(cb);
  }

  /**
   * Optional callback invoked just before the fsck semaphore wait.
   * Used by tests to know when a thread has reached the semaphore.
   */
  void setPreFsckSemaphoreCallback(folly::Function<void()> cb) {
    preFsckSemaphoreCallback_ = std::move(cb);
  }

 private:
  explicit Overlay(
      AbsolutePathPiece localDir,
      CaseSensitivity caseSensitive,
      InodeCatalogType inodeCatalogType,
      std::shared_ptr<EdenFsEventsLogger> logger,
      ErrorLogger& errorLogger,
      EdenStatsPtr stats,
      const EdenConfig& config);

  /**
   * Returns true if the overlay should use WAL (Write-Ahead Log) for
   * deferred directory writes. Computed once at construction as
   * `overlayUseWal` config flag AND `InodeCatalog::supportsWal()`.
   */
  bool useWal() const {
    return useWal_;
  }

  /**
   * Whether this overlay's backing catalog can have WAL files on disk.
   * Used for replay — we always replay WAL files if they exist,
   * regardless of whether WAL is currently enabled via config. This
   * ensures safe rollback when disabling the WAL.
   */
  bool canHaveWalFiles() const {
    return inodeCatalog_->supportsWal();
  }

  friend class OverlayTestHelper;

  /**
   * Inline compaction with a hard byte cap. On each call:
   *   - If `walFileSizeBytes >= walCompactionByteCap_`, compact
   *     unconditionally.
   *   - Else roll 1-in-max(`walMinCompactionThreshold_`,
   *     `walCompactionMultiplier_ * content.size()`) and compact on a hit.
   *     With no floor configured, roll
   *     1-in-`walCompactionMultiplier_ * max(content.size(), 10)`.
   *
   * The hard byte cap is a real upper bound on the on-disk WAL size; the
   * probabilistic roll keeps the typical-case compaction rate
   * proportional to directory size with no per-inode state.
   *
   * Inline (not background) by design: the existing `saveOverlayDir`
   * path also runs synchronously under the contents lock, so the worst
   * case matches pre-WAL behavior.
   *
   * Caller must hold the parent TreeInode's contents lock. Returns whether
   * the directory was compacted.
   */
  bool maybeCompactWal(
      InodeNumber parent,
      const DirContents& content,
      uint64_t walFileSizeBytes);

  /**
   * Append a WAL entry and return the WAL file's size afterwards. An entry
   * that does not fit the WAL format is persisted by rewriting the base
   * record instead, which retires the WAL; nothing is returned then.
   *
   * Callers must hold the parent TreeInode's contents lock so that
   * `content` is consistent with the WAL file on disk.
   */
  std::optional<uint64_t> appendWalEntryOrRewrite(
      InodeNumber parent,
      WalOpType op,
      PathComponentPiece childName,
      const overlay::OverlayEntry* entry,
      const DirContents& content);

  /**
   * Append a WAL entry and immediately roll for inline compaction. Every
   * WAL-using fast path in Overlay (`addChild`, `removeChild`,
   * `renameChild`, `materializeChild`) goes through this, except that a
   * rename across directories appends to both before rolling for either.
   * Bundling the two steps in one call ensures the compaction trigger
   * cannot be forgotten when a new WAL op is added later — a missed
   * `maybeCompactWal` would let the WAL grow unbounded for that op.
   *
   * Callers must hold the parent TreeInode's contents lock so that
   * `content` is consistent with the WAL file on disk.
   */
  void appendWalEntryAndCompact(
      InodeNumber parent,
      WalOpType op,
      PathComponentPiece childName,
      const overlay::OverlayEntry* entry,
      const DirContents& content);

  /**
   * A request for the background GC thread.  There are three types of
   * requests: recursively forget data underneath a given directory or
   * complete a promise.  The latter is used
   * for synchronization with the GC thread, primarily in unit tests.
   *
   * If additional request types are added in the future, consider renaming to
   * AsyncRequest.  However, recursive collection of forgotten inode numbers
   * is the only operation that can be made async while preserving our
   * durability goals.
   */
  struct GCRequest {
    explicit GCRequest(overlay::OverlayDir&& d) : requestType{std::move(d)} {}

    using FlushRequest = folly::Promise<folly::Unit>;
    explicit GCRequest(FlushRequest p) : requestType{std::move(p)} {}

    /**
     * Request to recursively remove an overlay directory tree in the
     * background. The GC thread will load, remove, and recurse into children.
     */
    explicit GCRequest(InodeNumber ino) : requestType{ino} {}

    /**
     * Request to remove the overlay data of one unlinked inode. openFile is
     * the cached open overlay file, if any, released on the GC thread so the
     * filesystem's inode eviction happens there too.
     */
    struct RemoveInodeRequest {
      InodeNumber ino;
      bool isDir;
      std::shared_ptr<void> openFile;
    };
    explicit GCRequest(RemoveInodeRequest req) : requestType{std::move(req)} {}

    std::variant<
        overlay::OverlayDir,
        FlushRequest,
        InodeNumber,
        RemoveInodeRequest>
        requestType;
  };

  struct GCQueue {
    bool stop = false;
    std::vector<GCRequest> queue;
  };

  void initOverlay(
      std::shared_ptr<ReloadableConfig> config,
      std::optional<AbsolutePath> mountPath,
      const OverlayChecker::ProgressCallback& progressCallback,
      InodeCatalog::LookupCallback& lookupCallback);
  void gcThread() noexcept;
  void handleGCRequest(GCRequest& request);

  // Serialize EdenFS overlay data structure into Thrift data structure
  overlay::OverlayEntry serializeOverlayEntry(const DirEntry& entry);

  using OverlayEntryVisitor = folly::FunctionRef<
      void(const std::string& name, const overlay::OverlayEntry& entry)>;
  using OverlayEntrySource =
      folly::FunctionRef<void(OverlayEntryVisitor visitor)>;

  /**
   * Iterate DirContents, validate each entry, serialize to OverlayEntry,
   * and pass to the visitor. This is the save-side counterpart to
   * buildDirEntries.
   */
  void visitDirEntries(
      InodeNumber inodeNumber,
      const DirContents& dir,
      OverlayEntryVisitor visitor);

  /**
   * Process overlay entries from the given source, handling inode allocation
   * and AppleDouble filtering. Appends processed entries to the output vector.
   * Returns true if the overlay should be rewritten and reports whether an
   * invalid entry was dropped.
   */
  bool buildDirEntries(
      InodeNumber inodeNumber,
      OverlayEntrySource source,
      folly::fbvector<std::pair<PathComponent, DirEntry>>& entries,
      bool& droppedCorruptEntry);

  void validateOverlayEntry(
      InodeNumber parentInodeNumber,
      std::string_view name,
      const overlay::OverlayEntry& entry,
      std::string_view source) const;

  bool tryIncOutstandingIORequests();
  void decOutstandingIORequests();
  void closeAndWaitForOutstandingIO();

#ifndef _WIN32
  void initializeInodeReservation(uint64_t nextInodeNumber);
  void ensureInodeReservation(uint64_t allocatedEnd);
  void saveInodeReservation(uint64_t reservation);

  const AbsolutePath& getOrCreateOnlineRepairDir();
  void archiveOverlayFile(
      InodeNumber inodeNumber,
      folly::StringPiece suffix,
      bool wal);
#endif
  void recoverOverlayDir(InodeNumber inodeNumber, const DirContents& dir);

  bool hadCleanStartup_{false};

  /**
   * The next inode number to allocate.  Zero indicates that neither
   * initializeFromTakeover nor getMaxRecordedInode have been called.
   *
   * Persists on disk on clean exit.
   *
   * This value will never be 1.
   */
  std::atomic<uint64_t> nextInodeNumber_{0};

#ifndef _WIN32
  /**
   * Inode reservation state, like `(nextInodeNumber / N + 1) * N`.
   *
   * Persists (+fsync-ed) on disk in all cases.
   * The on-disk state is designed to be always >= `nextInodeNumber`.
   */
  std::atomic<uint64_t> inodeReservation_{0};
  folly::Synchronized<folly::Unit, std::mutex> inodeReservationUpdateLock_;
#endif

  std::unique_ptr<FileContentStore> fileContentStore_;
  std::unique_ptr<InodeCatalog> inodeCatalog_;
  InodeCatalogType inodeCatalogType_;

  /**
   * Indicates if the backing overlay supports semantic operations, see
   * `InodeCatalog::supportsSemanticOperations` for more information.
   */
  bool supportsSemanticOperations_;

  bool filterAppleDouble_;

  const AbsolutePath localDir_;

#ifndef _WIN32
  /**
   * Disk-backed mapping from inode number to InodeMetadata.
   * Defined below inodeCatalog_ because it acquires its own file lock, which
   * should be released first during shutdown.
   */
  std::unique_ptr<InodeMetadataTable> inodeMetadataTable_;
#endif // !_WIN32

  /**
   * Wrapper around InodeMetadataTable::freeInode.
   *
   * This exist to limit the use of #ifdef to this function.
   */
  void freeInodeFromMetadataTable(InodeNumber ino);

  /**
   * Thread which recursively removes entries from the overlay underneath the
   * trees added to gcQueue_.
   */
  std::thread gcThread_;
  folly::Synchronized<GCQueue, std::mutex> gcQueue_;
  std::condition_variable gcCondVar_;

  /**
   * This uint64_t holds two values, a single bit on the MSB that
   * acts a boolean closed: True if the Overlay has been closed with
   * calling setClosed(). When this is true, reads and writes will throw an
   * error instead of applying an overlay change or read. On the rest of the
   * bits, the actual number of outstanding IO requests is held. This has been
   * done in order to synchronize these two variables and treat checking if the
   * overlay is closed and incrementing the IO reference count as a single
   * atomic action.
   */
  mutable std::atomic<uint64_t> outstandingIORequests_{0};

  folly::Baton<> lastOutstandingRequestIsComplete_;
  CaseSensitivity caseSensitive_;

  std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger_;
  // Borrowed from ServerState. Valid for Overlay's lifetime because
  // ServerState outlives all EdenMount instances.
  ErrorLogger& errorLogger_;
  EdenStatsPtr stats_;
  // Retained so renameChild can read the rollback setting after config reloads.
  std::shared_ptr<ReloadableConfig> reloadableConfig_;

  // Borrowed from EdenServer. Valid for Overlay's lifetime because
  // EdenServer::unmountAll() completes before semaphore destruction.
  folly::LifoSem* fsckSemaphore_{nullptr};

  folly::Function<void()> fsckCallback_;
  folly::Function<void()> preFsckSemaphoreCallback_;

  friend class IORequest;

  bool useInodeReservation_;
  bool useOnlineFsck_;

#ifndef _WIN32
  std::mutex onlineRepairMutex_;
  std::optional<AbsolutePath> onlineRepairDir_;
#endif

  bool useWal_{false};
  bool backgroundInodeRemoval_{false};
  size_t walCompactionMultiplier_{3};
  uint64_t walCompactionByteCap_{5'000'000};

  /**
   * Floor on the WAL compaction probability denominator; 0 uses the
   * previous formula. Gated by
   * experimental:overlay-wal-min-compaction-threshold.
   */
  size_t walMinCompactionThreshold_{0};
#ifndef _WIN32
  // Deliberate platform-scoped visibility switch: the pool's public API
  // and its private machinery both exist only on non-Windows builds.
 public:
  /**
   * Claim a pre-created overlay file (with its already-reserved inode
   * number) from the preallocation pool, writing `contents` into it if
   * non-empty. Returns nullopt when the pool is disabled, empty, or not
   * supported by the backing store; callers then fall back to
   * allocateInodeNumber() + createOverlayFile().
   */
  std::optional<std::pair<InodeNumber, OverlayFile>> tryClaimPreparedFile(
      folly::ByteRange contents);

  /**
   * Claim an inode number whose empty overlay directory record was already
   * written by the preallocation thread, so mkdir needs no overlay write
   * for the new child on the request path. Returns nullopt when the pool
   * is disabled or empty; callers then fall back to allocateInodeNumber()
   * + saveOverlayDir().
   */
  std::optional<InodeNumber> tryClaimPreparedDir();

 private:
  void preallocThreadLoop();
  void stopPreallocThread();

  struct PreparedFile {
    InodeNumber number;
    folly::File file;
  };

  /**
   * Pre-created overlay files for future file inodes: each entry's inode
   * number is already allocated (reserved exclusively for the pool) and its
   * on-disk file exists containing the standard file header, with the fd
   * positioned just past the header. Claiming one makes file creation free
   * of filesystem syscalls on the request path.
   */
  size_t filePreallocPoolSize_{0};
  size_t dirPreallocPoolSize_{0};
  std::mutex preallocMutex_;
  std::condition_variable preallocCondVar_;
  std::vector<PreparedFile> preallocPool_;
  std::vector<InodeNumber> preallocDirPool_;
  bool preallocStop_{false};
  std::thread preallocThread_;
#endif // !_WIN32

  /**
   * RNG used by `maybeCompactWal` to roll for inline compaction.
   * Production default uses `folly::Random::rand32()`. Tests inject a
   * deterministic generator via `OverlayTestHelper` so threshold checks
   * become reproducible.
   */
  std::function<uint32_t()> walCompactionRng_;

  /**
   * Drop the on-disk WAL file for `parent` after a full directory rewrite
   * or removal. Idempotent (safe to call when no WAL exists). No-op for
   * catalog types that never have WAL files on disk.
   */
  void clearWalAfterFullWrite(InodeNumber parent);

  /**
   * Replay any pending WAL entries for `parent` into `dir` so callers
   * that consume the loaded directory (e.g., recursive removal) see the
   * merged state.
   *
   * Steady-state cost is one fstatat (via hasWal) when no WAL exists;
   * the dynamic_cast and replay are skipped entirely. When a WAL does
   * exist, the cost is bounded by the inline-compaction threshold
   * (3 * max(dirSize, 10)) — the deferred work the WAL was hiding.
   *
   * Caller is responsible for cleaning up the on-disk WAL file
   * afterwards (e.g., via clearWalAfterFullWrite); this helper only
   * mutates `dir`.
   */
  void mergeWalIntoOverlayDir(InodeNumber parent, overlay::OverlayDir& dir);

  bool shouldDeferWalFlush() const noexcept {
    return checkoutDeferralCount_.load(std::memory_order_acquire) != 0;
  }

  // Number of in-flight checkout contexts deferring read-side WAL flush. Set on
  // checkout threads, read by parallel inode-load workers in loadOverlayDir().
  std::atomic<uint32_t> checkoutDeferralCount_{0};
};

/**
 * Used to reference count IO requests. In any place that there
 * is an overlay read or write, this struct should be constructed in order to
 * properly reference count and to properly deny overlay reads and
 * modifications in the case that the overlay is closed.
 */
class IORequest {
 public:
  explicit IORequest(Overlay* o) : overlay_{o} {
    if (!overlay_->tryIncOutstandingIORequests()) {
      throw std::system_error(
          EIO,
          std::generic_category(),
          "cannot access overlay after it is closed");
    }
  }

  ~IORequest() {
    overlay_->decOutstandingIORequests();
  }

 private:
  IORequest(IORequest&&) = delete;
  IORequest& operator=(IORequest&&) = delete;

  Overlay* const overlay_;
};

} // namespace facebook::eden
