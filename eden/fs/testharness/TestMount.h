/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/Portability.h>
#include <folly/Range.h>
#include <folly/executors/ManualExecutor.h>
#include <sys/stat.h>
#include <functional>
#include <optional>
#include <vector>

#include "eden/common/utils/CaseSensitivity.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/fs/fuse/FuseDispatcher.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/InodeNumber.h"
#include "eden/fs/inodes/InodePtr.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/inodes/overlay/gen-cpp2/overlay_types.h"
#include "eden/fs/model/TreeEntry.h"
#include "eden/fs/testharness/FakeClock.h"

namespace folly {
struct Unit;
class ManualExecutor;

namespace test {
class TemporaryDirectory;
}
} // namespace folly

namespace facebook::eden {

class BlobCache;
class TreeCache;
class CheckoutConfig;
class ErrorLogger;
class FakeBackingStore;
class FakeWalkDetector;
class FakeFuse;
class FakePrivHelper;
class FakeTreeBuilder;
class FileInode;
class FuseDispatcher;
class TestConfigSource;
class TreeInode;
class MockInodeAccessLogger;
template <typename T>
class StoredObject;
using StoredId = StoredObject<ObjectId>;

struct TestMountFile {
  RelativePath path;
  std::string contents;
  uint8_t rwx = 0b110;
  TreeEntryType type = TreeEntryType::REGULAR_FILE;

  /** Performs a structural equals comparison. */
  bool operator==(const TestMountFile& other) const;

  /**
   * @param p is a StringPiece (rather than a RelativePath) for convenience
   *     for creating instances of TestMountFile for unit tests.
   */
  TestMountFile(folly::StringPiece p, folly::StringPiece c)
      : path(p), contents(c.str()) {}
};

class TestMount {
 public:
  /**
   * Create a new uninitialized TestMount.
   *
   * The TestMount will not be fully initialized yet.  The caller must
   * populate the object store as desired, and then call initialize() to
   * create the underlying EdenMount object once the commit has been set up.
   *
   * enableActivityBuffer can be passed to override if the ActivityBuffer should
   * be active in the TestMount. This is default set to true but some tests
   * might fail with the ActivityBuffer enabled (i.e. InodePtr because inode
   * reference counts might be inaccurate if paths to store in the
   * ActivityBuffer are calculated concurrently), so we must turn it off then.
   *
   * configureEdenConfig is applied to the EdenConfig before ServerState and
   * the mount's ObjectStore are constructed (and to replacement test configs),
   * so construction-time config snapshots see its values.
   */
  TestMount(
      bool enableActivityBuffer = true,
      CaseSensitivity caseSensitivity = kPathMapDefaultCaseSensitive,
      std::shared_ptr<ErrorLogger> errorLogger = nullptr,
      std::function<void(EdenConfig&)> configureEdenConfig = {});

  /**
   * Create a new TestMount
   *
   * If startReady is true, all of the Tree and Blob objects created by the
   * rootBuilder will be made immediately ready in the FakeBackingStore.  If
   * startReady is false the objects will not be ready, and attempts to
   * retrieve them from the backing store will not complete until the caller
   * explicitly marks them ready.
   *
   * However, the root Tree object is always marked ready.  This is necessary
   * to create the EdenMount object.
   *
   * If an initialCommitId is not explicitly specified, makeTestId("1")
   * will be used.
   *
   * enableActivityBuffer can be set false to turn off the ActivityBuffer in the
   * TestMount if needed, preventing any tracebus subscriptions for it.
   */
  explicit TestMount(
      FakeTreeBuilder& rootBuilder,
      bool startReady = true,
      bool enableActivityBuffer = true,
      CaseSensitivity caseSensitivity = kPathMapDefaultCaseSensitive,
      std::shared_ptr<ErrorLogger> errorLogger = nullptr,
      std::function<void(EdenConfig&)> configureEdenConfig = {});
  explicit TestMount(
      FakeTreeBuilder&& rootBuilder,
      bool enableActivityBuffer = true,
      CaseSensitivity caseSensitivity = kPathMapDefaultCaseSensitive);
  TestMount(
      const RootId& initialCommitId,
      FakeTreeBuilder& rootBuilder,
      bool startReady = true,
      bool enableActivityBuffer = true,
      CaseSensitivity caseSensitivity = kPathMapDefaultCaseSensitive);
  explicit TestMount(CaseSensitivity caseSensitivity);
  ~TestMount();

  /**
   * Initialize the mount.
   *
   * This should only be used if the TestMount was default-constructed.
   * The caller must have already defined the root commit.  The lastCheckoutTime
   * is read from the FakeClock.
   */
  void initialize(const RootId& initialCommitId) {
    initialize(initialCommitId, getClock().getTimePoint());
  }

  /**
   * Initialize the mount.
   *
   * This should only be used if the TestMount was default-constructed.
   * The caller must have already defined the root commit.
   */
  void initialize(
      const RootId& initialCommitId,
      std::chrono::system_clock::time_point lastCheckoutTime);

  /**
   * Initialize the mount.
   *
   * This should only be used if the TestMount was default-constructed.
   * The caller must have already defined the root Tree in the object store.
   */
  void initialize(const RootId& initialCommitId, ObjectId rootTreeId);

  /**
   * Initialize the mount from the given root tree.
   *
   * This should only be used if the TestMount was default-constructed.
   *
   * If an initialCommitId is not explicitly specified, makeTestId("1")
   * will be used.
   */
  void initialize(
      const RootId& initialCommitId,
      FakeTreeBuilder& rootBuilder,
      bool startReady = true,
      InodeCatalogType inodeCatalogType = kInodeCatalogTypeDefault);
  void initialize(FakeTreeBuilder& rootBuilder, bool startReady = true);

  /**
   * Classify the mount as served over the given protocol: forwarded to
   * EdenMount::initialize() as the takeover mount protocol, which overrides
   * the checkout config (and, unlike the config path, does not depend on
   * platform defaults like experimental:enable-nfs-server). Must be called
   * before the mount is initialized, i.e. on a default-constructed
   * TestMount.
   */
  void setMountProtocol(MountProtocol protocol) {
    mountProtocol_ = protocol;
  }

  /**
   * Like initialize, except EdenMount::initialize is not called.
   *
   * This should only be used if the TestMount was default-constructed.
   */
  void createMountWithoutInitializing(
      const RootId& initialCommitId,
      FakeTreeBuilder& rootBuilder,
      bool startReady,
      InodeCatalogType inodeCatalogType = kInodeCatalogTypeDefault);
  void createMountWithoutInitializing(
      FakeTreeBuilder& rootBuilder,
      bool startReady = true);

  /**
   * Perform FUSE initialization on the EdenMount.
   *
   * This function calls registerFakeFuse on your behalf.
   *
   * Preconditions:
   * - initialize() was called.
   */
  void startFuseAndWait(std::shared_ptr<FakeFuse>);

  /**
   * Get the CheckoutConfig object.
   *
   * The CheckoutConfig object provides methods to get the paths to the mount
   * point, the client directory, etc.
   */
  const CheckoutConfig* getConfig() const {
    // Ownership of the config varies - we own it first and then move it into
    // edenMount_.
    return edenMount_ ? edenMount_->getCheckoutConfig() : config_.get();
  }

  /**
   * Updates the EdenConfig. Keys are of the form section:setting and values
   * must be parseable by the ConfigSetting.
   */
  void updateEdenConfig(const std::map<std::string, std::string>& values);

  /**
   * The mount's walk detector, which records what it is told for tests.
   */
  FakeWalkDetector& getWalkDetector() const;

  /**
   * Callers can use this to populate the BackingStore before calling build().
   */
  const std::shared_ptr<FakeBackingStore>& getBackingStore() const {
    return backingStore_;
  }

  const std::shared_ptr<BlobCache>& getBlobCache() const {
    return blobCache_;
  }

  const std::shared_ptr<TreeCache>& getTreeCache() const {
    return treeCache_;
  }

  const std::shared_ptr<InodeAccessLogger>& getInodeAccessLogger() const {
    return serverState_->getInodeAccessLogger();
  }

#ifndef _WIN32
  FuseDispatcher* getDispatcher() const;
#endif // !_WIN32

  /**
   * Access to the TestMount's FakeClock which is referenced by the underlying
   * EdenMount (and thus inodes).
   */
  FakeClock& getClock() {
    return *clock_;
  }

  /**
   * Re-create the EdenMount object, simulating a scenario where it was
   * unmounted and then remounted.
   *
   * Note that if the caller is holding references to the old EdenMount object
   * this will prevent it from being destroyed.  This may result in an error
   * trying to create the new EdenMount if the old mount object still exists
   * and is still holding a lock on the overlay or other data structures.
   *
   * When simulateUncleanShutdown is true, the overlay's saved next inode
   * number is deleted after the old mount is destroyed, so the new mount
   * takes the same fsck recovery path as a mount whose daemon crashed. The
   * InMemory catalog used on Windows keeps no such file and rebuilds the
   * overlay from the disk on every start, so the option does nothing there.
   */
  void remount(bool simulateUncleanShutdown = false);

#ifndef _WIN32
  /**
   * Simulate an edenfs daemon takeover for this mount.
   */
  void remountGracefully();
#endif

  /**
   * Add file to the mount; it will be available in the overlay.
   */
  void addFile(folly::StringPiece path, folly::StringPiece contents);

  void mkdir(folly::StringPiece path);

  /** Overwrites the contents of an existing file. */
  FileInodePtr overwriteFile(
      folly::StringPiece path,
      folly::StringPiece contents);

  /** Does the equivalent of mv(1). */
  void move(folly::StringPiece src, folly::StringPiece dest);

  std::string readFile(folly::StringPiece path);

  /** Returns true if path identifies a regular file in the tree. */
  bool hasFileAt(folly::StringPiece path);

  void deleteFile(folly::StringPiece path);
  void rmdir(folly::StringPiece path);

#ifndef _WIN32
  /**
   * Create symlink named path pointing to pointsTo or throw exception if fail
   */
  void addSymlink(folly::StringPiece path, folly::StringPiece pointsTo);

  void chmod(folly::StringPiece path, mode_t permissions);
#endif

  InodePtr getInode(RelativePathPiece path) const;
  InodePtr getInode(folly::StringPiece path) const;
  TreeInodePtr getTreeInode(RelativePathPiece path) const;
  TreeInodePtr getTreeInode(folly::StringPiece path) const;
  FileInodePtr getFileInode(RelativePathPiece path) const;
  FileInodePtr getFileInode(folly::StringPiece path) const;
  VirtualInode getVirtualInode(RelativePathPiece path) const;
  VirtualInode getVirtualInode(folly::StringPiece path) const;

  /**
   * Walk the entire tree and load all inode objects.
   */
  void loadAllInodes();
  [[nodiscard]] ImmediateFuture<folly::Unit> loadAllInodesFuture();

  /**
   * Load all inodes [recursively] under the specified subdirectory.
   */
  static void loadAllInodes(const TreeInodePtr& treeInode);
  [[nodiscard]] static ImmediateFuture<folly::Unit> loadAllInodesFuture(
      const TreeInodePtr& treeInode);

  /** Convenience method for getting the Tree for the root of the mount. */
  std::shared_ptr<const Tree> getRootTree() const;

  std::shared_ptr<EdenMount>& getEdenMount() & noexcept {
    return edenMount_;
  }

  const std::shared_ptr<EdenMount>& getEdenMount() const& {
    return edenMount_;
  }

  TreeInodePtr& getRootInode() & {
    return rootInode_;
  }

#ifndef _WIN32
  const std::shared_ptr<FakePrivHelper>& getPrivHelper() const {
    return privHelper_;
  }
#endif // !_WIN32

  void registerFakeFuse(std::shared_ptr<FakeFuse> fuse);

  const std::shared_ptr<ServerState>& getServerState() const {
    return serverState_;
  }

  /**
   * Get an id to use for the next commit.
   *
   * This mostly just helps pick easily readable commit IDs that increment
   * over the course of a test.
   *
   * This returns "0000000000000000000000000000000000000001" on the first call,
   * "0000000000000000000000000000000000000002" on the second, etc.
   */
  RootId nextCommitId();

  /**
   * Helper function to create a commit from a FakeTreeBuilder and call
   * EdenMount::resetCommit() with the result.
   */
  void resetCommit(FakeTreeBuilder& builder, bool setReady);
  void
  resetCommit(const RootId& commitId, FakeTreeBuilder& builder, bool setReady);

  /**
   * Returns true if the overlay contains a directory record for this inode.
   */
  bool hasOverlayDir(InodeNumber inodeNumber) const;

  /**
   * Returns true if the inode metadata table has an entry for this inode.
   */
  bool hasMetadata(InodeNumber inodeNumber) const;

  /**
   * Check out the mount's root to `snapshotId`. The returned future runs on
   * the server executor: drain it with drainServerExecutor() to make the
   * future ready.
   */
  folly::Future<CheckoutResult> checkout(
      const RootId& snapshotId,
      const ObjectFetchContextPtr& fetchContext,
      folly::StringPiece thriftMethodCaller,
      CheckoutMode checkoutMode = CheckoutMode::NORMAL);

  /**
   * Returns number of functions executed.
   */
  size_t drainServerExecutor();

  std::shared_ptr<folly::ManualExecutor> getServerExecutor() const {
    return serverExecutor_;
  }

 private:
  void createMount(
      InodeCatalogType inodeCatalogType = kInodeCatalogTypeDefault);
  void initTestDirectory(CaseSensitivity caseSensitivity);
  void setInitialCommit(const RootId& commitId);
  void setInitialCommit(const RootId& commitId, ObjectId rootTreeId);

  /**
   * Initialize the Eden mount. This is an internal function to initialize and
   * start the EdenMount.
   */
  void initializeEdenMount();

  /**
   * The temporary directory for this TestMount.
   *
   * This must be stored as a member variable to ensure the temporary directory
   * lives for the duration of the test.
   *
   * We intentionally list it before the edenMount_ so it gets constructed
   * first, and destroyed (and deleted from disk) after the EdenMount is
   * destroyed.
   */
  std::unique_ptr<folly::test::TemporaryDirectory> testDir_;

  std::shared_ptr<EdenMount> edenMount_;
  std::optional<MountProtocol> mountProtocol_;
  TreeInodePtr rootInode_;

#ifndef _WIN32
  std::unique_ptr<FuseDispatcher> dispatcher_;
#endif
  std::shared_ptr<FakeBackingStore> backingStore_;
  EdenStatsPtr stats_;
  std::shared_ptr<BlobCache> blobCache_;
  std::shared_ptr<TreeCache> treeCache_;
  std::shared_ptr<TestConfigSource> testConfigSource_;
  std::shared_ptr<EdenConfig> edenConfig_;
  std::function<void(EdenConfig&)> configureEdenConfig_;

  /*
   * config_ is only set before edenMount_ has been initialized.
   * When edenMount_ is created we pass ownership of the config to edenMount_.
   */
  std::unique_ptr<CheckoutConfig> config_;

  /**
   * A counter for creating temporary commit ids via the nextCommitId()
   * function.
   *
   * This is atomic just in case, but in general I would expect most tests to
   * perform all TestMount manipulation from a single thread.
   */
  std::atomic<uint64_t> commitNumber_{1};

  std::shared_ptr<ErrorLogger> errorLogger_;
  std::shared_ptr<FakeClock> clock_ = std::make_shared<FakeClock>();
  std::shared_ptr<FakePrivHelper> privHelper_;

  // The ManualExecutor must be destroyed prior to the EdenMount. Otherwise,
  // when clearing its queue, it will deallocate functions with captured values
  // that still reference the EdenMount (or its owned objects).
  std::shared_ptr<folly::ManualExecutor> serverExecutor_;

  std::shared_ptr<ServerState> serverState_;
};
} // namespace facebook::eden
