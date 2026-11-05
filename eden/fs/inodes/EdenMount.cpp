/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/EdenMount.h"

#include <boost/filesystem.hpp>
#include <folly/ExceptionWrapper.h>
#include <folly/FBString.h>
#include <folly/File.h>
#include <folly/chrono/Conv.h>
#include <folly/coro/Collect.h>
#include <folly/coro/Invoke.h>
#include <folly/coro/Task.h>
#include <folly/coro/safe/NowTask.h>
#include <folly/futures/Future.h>
#include <folly/io/async/EventBase.h>
#include <folly/logging/Logger.h>
#include <folly/logging/xlog.h>
#include <folly/stop_watch.h>
#include <folly/system/ThreadName.h>
#include <gflags/gflags.h>

#include "eden/common/utils/Bug.h"
#include "eden/common/utils/ErrnoUtils.h"
#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/ImmediateFuture.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/SpawnedProcess.h"
#include "eden/common/utils/UnboundedQueueExecutor.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/MountProtocol.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/fuse/FuseChannel.h"
#include "eden/fs/inodes/CheckoutContext.h"
#include "eden/fs/inodes/EdenDispatcherFactory.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/FsChannel.h"
#include "eden/fs/inodes/InodeError.h"
#include "eden/fs/inodes/InodeMap.h"
#include "eden/fs/inodes/InodeTable.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/journal/Journal.h"
#include "eden/fs/model/Hash.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/model/git/GitIgnoreStack.h"
#include "eden/fs/model/git/TopLevelIgnores.h"
#include "eden/fs/nfs/NfsServer.h"
#include "eden/fs/nfs/Nfsd3.h"
#include "eden/fs/privhelper/PrivHelper.h"
#include "eden/fs/prjfs/PrjfsChannel.h"
#include "eden/fs/service/PrettyPrinters.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/store/BlobAccess.h"
#include "eden/fs/store/Diff.h"
#include "eden/fs/store/DiffCallback.h"
#include "eden/fs/store/DiffContext.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/store/StatsFetchContext.h"
#include "eden/fs/store/TreeLookupProcessor.h"
#include "eden/fs/telemetry/EdenErrorInfoBuilder.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/telemetry/LogEvent.h"
#include "eden/fs/utils/Clock.h"
#include "eden/fs/utils/EdenError.h"
#include "eden/fs/utils/FsChannelTypes.h"
#include "eden/fs/utils/NfsSocket.h"

#ifdef _WIN32
#include "eden/fs/utils/NotImplemented.h"
#endif

#include <chrono>
#include <memory>

using folly::Future;
using folly::makeFuture;
using folly::Try;
using folly::Unit;
using std::make_unique;
using std::shared_ptr;

DEFINE_string(
    edenfsctlPath,
    "edenfsctl",
    "the path to the edenfsctl executable");

namespace facebook::eden {

InodeTraceEvent::InodeTraceEvent(
    std::chrono::system_clock::time_point startTime,
    InodeNumber ino,
    InodeType inodeType,
    InodeEventType eventType,
    InodeEventProgress progress)
    : ino{ino}, inodeType{inodeType}, eventType{eventType}, progress{progress} {
  systemTime = (progress == InodeEventProgress::START)
      ? startTime
      : std::chrono::system_clock::now();
  duration = std::chrono::duration_cast<std::chrono::microseconds>(
      systemTime - startTime);
  monotonicTime = std::chrono::steady_clock::now();
}

void InodeTraceEvent::setPath(std::string_view stringPath) {
  path.reset(new char[stringPath.size() + 1]);
  memcpy(path.get(), stringPath.data(), stringPath.size());
  path[stringPath.size()] = 0;
}

// These static asserts exist to make explicit the memory usage of the per-mount
// InodeTraceBus. TraceBus uses 2 * capacity * sizeof(TraceEvent) memory usage,
// so limit total memory usage to around 0.67 MB per mount. Note
// InodeTraceEvents do include pointers to path info that is saved on the heap,
// and there is memory usage of this data outside of the mount (by the
// EdenServiceHandler during eden trace inode calls)
static_assert(CheckSize<InodeTraceEvent, 64>());

#ifndef _WIN32
namespace {
// We used to play tricks and hard link the .eden directory
// into every tree, but the linux kernel doesn't seem to like
// hard linking directories.  Now we create a symlink that resolves
// to the .eden directory inode in the root.
// The name of that symlink is `this-dir`:
// .eden/this-dir -> /abs/path/to/mount/.eden
constexpr PathComponentPiece kDotEdenSymlinkName{"this-dir"_pc};
} // namespace
#endif

/**
 * Helper for computing unclean paths when changing parents
 *
 * This DiffCallback instance is used to compute the set
 * of unclean files before and after actions that change the
 * current commit id of the mount point.
 */
class EdenMount::JournalDiffCallback : public DiffCallback {
 public:
  explicit JournalDiffCallback()
      : data_{std::in_place, std::unordered_set<RelativePath>()} {}

  void ignoredPath(RelativePathPiece, dtype_t) override {}

  void addedPath(RelativePathPiece, dtype_t) override {}

  void removedPath(RelativePathPiece path, dtype_t type) override {
    if (type != dtype_t::Dir) {
      data_.wlock()->uncleanPaths.insert(path.copy());
    }
  }

  void modifiedPath(RelativePathPiece path, dtype_t type) override {
    if (type != dtype_t::Dir) {
      data_.wlock()->uncleanPaths.insert(path.copy());
    }
  }

  void diffError(RelativePathPiece path, const folly::exception_wrapper& ew)
      override {
    // TODO: figure out what we should do to notify the user, if anything.
    // perhaps we should just add this path to the list of unclean files?
    XLOGF(
        WARNING,
        "error computing journal diff data for {}: {}",
        path,
        folly::exceptionStr(ew));
  }

  [[nodiscard]] ImmediateFuture<StatsFetchContext> performDiff(
      EdenMount* mount,
      TreeInodePtr rootInode,
      std::vector<std::shared_ptr<const Tree>> rootTrees,
      std::shared_ptr<CheckoutContext> ctx) {
    auto diffContext = mount->createDiffContext(
        this, folly::CancellationToken{}, ctx->getFetchContext());
    auto rawContext = diffContext.get();

    return rootInode
        ->diff(
            rawContext,
            RelativePathPiece{},
            std::move(rootTrees),
            rawContext->getToplevelIgnore(),
            false)
        .thenValue([diffContext = std::move(diffContext), rootInode](
                       folly::Unit) { return diffContext->getStatsContext(); });
  }

  [[nodiscard]] folly::coro::now_task<StatsFetchContext> co_performDiff(
      EdenMount* mount,
      TreeInodePtr rootInode,
      std::vector<std::shared_ptr<const Tree>> rootTrees,
      std::shared_ptr<CheckoutContext> ctx) {
    auto diffContext = mount->createDiffContext(
        this, folly::CancellationToken{}, ctx->getFetchContext());
    auto rawContext = diffContext.get();
    co_await rootInode->co_diff(
        rawContext,
        RelativePathPiece{},
        std::move(rootTrees),
        rawContext->getToplevelIgnore(),
        false);
    co_return diffContext->getStatsContext();
  }

  /** moves the Unclean Path information out of this diff callback instance,
   * rendering it invalid */
  std::unordered_set<RelativePath> stealUncleanPaths() {
    std::unordered_set<RelativePath> result;
    std::swap(result, data_.wlock()->uncleanPaths);

    return result;
  }

 private:
  struct Data {
    explicit Data(std::unordered_set<RelativePath>&& unclean)
        : uncleanPaths(std::move(unclean)) {}

    std::unordered_set<RelativePath> uncleanPaths;
  };
  folly::Synchronized<Data> data_;
};

static constexpr folly::StringPiece kEdenStracePrefix = "eden.strace.";

// We compute this when the process is initialized, but stash a copy
// in each EdenMount.  We may in the future manage to propagate enough
// state across upgrades or restarts that we can preserve this, but
// as implemented today, a process restart will invalidate any cached
// mountGeneration that a client may be holding on to.
// We take the bottom 16-bits of the pid and 32-bits of the current
// time and shift them up, leaving 16 bits for a mount point generation
// number.
static const uint64_t globalProcessGeneration =
    (uint64_t(getpid()) << 48) | (uint64_t(time(nullptr)) << 16);

// Each time we create an EdenMount we bump this up and OR it together
// with the globalProcessGeneration to come up with a generation number
// for a given mount instance.
static std::atomic<uint16_t> mountGeneration{0};

std::shared_ptr<EdenMount> EdenMount::create(
    std::unique_ptr<CheckoutConfig> config,
    std::shared_ptr<ObjectStore> objectStore,
    std::shared_ptr<BlobCache> blobCache,
    std::shared_ptr<ServerState> serverState,
    std::unique_ptr<Journal> journal,
    EdenStatsPtr stats,
    std::optional<InodeCatalogType> inodeCatalogType,
    std::shared_ptr<WalkDetector> walkDetector) {
  return std::shared_ptr<EdenMount>{
      new EdenMount{
          std::move(config),
          std::move(objectStore),
          std::move(blobCache),
          std::move(serverState),
          std::move(journal),
          std::move(stats),
          std::move(inodeCatalogType),
          std::move(walkDetector)},
      EdenMountDeleter{}};
}

void EdenMount::setWalkDetectorParent(const RootId& parent) {
  if (walkDetector_) {
    walkDetector_->workingCopyParentHint(parent);
  }
}

EdenMount::EdenMount(
    std::unique_ptr<CheckoutConfig> checkoutConfig,
    std::shared_ptr<ObjectStore> objectStore,
    std::shared_ptr<BlobCache> blobCache,
    std::shared_ptr<ServerState> serverState,
    std::unique_ptr<Journal> journal,
    EdenStatsPtr stats,
    std::optional<InodeCatalogType> inodeCatalogType,
    std::shared_ptr<WalkDetector> walkDetector)
    : checkoutConfig_{std::move(checkoutConfig)},
      serverState_{std::move(serverState)},
#ifdef _WIN32
      invalidationExecutor_{std::make_shared<UnboundedQueueExecutor>(
          serverState_->getEdenConfig()->prjfsNumInvalidationThreads.getValue(),
          "prjfs-dir-inval")},
#endif
      inodeMap_{new InodeMap(
          this,
          serverState_->getReloadableConfig(),
          stats.copy(),
          serverState_->getEdenFsEventsLogger())},
      inodePathCache_{
          serverState_->getEdenConfig()->inodePathCacheSize.getValue()},
      objectStore_{std::move(objectStore)},
      walkDetector_{std::move(walkDetector)},
      blobCache_{std::move(blobCache)},
      blobAccess_{objectStore_, blobCache_},
      overlay_{Overlay::create(
          checkoutConfig_->getOverlayPath(),
          checkoutConfig_->getCaseSensitive(),
          getInodeCatalogType(inodeCatalogType),
          serverState_->getEdenFsEventsLogger(),
          serverState_->getErrorLogger(),
          std::move(stats),
          *serverState_->getEdenConfig())},
#ifndef _WIN32
      overlayFileAccess_{
          overlay_.get(),
          serverState_->getEdenConfig()->overlayFileAccessCacheSize.getValue()},
#endif
      journal_{std::move(journal)},
      mountGeneration_{globalProcessGeneration | ++mountGeneration},
      straceLogger_{
          kEdenStracePrefix.str() + checkoutConfig_->getMountPath().value()},
      lastCheckoutTime_{EdenTimestamp{serverState_->getClock()->getRealtime()}},
      owner_{Owner{getuid(), getgid()}},
      inodeActivityBuffer_{initInodeActivityBuffer()},
      inodeTraceBus_{TraceBus<InodeTraceEvent>::create(
          "inode",
          serverState_->getEdenConfig()->InodeTraceBusCapacity.getValue())},
      clock_{serverState_->getClock()},
      scmStatusCache_{ScmStatusCache::create(
          serverState_->getReloadableConfig()->getEdenConfig().get(),
          serverState_->getStats().copy(),
          journal_)} {
  subscribeInodeActivityBuffer();
  updateInodePressurePolicy();
}

InodeCatalogType EdenMount::getInodeCatalogType(
    std::optional<InodeCatalogType> inodeCatalogType) {
  // InodCatalogType is determined by:
  //   1. optional parameter (`inodeCatalogType`) provided by caller - used by
  //        test code
  //   2. optional inode-catalog-type setting in CheckoutConfig
  //        (`checkoutConfig_->getInodeCatalogType()`)
  //   3. the EdenConfig default

  if (inodeCatalogType.has_value()) {
    return inodeCatalogType.value();
  }

  if (checkoutConfig_->getInodeCatalogType().has_value()) {
    return checkoutConfig_->getInodeCatalogType().value();
  }

  return serverState_->getEdenConfig()->inodeCatalogType.getValue();
}

[[nodiscard]] ImmediateFuture<folly::Unit> EdenMount::initialize(
    OverlayChecker::ProgressCallback&& progressCallback,
    const std::optional<SerializedInodeMap>& takeover,
    const std::optional<MountProtocol>& takeoverMountProtocol) {
  // it is an invariant that shouldUseNfs_ is set before we transition to
  // INITIALIZING
  calculateIsNfsMount(takeoverMountProtocol);

  transitionState(State::UNINITIALIZED, State::INITIALIZING);

  return makeImmediateFutureWith([&] {
           return initializeImpl(std::move(progressCallback), takeover);
         })
      .thenTry([this](auto&& result) {
        if (result.hasException()) {
          transitionState(State::INITIALIZING, State::INIT_ERROR);
        } else {
          transitionState(State::INITIALIZING, State::INITIALIZED);
        }
        return std::move(result);
      });
}

ImmediateFuture<folly::Unit> EdenMount::initializeImpl(
    OverlayChecker::ProgressCallback&& progressCallback,
    const std::optional<SerializedInodeMap>& takeover) {
  auto parentCommit = checkoutConfig_->getParentCommit();
  auto parent =
      parentCommit.getLastCheckoutId(ParentCommit::RootIdPreference::To)
          .value();

  static auto context = ObjectFetchContext::getNullContextWithCauseDetail(
      ObjectFetchContext::StaticCauseDetail::fromLiteral(
          "EdenMount::initialize"));
  return serverState_->getFaultInjector()
      .checkAsync("mount", getPath().view())
      .thenValue([this, parent](auto&&) {
        return objectStore_->getRootTree(parent, context);
      })
      .thenValue(
          [this,
           progressCallback = std::move(progressCallback),
           parent,
           parentCommit](ObjectStore::GetRootTreeResult parentTree) mutable {
            ParentCommitState::CheckoutState checkoutState =
                ParentCommitState::NoOngoingCheckout{};
            if (parentCommit.isCheckoutInProgress()) {
              checkoutState = ParentCommitState::InterruptedCheckout{
                  *parentCommit.getLastCheckoutId(
                      ParentCommit::RootIdPreference::From),
                  *parentCommit.getLastCheckoutId(
                      ParentCommit::RootIdPreference::To)};
            }

            auto wcParent = parentCommit.getWorkingCopyParent();

            *parentState_.wlock() = ParentCommitState{
                parent, parentTree.tree, wcParent, std::move(checkoutState)};

            setWalkDetectorParent(wcParent);

            // Record the transition from no snapshot to the current snapshot in
            // the journal.  This also sets things up so that we can carry the
            // snapshot id forward through subsequent journal entries.
            journal_->recordRootUpdate(parent);

            // Initialize the overlay.
            // This must be performed before we do any operations that may
            // allocate inode numbers, including creating the root TreeInode.
            return overlay_
                ->initialize(
                    serverState_->getReloadableConfig(),
                    getPath(),
                    std::move(progressCallback),
                    [this](
                        const std::shared_ptr<const Tree>& parentTree,
                        RelativePathPiece path) {
                      return ::facebook::eden::getTreeOrTreeEntry(
                          parentTree ? parentTree : getCheckedOutRootTree(),
                          path,
                          objectStore_,
                          context.copy());
                    })
                .deferValue([parentTree = std::move(parentTree.tree)](
                                auto&&) mutable { return parentTree; });
          })
      .thenValue([this, takeover](std::shared_ptr<const Tree> parentTree) {
        auto initTreeNode = createRootInode(std::move(parentTree));
        if (takeover) {
          inodeMap_->initializeFromTakeover(initTreeNode, *takeover);
        } else if (isWorkingCopyPersistent()) {
          inodeMap_->initializeFromOverlay(initTreeNode, *overlay_);
        } else {
          inodeMap_->initialize(initTreeNode);
        }

        // TODO: It would be nice if the .eden inode was created before
        // allocating inode numbers for the Tree's entries. This would give the
        // .eden directory inode number 2.
        return setupDotEden(std::move(initTreeNode));
      });
}

TreeInodePtr EdenMount::createRootInode(std::shared_ptr<const Tree> tree) {
  // Load the overlay, if present.
  auto rootOverlayDir = overlay_->loadOverlayDir(kRootNodeId);
  if (!rootOverlayDir.empty()) {
    // No id is necessary because the root is always materialized.
    return TreeInodePtr::makeNew(this, std::move(rootOverlayDir), std::nullopt);
  }

  return TreeInodePtr::makeNew(this, std::move(tree));
}

#ifndef _WIN32
namespace {
ImmediateFuture<Unit> ensureDotEdenSymlink(
    TreeInodePtr directory,
    const PathComponent& symlinkName,
    const AbsolutePath& symlinkTarget) {
  enum class Action {
    Nothing,
    CreateSymlink,
    UnlinkThenSymlink,
  };

  static auto context = ObjectFetchContext::getNullContextWithCauseDetail(
      ObjectFetchContext::StaticCauseDetail::fromLiteral(
          "ensureDotEdenSymlink"));
  return directory->getOrLoadChild(symlinkName, context)
      .thenTry([=](Try<InodePtr>&& result) -> ImmediateFuture<Action> {
        if (!result.hasValue()) {
          // If we failed to look up the file this generally means it
          // doesn't exist.
          // TODO: it would be nicer to actually check the exception to
          // confirm it is ENOENT.  However, if it was some other error the
          // symlink creation attempt below will just fail with some
          // additional details anyway.
          return Action::CreateSymlink;
        }

        auto fileInode = result->asFilePtrOrNull();
        if (!fileInode) {
          // Hmm, it's unexpected that we would have a directory here.
          // Just return for now, without trying to replace the directory.
          // We'll continue mounting the checkout, but this symlink won't be
          // set up.  This potentially could confuse applications that look
          // for it later.
          XLOGF(
              ERR,
              "error setting up .eden/{} symlink: a directory exists at this location",
              symlinkName);
          return Action::Nothing;
        }

        // If there is a regular file at this location, remove it then
        // create the symlink.
        if (dtype_t::Symlink != fileInode->getType()) {
          return Action::UnlinkThenSymlink;
        }

        // Check if the symlink already has the desired contents.
        return fileInode->readlink(context, CacheHint::LikelyNeededAgain)
            .thenValue([=](std::string&& contents) {
              if (contents == symlinkTarget) {
                // The symlink already contains the desired contents.
                return Action::Nothing;
              }
              // Remove and re-create the symlink with the desired contents.
              return Action::UnlinkThenSymlink;
            });
      })
      .thenValue([=](Action action) -> ImmediateFuture<Unit> {
        switch (action) {
          case Action::Nothing:
            return folly::unit;
          case Action::CreateSymlink:
            directory->symlink(
                symlinkName, symlinkTarget.view(), InvalidationRequired::Yes);
            return folly::unit;
          case Action::UnlinkThenSymlink:
            return directory
                ->unlink(symlinkName, InvalidationRequired::Yes, context)
                .thenValue([=](Unit&&) {
                  directory->symlink(
                      symlinkName,
                      symlinkTarget.view(),
                      InvalidationRequired::Yes);
                });
        }
        EDEN_BUG() << "unexpected action type when configuring .eden directory";
      })
      .thenTry([symlinkName](folly::Try<folly::Unit>&& try_) {
        if (try_.hasException()) {
          XLOGF(
              ERR,
              "error setting up .eden/{} symlink: {}",
              symlinkName,
              try_.exception().what());
          return ImmediateFuture<Unit>(std::move(try_).exception());
        }
        return ImmediateFuture<Unit>(folly::unit);
      });
}
} // namespace
#endif

ImmediateFuture<folly::Unit> EdenMount::setupDotEden(TreeInodePtr root) {
  // Set up the magic .eden dir
  static auto context = ObjectFetchContext::getNullContextWithCauseDetail(
      ObjectFetchContext::StaticCauseDetail::fromLiteral("setupDotEden"));
  return root->getOrLoadChildTree(PathComponentPiece{kDotEdenName}, context)
      .thenTry([=, this](Try<TreeInodePtr>&& lookupResult) {
        TreeInodePtr dotEdenInode;
        if (lookupResult.hasValue()) {
          dotEdenInode = *lookupResult;
        } else {
          dotEdenInode = root->mkdir(
              PathComponentPiece{kDotEdenName},
              0755,
              InvalidationRequired::Yes);
        }

        // Make sure all of the symlinks in the .eden directory exist and
        // have the correct contents.
        std::vector<ImmediateFuture<Unit>> futures;

#ifndef _WIN32
        futures.emplace_back(ensureDotEdenSymlink(
            dotEdenInode,
            kDotEdenSymlinkName.copy(),
            (checkoutConfig_->getMountPath() +
             PathComponentPiece{kDotEdenName})));
        futures.emplace_back(ensureDotEdenSymlink(
            dotEdenInode, "root"_pc.copy(), checkoutConfig_->getMountPath()));
        futures.emplace_back(ensureDotEdenSymlink(
            dotEdenInode, "socket"_pc.copy(), serverState_->getSocketPath()));
        futures.emplace_back(ensureDotEdenSymlink(
            dotEdenInode,
            "client"_pc.copy(),
            checkoutConfig_->getClientDirectory()));

        if (getEdenConfig()->findIgnoreInDotEden.getValue()) {
          futures.emplace_back(
              dotEdenInode->getOrLoadChild(".find-ignore"_pc, context)
                  .unit()
                  .thenError([dotEdenInode](auto&&) {
                    dotEdenInode->mknod(
                        ".find-ignore"_pc,
                        S_IFREG | 0644,
                        0,
                        InvalidationRequired::No);
                    return folly::unit;
                  }));
        }
#endif

        // Wait until we finish setting up all of the symlinks.
        // Use collectAll() since we want to wait for everything to complete,
        // even if one of them fails early.
        return collectAll(std::move(futures))
            .thenValue([self = shared_from_this(),
                        dotEdenInode](auto&& results) {
              for (auto& t : results) {
                if (t.hasException()) {
                  XLOGF(ERR, "Symlink setup failed: {}", t.exception().what());
                  return ImmediateFuture<Unit>(std::move(t).exception());
                }
              }
              // Set the dotEdenInodeNumber_ as our final step.
              // We do this after all of the ensureDotEdenSymlink() calls have
              // finished, since the TreeInode code will refuse to allow any
              // modifications to the .eden directory once we have set
              // dotEdenInodeNumber_.
              self->dotEdenInodeNumber_ = dotEdenInode->getNodeId();
              return ImmediateFuture<Unit>(folly::unit);
            });
      });
}

folly::SemiFuture<Unit> EdenMount::performBindMounts() {
  auto mountPath = getPath();
  auto edenDir = getEdenConfig()->edenDir.getValue();
  auto systemConfigDir = getEdenConfig()->getSystemConfigDir();
  SpawnedProcess::Options opts;
#ifdef _WIN32
  opts.creationFlags(CREATE_NO_WINDOW);
  opts.nullStderr();
  opts.nullStdin();
  opts.nullStdout();
#endif // _WIN32
  return folly::makeSemiFutureWith([&] {
           std::vector<std::string> argv{
               FLAGS_edenfsctlPath,
               "--config-dir",
               edenDir.c_str(),
               "--etc-eden-dir",
               systemConfigDir.c_str(),
               "redirect",
               "fixup",
               "--mount",
               mountPath.c_str()};
           return SpawnedProcess(argv, std::move(opts)).future_wait();
         })
      .deferValue([mountPath](ProcessStatus returnCode) {
        if (returnCode.exitStatus() == 0) {
          return folly::unit;
        }
        throw_<std::runtime_error>(
            "Failed to run `",
            FLAGS_edenfsctlPath,
            " redirect fixup --mount ",
            mountPath,
            "`: exited with status ",
            returnCode.str());
      })
      .deferError([mountPath](const folly::exception_wrapper& err) {
        throw_<std::runtime_error>(
            "Failed to run `",
            FLAGS_edenfsctlPath,
            " fixup --mount ",
            mountPath,
            "`: ",
            folly::exceptionStr(err));
      });
}

EdenMount::~EdenMount() = default;

bool EdenMount::tryToTransitionState(State expected, State newState) {
  return state_.compare_exchange_strong(
      expected, newState, std::memory_order_acq_rel);
}

void EdenMount::transitionState(State expected, State newState) {
  State found = expected;
  if (!state_.compare_exchange_strong(
          found, newState, std::memory_order_acq_rel)) {
    throwf<std::runtime_error>(
        "unable to transition mount {} to state {}: "
        "expected to be in state {} but actually in {}",
        getPath(),
        apache::thrift::util::enumNameSafe(newState),
        apache::thrift::util::enumNameSafe(expected),
        apache::thrift::util::enumNameSafe(found));
  }
}

void EdenMount::transitionToFsChannelInitializationErrorState() {
  auto oldState = State::STARTING;
  auto newState = State::FUSE_ERROR;
  if (!state_.compare_exchange_strong(
          oldState, newState, std::memory_order_acq_rel)) {
    switch (oldState) {
      case State::DESTROYING:
      case State::SHUTTING_DOWN:
      case State::SHUT_DOWN:
        break;

      case State::INIT_ERROR:
      case State::FUSE_ERROR:
      case State::INITIALIZED:
      case State::INITIALIZING:
      case State::RUNNING:
      case State::UNINITIALIZED:
        XLOGF(
            ERR,
            "FS channel initialization error occurred for an EdenMount in the unexpected {} state",
            oldState);
        break;

      case State::STARTING:
        XLOGF(
            FATAL,
            "compare_exchange_strong failed when transitioning EdenMount's state from {} to {}",
            oldState,
            newState);
        break;
    }
  }

  // For NFS mounts, we register the mount prior to finishing initialization.
  // Failure after registration (but before initialization) causes the
  // uninitialized mount to get stuck in the Mountd's map of registered mounts
  // and causes crashes when remount attempts occur. To avoid this, we must
  // always unregister upon initialization failure.
  auto nfsServer = serverState_->getNfsServer();
  if (nfsServer) {
    nfsServer->tryUnregisterMount(getPath());
  }
}

static folly::StringPiece getCheckoutModeString(CheckoutMode checkoutMode) {
  switch (checkoutMode) {
    case CheckoutMode::DRY_RUN:
      return "dry_run";
    case CheckoutMode::NORMAL:
      return "normal";
    case CheckoutMode::FORCE:
      return "force";
  }
  return "<unknown>";
}

#ifndef _WIN32
namespace {
TreeEntryType toEdenTreeEntryType(facebook::eden::ObjectType objectType) {
  switch (objectType) {
    case facebook::eden::ObjectType::TREE:
      return TreeEntryType::TREE;
    case facebook::eden::ObjectType::REGULAR_FILE:
      return TreeEntryType::REGULAR_FILE;
    case facebook::eden::ObjectType::EXECUTABLE_FILE:
      return TreeEntryType::EXECUTABLE_FILE;
    case facebook::eden::ObjectType::SYMLINK:
      return TreeEntryType::SYMLINK;
  }
  throw std::runtime_error("unsupported root type");
}

} // namespace

ImmediateFuture<SetPathObjectIdResultAndTimes> EdenMount::setPathsToObjectIds(
    std::vector<SetPathObjectIdObjectAndPath> objects,
    CheckoutMode checkoutMode,
    const ObjectFetchContextPtr& context) {
  std::vector<ImmediateFuture<SetPathObjectIdResultAndTimes>> futures;
  // Helper structs to heterogeneous lookup parentToObjectsMap by
  // RelativePathPiece whose index is RelativePath
  struct RelativePathHeterogeneousHasher {
    using is_transparent = void;

    size_t operator()(const RelativePathPiece& path) const {
      return facebook::eden::detail::hash_value(path);
    }
  };

  struct RelativePathHeterogeneousEqual {
    using is_transparent = void;
    bool operator()(const RelativePathPiece& lhs, const RelativePath& rhs)
        const {
      return lhs.view() == rhs.view();
    }
  };

  folly::F14FastMap<
      RelativePath,
      std::vector<SetPathObjectIdObjectAndPath>,
      RelativePathHeterogeneousHasher,
      RelativePathHeterogeneousEqual>
      parentToObjectsMap;
  for (auto& object : objects) {
    /*
     * In theory, an exclusive wlock should be issued, but
     * this is not efficient if many calls to this method ran in parallel.
     * So we use read lock instead assuming the contents of loaded rootId
     * objects are not weaving too much
     */
    XLOGF(
        DBG3,
        "adding {} to mount {} at path {}",
        objectStore_->renderObjectId(object.id),
        this->getPath(),
        object.path);
    auto& path = object.path;
    if (path.empty()) {
      // If the path is root, only setting to a tree is allowed
      if (facebook::eden::ObjectType::TREE == object.type) {
        // If the path is root, and setting to tree type, no more than one tree
        // is allowed.
        if (!parentToObjectsMap[path.dirname()].empty()) {
          throw std::domain_error(
              "SetPathObjectId does not support set multiple trees on root");
        }
      } else {
        throw std::domain_error(
            "SetPathObjectId only support set tree object type on root");
      }
    }
    parentToObjectsMap[path.dirname()].push_back(std::move(object));
  }
  objects.clear();

  for (auto& [path, objs] : parentToObjectsMap) {
    const folly::stop_watch<> stopWatch;
    auto setPathObjectIdTime = std::make_shared<SetPathObjectIdTimes>();

    auto ctx = std::make_shared<CheckoutContext>(
        this,
        checkoutMode,
        context->getClientPid(),
        "setPathObjectId",
        nullptr,
        context->getRequestInfo(),
        context->getCancellationToken());
    if (!ctx->isDryRun()) {
      ctx->inhibitInodeGC();
    }

    /**
     * This will update the timestamp for the entire mount,
     * TODO(yipu) We should only update the timestamp for the
     * partial node so only affects its children.
     */
    setLastCheckoutTime(EdenTimestamp{clock_->getRealtime()});

    // A special case is set root to a tree. Then setPathObjectId is essentially
    // checkout
    bool setOnRoot = path.empty() && objs.size() == 1 &&
        objs.at(0).path.empty() &&
        facebook::eden::ObjectType::TREE == objs.at(0).type;

    auto getTargetTreeInodeFuture =
        ensureDirectoryExists(path, ctx->getFetchContext());

    std::vector<ImmediateFuture<shared_ptr<TreeEntry>>> getTreeEntryFutures;
    if (!setOnRoot) {
      getTreeEntryFutures.reserve(objs.size());
      for (auto& object : objs) {
        ImmediateFuture<shared_ptr<TreeEntry>> getTreeEntryFuture =
            objectStore_->getTreeEntryForObjectId(
                object.id,
                toEdenTreeEntryType(object.type),
                ctx->getFetchContext());
        getTreeEntryFutures.emplace_back(std::move(getTreeEntryFuture));
      }
    }

    auto getRootTreeFuture = setOnRoot
        ? objectStore_->getTree(objs.at(0).id, ctx->getFetchContext())
        : collectAllSafe(std::move(getTreeEntryFutures))
              .thenValue(
                  [objs_2 = std::move(objs),
                   caseSensitive = getCheckoutConfig()->getCaseSensitive()](
                      std::vector<shared_ptr<TreeEntry>> entries) {
                    // Make up a fake ObjectId for this tree.
                    // WARNING: This is dangerous -- this ObjectId cannot be
                    // used to look up this synthesized tree from the
                    // BackingStore.
                    ObjectId fakeObjectId{};
                    Tree::container treeEntries{caseSensitive};
                    for (size_t i = 0; i < entries.size(); ++i) {
                      treeEntries.emplace(
                          PathComponent{objs_2.at(i).path.basename()},
                          std::move(*entries.at(i)));
                    }

                    return std::make_shared<const Tree>(
                        std::move(treeEntries), fakeObjectId);
                  });

    auto future =
        collectAllSafe(getTargetTreeInodeFuture, getRootTreeFuture)
            .thenValue(
                [ctx, setPathObjectIdTime, stopWatch](
                    const std::tuple<TreeInodePtr, shared_ptr<const Tree>>&
                        results) {
                  setPathObjectIdTime->didLookupTreesOrGetInodeByPath =
                      stopWatch.elapsed();
                  auto [targetTreeInode, incomingTree] = results;
                  targetTreeInode->unloadChildrenUnreferencedByFs();
                  return targetTreeInode
                      ->checkout(ctx.get(), nullptr, incomingTree)
                      .semi();
                })
            .thenValue([ctx, setPathObjectIdTime, stopWatch](auto&&) {
              setPathObjectIdTime->didCheckout = stopWatch.elapsed();
              return ctx->flush().semi();
            })
            .thenValue([ctx, setPathObjectIdTime, stopWatch](
                           std::vector<CheckoutConflict>&& conflicts) {
              setPathObjectIdTime->didFinish = stopWatch.elapsed();
              SetPathObjectIdResultAndTimes resultAndTimes;
              resultAndTimes.times = *setPathObjectIdTime;
              SetPathObjectIdResult result;
              result.conflicts() = std::move(conflicts);
              resultAndTimes.result = std::move(result);
              return resultAndTimes;
            })
            .thenTry([this, ctx](
                         Try<SetPathObjectIdResultAndTimes>&& resultAndTimes) {
              auto fetchStats = ctx->getStatsContext().computeStatistics();
              XLOGF(
                  DBG4,
                  "{}setPathObjectId for {} accessed {} trees ({}% chr), {} blobs ({}% chr), and {} metadata ({}% chr).",
                  (resultAndTimes.hasValue() ? "" : "failed "),
                  this->getPath(),
                  fetchStats.tree.accessCount,
                  fetchStats.tree.cacheHitRate,
                  fetchStats.blob.accessCount,
                  fetchStats.blob.cacheHitRate,
                  fetchStats.blobAuxData.accessCount,
                  fetchStats.blobAuxData.cacheHitRate);

              return std::move(resultAndTimes);
            });
    futures.emplace_back(std::move(future));
  }

  // Merge conflicts and stats
  return collectAllSafe(std::move(futures))
      .thenValue(
          [](std::vector<SetPathObjectIdResultAndTimes> resultAndTimesList) {
            SetPathObjectIdTimes times;
            std::vector<CheckoutConflict> conflicts;
            for (auto& resultAndTime : resultAndTimesList) {
              for (auto conflict : *resultAndTime.result.conflicts()) {
                conflicts.emplace_back(std::move(conflict));
              }
              times.didLookupTreesOrGetInodeByPath +=
                  resultAndTime.times.didLookupTreesOrGetInodeByPath;
              times.didCheckout += resultAndTime.times.didCheckout;
              times.didFinish += resultAndTime.times.didFinish;
            }
            SetPathObjectIdResult result;
            result.conflicts() = std::move(conflicts);
            SetPathObjectIdResultAndTimes resultAndTimes;
            resultAndTimes.times = std::move(times);
            resultAndTimes.result = std::move(result);
            return resultAndTimes;
          });
}
#endif // !_WIN32

void EdenMount::destroy() {
  auto oldState = state_.exchange(State::DESTROYING, std::memory_order_acq_rel);
  // Clear channel_ so concurrent readers see null and bail out.
  // Existing ReadMostlySharedPtr holders keep the channel alive.
  channel_.store(std::shared_ptr<FsChannel>());
  XLOGF(DBG4, "attempting to destroy EdenMount {}", getPath());
  switch (oldState) {
    case State::UNINITIALIZED:
    case State::INITIALIZING: {
      // The root inode may still be null here if we failed to load the root
      // inode.  In this case just delete ourselves immediately since we don't
      // have any inodes to unload.  shutdownImpl() requires the root inode be
      // loaded.
      if (!getRootInode()) {
        delete this;
      } else {
        // Call shutdownImpl() to destroy all loaded inodes.
        shutdownImpl(/*doTakeover=*/false);
      }
      return;
    }
    case State::INITIALIZED:
    case State::RUNNING:
    case State::STARTING:
    case State::INIT_ERROR:
    case State::FUSE_ERROR: {
      // Call shutdownImpl() to destroy all loaded inodes.
      shutdownImpl(/*doTakeover=*/false);
      return;
    }
    case State::SHUTTING_DOWN:
      // Nothing else to do.  shutdown() will destroy us when it completes.
      return;
    case State::SHUT_DOWN:
      // We were already shut down, and can delete ourselves immediately.
      XLOGF(DBG1, "destroying shut-down EdenMount {}", getPath());
      delete this;
      return;
    case State::DESTROYING:
      // Fall through to the error handling code below.
      break;
  }

  XLOGF(
      FATAL,
      "EdenMount::destroy() called on mount {} in unexpected state {}",
      getPath(),
      oldState);
}

folly::SemiFuture<SerializedInodeMap> EdenMount::shutdown(
    bool doTakeover,
    bool allowFuseNotStarted) {
  XLOGF(DBG4, "attempting to shutdown EdenMount {}", getPath());
  // shutdown() should only be called on mounts that have not yet reached
  // SHUTTING_DOWN or later states.  Confirm this is the case, and move to
  // SHUTTING_DOWN.
  if (!(allowFuseNotStarted &&
        (tryToTransitionState(State::UNINITIALIZED, State::SHUTTING_DOWN) ||
         tryToTransitionState(State::INITIALIZING, State::SHUTTING_DOWN) ||
         tryToTransitionState(State::INITIALIZED, State::SHUTTING_DOWN))) &&
      !tryToTransitionState(State::RUNNING, State::SHUTTING_DOWN) &&
      !tryToTransitionState(State::STARTING, State::SHUTTING_DOWN) &&
      !tryToTransitionState(State::INIT_ERROR, State::SHUTTING_DOWN) &&
      !tryToTransitionState(State::FUSE_ERROR, State::SHUTTING_DOWN)) {
    EDEN_BUG() << "attempted to call shutdown() on a non-running EdenMount: "
               << "state was " << fmt::underlying(getState());
  }

  // The caller calls us with the EdenServer::mountPoints_ lock, make sure that
  // shutdownImpl isn't executed inline so the lock can be released.
  return folly::makeSemiFuture().defer(
      [doTakeover, this](auto&&) { return shutdownImpl(doTakeover); });
}

folly::SemiFuture<SerializedInodeMap> EdenMount::shutdownImpl(bool doTakeover) {
  journal_->cancelAllSubscribers();
  XLOGF(DBG1, "beginning shutdown for EdenMount {}", getPath());

  return inodeMap_->shutdown(doTakeover)
      .thenValue([this](SerializedInodeMap inodeMap) {
        XLOGF(DBG1, "shutdown complete for EdenMount {}", getPath());
        // Close the Overlay object to make sure we have released its lock.
        // This is important during graceful restart to ensure that we have
        // released the lock before the new edenfs process begins to take over
        // the mount point.
        overlay_->close();
        XLOGF(DBG1, "successfully closed overlay at {}", getPath());
        auto oldState =
            state_.exchange(State::SHUT_DOWN, std::memory_order_acq_rel);
        if (oldState == State::DESTROYING) {
          delete this;
        }
        return inodeMap;
      });
}

folly::SemiFuture<folly::Unit> EdenMount::unmount(UnmountOptions options) {
  auto mountingUnmountingState = mountingUnmountingState_.wlock();
  if (mountingUnmountingState->fsChannelUnmountStarted()) {
    return mountingUnmountingState->fsChannelUnmountPromise->getFuture();
  }
  mountingUnmountingState->fsChannelUnmountPromise.emplace();
  if (!mountingUnmountingState->fsChannelMountStarted()) {
    return folly::makeFuture();
  }
  auto mountFuture =
      mountingUnmountingState->fsChannelMountPromise->getFuture();
  mountingUnmountingState.unlock();

  auto channelHolder =
      std::make_shared<folly::ReadMostlySharedPtr<FsChannel>>();
  return std::move(mountFuture)
      .thenTry([this, options, channelHolder](Try<Unit>&& mountResult) {
        if (mountResult.hasException()) {
          return folly::makeSemiFuture();
        }
        auto ch = channel_.load();
        if (!ch) {
          throw std::runtime_error(
              "attempting to unmount() an EdenMount without an FsChannel");
        }
        // If a Future then callback returns a SemiFuture, that SemiFuture is
        // attached to the implied InlineExecutor.
        // Therefore, the following callback will be guaranteed to fixup
        // the mountingUnmountingState, even if the returned SemiFuture is
        // dropped.
        // TODO: Is it safe to call FsChannel::unmount if the FuseChannel
        // is in the process of starting? Or can we assume that
        // mountResult.hasException() above covers that case?

        // Keep channel alive until the unmount future completes.
        *channelHolder = std::move(ch);
        return (*channelHolder)->unmount(options);
      })
      .thenTry(
          [this,
           channelHolder](Try<Unit>&& result) noexcept -> folly::Future<Unit> {
            // Release the channel reference now that unmount has completed.
            channelHolder->reset();

            auto unmountState = mountingUnmountingState_.wlock();
            XDCHECK(unmountState->fsChannelUnmountPromise.has_value());
            folly::SharedPromise<folly::Unit>* unsafeUnmountPromise =
                &*unmountState->fsChannelUnmountPromise;
            unmountState.unlock();

            unsafeUnmountPromise->setTry(Try<Unit>{result});
            return folly::makeFuture<folly::Unit>(std::move(result));
          });
}

const shared_ptr<UnboundedQueueExecutor>& EdenMount::getServerThreadPool()
    const {
  return serverState_->getThreadPool();
}

#ifdef _WIN32
const shared_ptr<UnboundedQueueExecutor>& EdenMount::getInvalidationThreadPool()
    const {
  return invalidationExecutor_;
}
#endif

folly::ReadMostlySharedPtr<const EdenConfig> EdenMount::getEdenConfig() const {
  return serverState_->getReloadableConfig()->getEdenConfig();
}

void EdenMount::updateInodePressurePolicy() {
  auto config = getEdenConfig();
  auto minInodes = config->gcPressureMinInodes.getValue();
  auto maxInodes = config->gcPressureMaxInodes.getValue();
  auto fuseTtlMax = config->fuseTtlMaxSeconds.getValue();
  auto fuseTtlMin = config->fuseTtlMinSeconds.getValue();
  auto gcCutoffMax = config->gcCutoffMaxSeconds.getValue();
  auto gcCutoffMin = config->gcCutoffMinSeconds.getValue();
  auto gcPeriodMax = std::chrono::duration_cast<std::chrono::seconds>(
      config->pressureBasedGcPeriodMax.getValue());
  auto gcPeriodMin = std::chrono::duration_cast<std::chrono::seconds>(
      config->pressureBasedGcPeriodMin.getValue());

  cachedPressurePolicy_ = std::make_shared<const InodePressurePolicy>(
      minInodes,
      maxInodes,
      std::chrono::seconds{fuseTtlMax},
      std::chrono::seconds{fuseTtlMin},
      std::chrono::seconds{gcCutoffMax},
      std::chrono::seconds{gcCutoffMin},
      gcPeriodMax,
      gcPeriodMin);

  XLOGF(
      DBG4,
      "Updated InodePressurePolicy for {}: "
      "inodes=[{}, {}], fuseTtl=[{}s, {}s], gcCutoff=[{}s, {}s], "
      "gcPeriod=[{}s, {}s]",
      getPath(),
      minInodes,
      maxInodes,
      fuseTtlMin,
      fuseTtlMax,
      gcCutoffMin,
      gcCutoffMax,
      gcPeriodMin.count(),
      gcPeriodMax.count());
}

// Below this many invalidations, rerunning GC is cheap enough that reclaim
// tracking isn't worthwhile.
constexpr uint64_t kPressureGcReclaimMinInvalidated = 10'000;

void EdenMount::recordPressureGcOutcome(
    uint64_t numInvalidated,
    uint64_t numUnloaded,
    bool pinScanFailed) {
  // GC flushes the invalidation queue between invalidating entries and
  // sweeping, and the kernel FORGETs triggered by the invalidations arrive
  // quickly in practice, so most of a run's invalidations should be unloaded
  // by the run's own sweep. A healthy run reclaims far more than the default
  // 10%; should a run be misjudged anyway, the cost is one cycle at the
  // regular GC cadence. The sweep's own count is used rather than the change
  // in the mount's inode count, which concurrent lookups (a build, a crawl)
  // can push the other way while GC runs.
  auto minReclaimPercent =
      getEdenConfig()->pressureBasedGcMinReclaimPercent.getValue();
  const bool stalled = minReclaimPercent > 0 &&
      numInvalidated >= kPressureGcReclaimMinInvalidated &&
      numUnloaded * 100 <= numInvalidated * minReclaimPercent;
  // Without a pin set the run reclaimed files only, and every run until a
  // scan succeeds would do the same; each costs a full scan of every
  // process, so it is not worth the pressure-derived rate either.
  const bool backOff = stalled || pinScanFailed;

  if (pressureGcBackoff_.exchange(backOff, std::memory_order_relaxed) !=
      backOff) {
    if (pinScanFailed) {
      XLOGF(
          INFO,
          "Pressure-based GC for {} ran without a pin set because the pin "
          "scan failed. Falling back to the regular GC period.",
          getPath());
    } else if (backOff) {
      XLOGF(
          INFO,
          "Pressure-based GC for {} invalidated {} inodes but only reclaimed "
          "{}; the kernel may no longer hold the invalidated entries. "
          "Falling back to the regular GC period.",
          getPath(),
          numInvalidated,
          numUnloaded);
    } else {
      XLOGF(
          INFO,
          "Pressure-based GC for {} completed, resuming the pressure-based GC "
          "period",
          getPath());
    }
  }
}

std::optional<int64_t> EdenMount::getCheckoutProgress() const {
  auto parentLock = parentState_.rlock();
  if (!std::holds_alternative<ParentCommitState::CheckoutInProgress>(
          parentLock->checkoutState)) {
    return std::nullopt;
  }
  auto checkout = std::get<ParentCommitState::CheckoutInProgress>(
      parentLock->checkoutState);
  auto progress = checkout.checkoutProgress.get();
  if (progress == nullptr) {
    return std::nullopt;
  }
  return progress->load(std::memory_order_relaxed);
}

#ifndef _WIN32
InodeMetadataTable* EdenMount::getInodeMetadataTable() const {
  return overlay_->getInodeMetadataTable();
}
#endif

FsChannel* EdenMount::getFsChannel() const {
  return channel_.load().get();
}

Nfsd3* FOLLY_NULLABLE EdenMount::getNfsdChannel() const {
  return dynamic_cast<Nfsd3*>(channel_.load().get());
}

FuseChannel* FOLLY_NULLABLE EdenMount::getFuseChannel() const {
#ifndef _WIN32
  return dynamic_cast<FuseChannel*>(channel_.load().get());
#else
  return nullptr;
#endif
}

#ifndef _WIN32
std::shared_ptr<FuseChannel> EdenMount::getFuseChannelShared() const {
  auto channel = channel_.load().getStdShared();
  auto* fuseChannel = dynamic_cast<FuseChannel*>(channel.get());
  if (fuseChannel == nullptr) {
    return {};
  }
  return {std::move(channel), fuseChannel};
}

std::shared_ptr<UnboundedQueueExecutor>
EdenMount::getInodeGCInvalidationExecutor() const {
  folly::call_once(inodeGCInvalidationExecutorOnce_, [this] {
    inodeGCInvalidationExecutor_ =
        std::make_shared<UnboundedQueueExecutor>(1, "inode-gc-inval");
  });
  return inodeGCInvalidationExecutor_;
}
#endif

PrjfsChannel* FOLLY_NULLABLE EdenMount::getPrjfsChannel() const {
#ifdef _WIN32
  return dynamic_cast<PrjfsChannel*>(channel_.load().get());
#else
  return nullptr;
#endif
}

void EdenMount::setChannel(FsChannelPtr channel) {
  if (channel) {
    channel_.store(
        std::shared_ptr<FsChannel>(channel.release(), FsChannelDeleter{}));
  } else {
    channel_.store(std::shared_ptr<FsChannel>());
  }
}

void EdenMount::setTestFsChannel(FsChannelPtr channel) {
  setChannel(std::move(channel));
}

bool EdenMount::isNfsdChannel() const {
  return getNfsdChannel() != nullptr;
}

void EdenMount::calculateIsNfsMount(
    const std::optional<MountProtocol>& takeover) {
  if (takeover) {
    shouldUseNFSMount_ = takeover.value() == MountProtocol::NFS;
  } else {
    shouldUseNFSMount_ = getEdenConfig()->enableNfsServer.getValue() &&
        getCheckoutConfig()->getMountProtocol() == MountProtocol::NFS;
  }
  // So this whole method should run before the mount is initialized.
  XCHECK_LT(
      folly::to_underlying(state_.load(std::memory_order_acquire)),
      folly::to_underlying(State::INITIALIZING))
      << "The invariant that shouldUseNFSMount_ should not be modified after "
         "the mount has started initializing has been violated. This could "
         "make calls to shouldBeOrIsNfsChannel unsafe.";
}

bool EdenMount::shouldBeOrIsNfsChannel() const {
  XCHECK_GE(
      folly::to_underlying(state_.load(std::memory_order_acquire)),
      folly::to_underlying(State::INITIALIZING))
      << "Though we guarantee that we won't modify shouldUseNFSMount_ "
         "after initialization begins. shouldUseNFSMount_ might be set any time "
         "before initialization starts and we provide no explicit synchronization "
         "on it, so it is not safe to access right now.";
  XCHECK(shouldUseNFSMount_.has_value())
      << "shouldUseNFSMount_ should have been set by this point. It is intended"
         " that this is set before the mount begins initializing, and we only "
         "access it after the mount has started initializing. ";
  return shouldUseNFSMount_.value();
}

bool EdenMount::throwEstaleIfInodeIsMissing() const {
  return shouldBeOrIsNfsChannel();
}

EdenMount::ReadLocation EdenMount::getReadLocationForMaterializedFiles() const {
#ifdef _WIN32
  if (!shouldBeOrIsNfsChannel()) {
    // if we are on Windows and  the mount is not an NFS channel then it must be
    // a prjfs one.
    return ReadLocation::InRepo;
  }
#endif
  return ReadLocation::Overlay;
}

ProcessAccessLog& EdenMount::getProcessAccessLog() const {
  auto ch = channel_.load();
  if (!ch) {
    EDEN_BUG() << "cannot call getProcessAccessLog() before "
                  "EdenMount has started or unmounted";
  }
  return ch->getProcessAccessLog();
}

const AbsolutePath& EdenMount::getPath() const {
  return checkoutConfig_->getMountPath();
}

const EdenStatsPtr& EdenMount::getStats() const {
  return serverState_->getStats();
}

TreeInodePtr EdenMount::getRootInode() const {
  auto rootInode = inodeMap_->getRootInode();
  if (isSafeForInodeAccess()) {
    XDCHECK(rootInode);
    // The root inode is initialized when the InodeMap is initialized, and
    // destroyed when the InodeMap is shutdown. Both of these occur outside of
    // the constructor/destructor when the EdenMount is still alive and may be
    // held in multiple threads.
    //
    // To prevent the InodeMap from being shutdown, a reference must be held on
    // the rootInode, and this reference must be obtained with the
    // EdenServer::mountPoints_ lock. Subsequent getRootInode are safe outside
    // of that lock.
    //
    // At this point, the root inode should thus have a refcount of at least 3:
    //  - One held by InodeMap
    //  - One returned from EdenServer::getMountAndRootInode
    //  - And the one on the stack just above.
    XCHECK_GE(rootInode->debugGetPtrRef(), 3u);
  }
  return rootInode;
}

TreeInodePtr EdenMount::getRootInodeUnchecked() const {
  return inodeMap_->getRootInode();
}

std::shared_ptr<const Tree> EdenMount::getCheckedOutRootTree() const {
  return parentState_.rlock()->checkedOutRootTree;
}

ImmediateFuture<std::variant<std::shared_ptr<const Tree>, TreeEntry>>
EdenMount::getTreeOrTreeEntry(
    RelativePathPiece path,
    const ObjectFetchContextPtr& context) const {
  return ::facebook::eden::getTreeOrTreeEntry(
      getCheckedOutRootTree(), path, objectStore_, context.copy());
}

namespace {
class CanonicalizeProcessor {
 public:
  CanonicalizeProcessor(
      RelativePath path,
      std::shared_ptr<ObjectStore> objectStore,
      ObjectFetchContextPtr context)
      : path_{std::move(path)},
        iterRange_{path_.components()},
        iter_{iterRange_.begin()},
        objectStore_{std::move(objectStore)},
        context_{std::move(context)} {}

  ImmediateFuture<RelativePath> next(std::shared_ptr<const Tree> tree) {
    auto name = *iter_++;
    auto it = tree->find(name);

    if (it == tree->cend()) {
      return makeImmediateFuture<RelativePath>(
          std::system_error(ENOENT, std::generic_category()));
    }

    retPath_ = retPath_ + it->first;

    if (iter_ == iterRange_.end()) {
      return retPath_;
    } else {
      if (!it->second.isTree()) {
        return makeImmediateFuture<RelativePath>(
            std::system_error(ENOTDIR, std::generic_category()));
      } else {
        return objectStore_->getTree(it->second.getObjectId(), context_)
            .thenValue([this](std::shared_ptr<const Tree> tree) {
              return next(std::move(tree));
            });
      }
    }
  }

 private:
  RelativePath path_;
  RelativePath::base_type::component_iterator_range iterRange_;
  RelativePath::base_type::component_iterator iter_;
  std::shared_ptr<ObjectStore> objectStore_;
  ObjectFetchContextPtr context_;

  RelativePath retPath_{""};
};
} // namespace

ImmediateFuture<RelativePath> EdenMount::canonicalizePathFromTree(
    RelativePathPiece path,
    const ObjectFetchContextPtr& context) const {
  if (path.empty()) {
    return path.copy();
  }

  auto tree = getCheckedOutRootTree();
  auto processor = std::make_unique<CanonicalizeProcessor>(
      path.copy(), objectStore_, context.copy());
  auto future = processor->next(std::move(tree));
  return std::move(future).ensure(
      [p = std::move(processor)]() mutable { p.reset(); });
}

InodeNumber EdenMount::getDotEdenInodeNumber() const {
  return dotEdenInodeNumber_;
}

ImmediateFuture<InodePtr> EdenMount::getInodeSlow(
    RelativePathPiece path,
    const ObjectFetchContextPtr& context) const {
  // DEPRECATED: use co_getInodeSlow directly. Kept only because
  // PrjfsDispatcherImpl and EdenServiceHandler (removeRecursively,
  // inodeFromUserPath) still consume ImmediateFuture chains; delete
  // once those are migrated.
  return ImmediateFuture{
      // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
      folly::coro::co_invoke(
          [this](auto&&... args) -> folly::coro::Task<InodePtr> {
            co_return co_await co_getInodeSlow(
                std::forward<decltype(args)>(args)...);
          },
          path.copy(),
          context.copy())
          .semi()};
}

folly::coro::now_task<InodePtr> EdenMount::co_getInodeSlow(
    RelativePathPiece path,
    const ObjectFetchContextPtr& context) const {
  co_return co_await inodeMap_->getRootInode()->co_getChildRecursive(
      path, context);
}
namespace {

class VirtualInodeLookupProcessor {
 public:
  explicit VirtualInodeLookupProcessor(
      RelativePathPiece path,
      std::shared_ptr<ObjectStore> objectStore,
      ObjectFetchContextPtr context)
      : path_{path},
        iterRange_{path_.components()},
        iter_{iterRange_.begin()},
        objectStore_(std::move(objectStore)),
        context_{std::move(context)} {}

  folly::coro::now_task<VirtualInode> co_next(VirtualInode inodeTreeEntry) {
    if (iter_ == iterRange_.end()) {
      // Lookup terminated, return the existing entry
      co_return std::move(inodeTreeEntry);
    }
    // There are path components left, recurse looking for the next child
    auto childName = *iter_++;
    auto entry = co_await inodeTreeEntry.co_getOrFindChild(
        childName, path_, objectStore_, context_);
    co_return co_await co_next(std::move(entry));
  }

 private:
  RelativePath path_;
  RelativePath::base_type::component_iterator_range iterRange_;
  RelativePath::base_type::component_iterator iter_;
  std::shared_ptr<ObjectStore> objectStore_;
  ObjectFetchContextPtr context_;
};

} // namespace

folly::coro::now_task<VirtualInode> EdenMount::co_getVirtualInode(
    RelativePathPiece path,
    const ObjectFetchContextPtr& context) const {
  auto rootInode = static_cast<InodePtr>(getRootInode());
  auto processor = std::make_unique<VirtualInodeLookupProcessor>(
      path, getObjectStore(), context.copy());
  co_return co_await processor->co_next(VirtualInode(std::move(rootInode)));
}

ImmediateFuture<VirtualInode> EdenMount::getVirtualInode(
    RelativePathPiece path,
    const ObjectFetchContextPtr& context) const {
  // DEPRECATED: use co_getVirtualInode directly. Kept only because
  // EdenServiceHandler (changesSince, getEntryAttributesForPath,
  // getFileContentImpl, readdir) still consume ImmediateFuture chains;
  // delete once those are migrated.
  return ImmediateFuture{
      // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
      folly::coro::co_invoke(
          [this](auto&&... args) -> folly::coro::Task<VirtualInode> {
            co_return co_await co_getVirtualInode(
                std::forward<decltype(args)>(args)...);
          },
          path.copy(),
          context.copy())
          .semi()};
}

ImmediateFuture<folly::Unit> EdenMount::waitForPendingWrites() const {
  // DEPRECATED: use co_waitForPendingWrites directly. Kept only because
  // checkout and diff paths still call this via ImmediateFuture chains;
  // delete once those paths are migrated to coroutines.
  return ImmediateFuture{
      // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
      folly::coro::co_invoke([this]() -> folly::coro::Task<folly::Unit> {
        co_return co_await co_waitForPendingWrites();
      }).semi()};
}

folly::coro::now_task<folly::Unit> EdenMount::co_waitForPendingWrites() const {
  co_await serverState_->getFaultInjector().co_checkAsync(
      "waitForPendingWrites", "");
  auto ch = channel_.load();
  if (ch) {
    co_await ch->co_waitForPendingWrites();
  }
  co_return folly::unit;
}

constexpr const char* interruptedCheckoutAdvice =
    "a previous checkout was interrupted - please run `sl go {0}` to resume it"
    ".\nIf there are conflicts, run `sl go --clean {0}` to discard changes, or `sl go --merge {0}` to merge.";

struct EdenMount::CheckoutInProgressGuard {
  CheckoutInProgressGuard(
      EdenMount& mount,
      std::shared_ptr<CheckoutContext> ctx,
      RootId oldParent,
      RootId snapshotId,
      ParentCommitState::CheckoutState oldState)
      : mount_{&mount},
        ctx_{std::move(ctx)},
        oldParent_{std::move(oldParent)},
        snapshotId_{std::move(snapshotId)},
        oldState_{std::move(oldState)} {}

  CheckoutInProgressGuard(CheckoutInProgressGuard&& other) noexcept
      : mount_{other.mount_},
        ctx_{std::move(other.ctx_)},
        oldParent_{std::move(other.oldParent_)},
        snapshotId_{std::move(other.snapshotId_)},
        oldState_{std::move(other.oldState_)} {
    other.mount_ = nullptr;
  }

  CheckoutInProgressGuard& operator=(CheckoutInProgressGuard&&) = delete;
  CheckoutInProgressGuard(const CheckoutInProgressGuard&) = delete;
  CheckoutInProgressGuard& operator=(const CheckoutInProgressGuard&) = delete;

  ~CheckoutInProgressGuard() {
    if (mount_ != nullptr) {
      // A dropped continuation is treated like a failed checkout.
      applyResetState(mount_, /*hadException=*/true);
    }
  }

  folly::Try<std::vector<CheckoutConflict>> finish(
      folly::Try<std::vector<CheckoutConflict>>&& res) {
    XCHECK(mount_ != nullptr);
    auto mount = mount_;
    mount_ = nullptr;
    if (res.hasException()) {
      XLOGF(ERR, "checkout error: {}", folly::exceptionStr(res.exception()));
    } else {
      for (const auto& conflict : res.value()) {
        if (conflict.type() == ConflictType::ERROR) {
          XLOGF(
              ERR,
              "checkout error for {}: {}",
              conflict.path().value(),
              conflict.message().value());
        }
      }
    }
    if (applyResetState(mount, res.hasException())) {
      res.exception().with_exception([&](const std::exception& ex) {
        mount->getServerState()->getErrorLogger().log(
            EdenErrorInfo::objectStore(ErrorArg::fromExceptionWithoutTrace(ex))
                .withMountPoint(mount->getPath().asString())
                .withErrorType("checkout_update_error"));
      });
      return folly::Try<std::vector<CheckoutConflict>>{
          newEdenError(res.exception())};
    }
    return std::move(res);
  }

 private:
  // Resets parentState_ out of CheckoutInProgress: restores the prior state for
  // dry runs, leaves the mount interrupted when the checkout failed, or clears
  // the state otherwise. Returns true when the mount was left interrupted, so
  // the caller can surface the error to the user.
  bool applyResetState(EdenMount* mount, bool hadException) {
    auto parentLock = mount->parentState_.wlock();
    XCHECK(
        std::holds_alternative<ParentCommitState::CheckoutInProgress>(
            parentLock->checkoutState));
    if (ctx_->isDryRun()) {
      // Restore the prior state (possibly InterruptedCheckout) for dry runs.
      parentLock->checkoutState = oldState_;
      return false;
    } else if (hadException) {
      parentLock->checkoutState = ParentCommitState::InterruptedCheckout{
          oldParent_,
          snapshotId_,
      };
      return true;
    } else {
      parentLock->checkoutState = ParentCommitState::NoOngoingCheckout{};
      return false;
    }
  }

  EdenMount* mount_;
  std::shared_ptr<CheckoutContext> ctx_;
  RootId oldParent_;
  RootId snapshotId_;
  ParentCommitState::CheckoutState oldState_;
};

/**
 * State produced by `beginCheckout` and consumed by `checkout`.
 *
 * `firstFaultCheck` holds the result of `serverState_->getFaultInjector()
 * .checkAsync("checkout", ...)` registered EAGERLY when
 * `beginCheckout` runs. The fault handler must be registered
 * synchronously during the call so a subsequent `FaultInjector::unblock()`
 * finds it; registering it only on first await would break tests that block +
 * unblock the "checkout" fault around an outstanding checkout
 * (`checkoutFailsOnInProgressCheckout`, `diffFailsOnInProgressCheckout`).
 */
struct EdenMount::CheckoutSetup {
  std::shared_ptr<CheckoutContext> ctx;
  RootId oldParent;
  ParentCommitState::CheckoutState oldState;
  // Default-initialized to a ready future. `beginCheckout` overwrites this with
  // the real eagerly-registered fault check.
  ImmediateFuture<folly::Unit> firstFaultCheck{std::in_place};
};

folly::Try<EdenMount::CheckoutSetup> EdenMount::beginCheckout(
    const RootId& snapshotId,
    const ObjectFetchContextPtr& fetchContext,
    folly::StringPiece thriftMethodCaller,
    CheckoutMode checkoutMode) {
  CheckoutSetup setup;
  setup.oldState = ParentCommitState::NoOngoingCheckout{};
  auto progressTracker = std::make_shared<std::atomic<uint64_t>>(0);

  // Scope the parentState_ wlock tightly: validate state, mutate to
  // CheckoutInProgress, then release. Holding the wlock across the
  // checkAsync call below would, under fault injection, block every
  // parentState_ reader.
  {
    auto parentLock = parentState_.wlock();
    if (parentLock->isCheckoutInProgressOrInterrupted()) {
      if (std::holds_alternative<ParentCommitState::CheckoutInProgress>(
              parentLock->checkoutState)) {
        return folly::Try<CheckoutSetup>{newEdenError(
            EdenErrorType::CHECKOUT_IN_PROGRESS,
            "another checkout operation is still in progress")};
      } else {
        auto& interruptedCheckout =
            std::get<ParentCommitState::InterruptedCheckout>(
                parentLock->checkoutState);
        if (objectStore_->compareRootsById(
                interruptedCheckout.toCommit, snapshotId) !=
            ObjectComparison::Identical) {
          return folly::Try<CheckoutSetup>{newEdenError(
              EdenErrorType::CHECKOUT_IN_PROGRESS,
              fmt::format(
                  interruptedCheckoutAdvice,
                  folly::hexlify(objectStore_->renderRootId(
                      interruptedCheckout.toCommit))))};
        } else {
          setup.oldParent = interruptedCheckout.fromCommit;
          setup.oldState = interruptedCheckout;
        }
      }
    } else {
      setup.oldParent = parentLock->workingCopyParentRootId;
    }
    // Set checkoutInProgress and release the lock. An alternative way of
    // achieving the same would be to hold the lock during the checkout
    // operation, but this might lead to deadlocks on Windows due to callbacks
    // needing to access the parent commit to service callbacks.
    parentLock->checkoutState =
        ParentCommitState::CheckoutInProgress{progressTracker};
    setup.ctx = std::make_shared<CheckoutContext>(
        this,
        checkoutMode,
        fetchContext->getClientPid(),
        thriftMethodCaller,
        progressTracker,
        fetchContext->getRequestInfo(),
        fetchContext->getCancellationToken());
    setup.ctx->getFetchContext()->setDetachedExecutor(
        fetchContext->getDetachedExecutor());
  } // parentState_ wlock released here.

  if (!setup.ctx->isDryRun()) {
    setup.ctx->inhibitInodeGC();
  }

  // Eagerly register the first fault check so unblock() can find it.
  // See struct comment for details. Done OUTSIDE the wlock scope so
  // fault injection (which can block this call) doesn't block
  // parentState_ readers.
  setup.firstFaultCheck =
      serverState_->getFaultInjector().checkAsync("checkout", getPath().view());
  return folly::Try<CheckoutSetup>{std::move(setup)};
}

folly::Try<CheckoutResult> EdenMount::finalizeCheckout(
    const std::shared_ptr<CheckoutContext>& ctx,
    CheckoutMode checkoutMode,
    const CheckoutTimes& checkoutTimes,
    const folly::stop_watch<>& stopWatch,
    const RootId& oldParent,
    const RootId& snapshotId,
    JournalDiffCallback* journalDiffCallback,
    folly::Try<std::vector<CheckoutConflict>>&& conflictsResult) {
  auto result = [&]() -> folly::Try<CheckoutResult> {
    try {
      if (conflictsResult.hasException()) {
        return folly::Try<CheckoutResult>{conflictsResult.exception()};
      }

      CheckoutResult checkoutResult;
      checkoutResult.times = checkoutTimes;
      checkoutResult.times.didFinish = stopWatch.elapsed();
      checkoutResult.conflicts = std::move(conflictsResult).value();
      if (ctx->isDryRun()) {
        // This is a dry run, so all we need to do is tell the caller
        // about the conflicts: we should not modify any files or add
        // any entries to the journal.
        return folly::Try<CheckoutResult>{std::move(checkoutResult)};
      }

      {
        auto journalSpan = ctx->createSpan("journalUpdate");

        // Write a journal entry
        //
        // Note that we do not call journalDiffCallback->performDiff() a
        // second time here to compute the files that are now different
        // from the new state.  The checkout operation will only touch
        // files that are changed between fromTree and toTree.
        //
        // Any files that are unclean after the checkout operation must
        // have either been unclean before it started, or different
        // between the two trees.  Therefore the JournalDelta already
        // includes information that these files changed.
        auto uncleanPaths = journalDiffCallback->stealUncleanPaths();
        journal_->recordUncleanPaths(
            oldParent, snapshotId, std::move(uncleanPaths));

        // Record journal entries for file changes caused by
        // a force checkout. Do this here after the checkout finishes
        // in case there are any errors during the checkout.
        if (checkoutMode == CheckoutMode::FORCE) {
          for (const auto& conflict : checkoutResult.conflicts) {
            switch (conflict.type().value()) {
              case ConflictType::ERROR:
                // Ignore
                break;
              case ConflictType::MODIFIED_REMOVED:
                journal_->recordRemoved(
                    RelativePathPiece(conflict.path().value()),
                    static_cast<dtype_t>(conflict.dtype().value()));
                break;
              case ConflictType::UNTRACKED_ADDED:
                // An untracked file in current differs from a tracked
                // file inside the target commit. Basically a modify
                journal_->recordChanged(
                    RelativePathPiece(conflict.path().value()),
                    static_cast<dtype_t>(conflict.dtype().value()));
                break;
              case ConflictType::REMOVED_MODIFIED:
                // Not sure if the created is required
                journal_->recordCreated(
                    RelativePathPiece(conflict.path().value()),
                    static_cast<dtype_t>(conflict.dtype().value()));
                journal_->recordChanged(
                    RelativePathPiece(conflict.path().value()),
                    static_cast<dtype_t>(conflict.dtype().value()));
                break;
              case ConflictType::MISSING_REMOVED:
                // By the comment in MISSING_REMOVED, the file should
                // have already been removed from the filesystem so it
                // should already be recorded
                break;
              case ConflictType::MODIFIED_MODIFIED:
                journal_->recordChanged(
                    RelativePathPiece(conflict.path().value()),
                    static_cast<dtype_t>(conflict.dtype().value()));
                break;
              case ConflictType::DIRECTORY_NOT_EMPTY:
                // Ignore
                break;
              default:
                // Ignore
                break;
            }
          }
        }
      }

      return folly::Try<CheckoutResult>{std::move(checkoutResult)};
    } catch (const std::exception&) {
      return folly::Try<CheckoutResult>{
          folly::exception_wrapper{std::current_exception()}};
    }
  }();

  auto fetchStats = ctx->getStatsContext().computeStatistics();
  auto inodeCounts = getInodeMap()->getInodeCounts();

  XLOGF(
      DBG1,
      "{}checkout for {} from {} to {} accessed {} trees ({}% chr), {} blobs ({}% chr), and {} metadata ({}% chr).",
      result.hasValue() ? "" : "failed ",
      this->getPath(),
      objectStore_->displayRootId(oldParent),
      objectStore_->displayRootId(snapshotId),
      fetchStats.tree.accessCount,
      fetchStats.tree.cacheHitRate,
      fetchStats.blob.accessCount,
      fetchStats.blob.cacheHitRate,
      fetchStats.blobAuxData.accessCount,
      fetchStats.blobAuxData.cacheHitRate);

  auto checkoutTimeInSeconds =
      std::chrono::duration<double>{stopWatch.elapsed()};

  uint64_t numConflicts = 0;
  uint64_t numErrors = 0;
  std::string firstError;
  if (result.hasValue()) {
    auto& conflicts = result.value().conflicts;
    numConflicts = conflicts.size();

    for (const auto& conflict : conflicts) {
      if (conflict.type() == ConflictType::ERROR) {
        ++numErrors;
        if (firstError.empty()) {
          firstError = fmt::format(
              "{}: {}", conflict.path().value(), conflict.message().value());
        }
      }
    }

    if (!ctx->isDryRun()) {
      const auto maxConflictsToPrint =
          getEdenConfig()->numConflictsToLog.getValue();
      uint64_t printedConflicts = 0ull;
      for (const auto& conflict : conflicts) {
        if (printedConflicts == maxConflictsToPrint) {
          XLOGF(
              DBG2,
              "And {} more checkout conflicts",
              (numConflicts - printedConflicts));
          break;
        }
        XLOGF(
            DBG2,
            "Checkout conflict on: {} of type {} with dtype {}",
            conflict,
            conflict.type().value(),
            static_cast<int>(conflict.dtype().value()));
        printedConflicts++;
      }
    }
  } else {
    numErrors = 1;
    firstError = folly::exceptionStr(result.exception()).toStdString();
  }

  // Don't log aux data fetches, because our backends don't yet support
  // fetching aux data directly. We expect tree fetches to eventually
  // return aux data for their entries.
  auto finishedCheckout = FinishedCheckout{
      getCheckoutModeString(checkoutMode).str(),
      checkoutTimeInSeconds.count(),
      result.hasValue(),
      fetchStats.tree.fetchCount,
      fetchStats.blob.fetchCount,
      fetchStats.blobAuxData.fetchCount,
      fetchStats.tree.accessCount,
      fetchStats.blob.accessCount,
      fetchStats.blobAuxData.accessCount,
      numConflicts,
      inodeCounts.treeCount + inodeCounts.fileCount,
      inodeCounts.unloadedInodeCount,
      inodeCounts.periodicLinkedUnloadInodeCount,
      inodeCounts.periodicUnlinkedUnloadInodeCount};
  finishedCheckout.populateError(numErrors, std::move(firstError));
  if (result.hasValue()) {
    finishedCheckout.populateCheckoutDurations(
        std::chrono::duration_cast<std::chrono::microseconds>(
            result.value().times.didLookupTrees)
            .count(),
        std::chrono::duration_cast<std::chrono::microseconds>(
            result.value().times.didDiff)
            .count(),
        std::chrono::duration_cast<std::chrono::microseconds>(
            result.value().times.didAcquireRenameLock)
            .count(),
        std::chrono::duration_cast<std::chrono::microseconds>(
            result.value().times.didCheckout)
            .count(),
        std::chrono::duration_cast<std::chrono::microseconds>(
            result.value().times.didFinish)
            .count());
  }
  this->serverState_->getEdenFsEventsLogger()->logEvent(finishedCheckout);

  return result;
}

ImmediateFuture<CheckoutResult> EdenMount::checkout(
    TreeInodePtr rootInode,
    const RootId& snapshotId,
    const ObjectFetchContextPtr& fetchContext,
    folly::StringPiece thriftMethodCaller,
    CheckoutMode checkoutMode) {
  auto config = serverState_->getReloadableConfig()->getEdenConfig();
  if (config->enableCoroutines.getValue() &&
      config->enableCoroutinesPhase7.getValue()) {
    return ImmediateFuture{
        // @lint-ignore CLANGTIDY facebook-folly-coro-return-captures-local-var
        folly::coro::co_invoke(
            [self = shared_from_this()](
                TreeInodePtr ri,
                RootId sid,
                ObjectFetchContextPtr fc,
                std::string caller,
                CheckoutMode mode) -> folly::coro::Task<CheckoutResult> {
              co_return co_await self->co_checkout(
                  std::move(ri), sid, fc, caller, mode);
            },
            std::move(rootInode),
            snapshotId,
            fetchContext.copy(),
            thriftMethodCaller.str(),
            checkoutMode)
            .semi()};
  }

  const folly::stop_watch<> stopWatch;
  auto checkoutTimes = std::make_shared<CheckoutTimes>();

  auto setupTry =
      beginCheckout(snapshotId, fetchContext, thriftMethodCaller, checkoutMode);
  if (setupTry.hasException()) {
    return makeFuture<CheckoutResult>(std::move(setupTry).exception());
  }
  auto setup = std::move(setupTry).value();
  auto ctx = std::move(setup.ctx);
  RootId oldParent = std::move(setup.oldParent);
  ParentCommitState::CheckoutState oldState = std::move(setup.oldState);
  ImmediateFuture<folly::Unit> firstFaultCheck =
      std::move(setup.firstFaultCheck);

  XLOGF(
      DBG1,
      "starting checkout for {}: {} to {}",
      this->getPath(),
      objectStore_->displayRootId(oldParent),
      objectStore_->displayRootId(snapshotId));

  // Update lastCheckoutTime_ before starting the checkout operation.
  // This ensures that any inode objects created once the checkout starts will
  // get the current checkout time, rather than the time from the previous
  // checkout
  setLastCheckoutTime(EdenTimestamp{clock_->getRealtime()});

  setWalkDetectorParent(snapshotId);

  auto journalDiffCallback = std::make_shared<JournalDiffCallback>();

  using RootTreeTuple = std::
      tuple<ObjectStore::GetRootTreeResult, ObjectStore::GetRootTreeResult>;

  return std::move(firstFaultCheck)
      .thenValue([this, ctx, parent1Id = oldParent, snapshotId](auto&&) {
        XLOG(DBG7, "Checkout: getRoots");
        auto fromTreeFuture =
            objectStore_->getRootTree(parent1Id, ctx->getFetchContext());
        auto toTreeFuture =
            objectStore_->getRootTree(snapshotId, ctx->getFetchContext());
        return collectAllSafe(fromTreeFuture, toTreeFuture);
      })
      .thenValue([this](RootTreeTuple treeResults) {
        XLOG(DBG7, "Checkout: waitForPendingWrites");
        return waitForPendingWrites().thenValue(
            [treeResults = std::move(treeResults)](auto&&) {
              return treeResults;
            });
      })
      .thenValue(
          [this,
           rootInode,
           ctx,
           checkoutTimes,
           stopWatch,
           journalDiffCallback,
           resumingCheckout =
               std::holds_alternative<ParentCommitState::InterruptedCheckout>(
                   oldState)](
              RootTreeTuple treeResults) -> ImmediateFuture<RootTreeTuple> {
            XLOG(DBG7, "Checkout: performDiff");
            checkoutTimes->didLookupTrees = stopWatch.elapsed();
            // Call JournalDiffCallback::performDiff() to compute the changes
            // between the original working directory state and the source
            // tree state.
            //
            // If we are doing a dry-run update we aren't going to create a
            // journal entry, so we can skip this step entirely.
            if (ctx->isDryRun()) {
              return treeResults;
            }

            ctx->throwIfCanceled();

            auto& fromTree = std::get<0>(treeResults);
            auto trees = std::vector{fromTree.tree};
            if (resumingCheckout) {
              trees.push_back(std::get<1>(treeResults).tree);
            }
            auto diffSpan = ctx->createSpan("performDiff");
            return journalDiffCallback
                ->performDiff(this, rootInode, std::move(trees), ctx)
                .thenValue([ctx,
                            journalDiffCallback,
                            treeResults,
                            diffSpan = std::move(diffSpan)](
                               const StatsFetchContext& diffFetchContext) {
                  ctx->getStatsContext().merge(diffFetchContext);
                  return treeResults;
                });
          })
      .thenValue([this,
                  rootInode,
                  ctx,
                  checkoutTimes,
                  stopWatch,
                  oldParent,
                  snapshotId](RootTreeTuple treeResults) {
        checkoutTimes->didDiff = stopWatch.elapsed();

        // Perform the requested checkout operation after the journal diff
        // completes. This also updates the SNAPSHOT file to make sure that an
        // interrupted checkout can be properly detected.
        RenameLock renameLock;
        {
          auto renameLockSpan = ctx->createSpan("acquireRenameLock");
          renameLock = this->acquireRenameLock();
        }
        ctx->start(
            std::move(renameLock),
            parentState_.wlock(),
            oldParent,
            snapshotId,
            std::get<1>(treeResults).tree);

        checkoutTimes->didAcquireRenameLock = stopWatch.elapsed();

        // If a significant number of tree inodes are loaded or referenced
        // by FUSE, then checkout is slow, because Eden must precisely
        // manage changes to each one, as if the checkout was actually
        // creating and removing files in each directory. If a tree is
        // unloaded and unmodified, Eden can pretend the checkout
        // operation blew away the entire subtree and assigned new inode
        // numbers to everything under it, which is much cheaper.
        //
        // To make checkout faster, enumerate all loaded, unreferenced
        // inodes and unload them, allowing checkout to use the fast path.
        //
        // Note that this will not unload any inodes currently referenced
        // by FUSE, including the kernel's cache, so rapidly switching
        // between commits while working should not be materially
        // affected.
        //
        // On Windows, most of the above is also true, but instead of files
        // being referenced by the kernel, the files are actually on disk. All
        // the files on disk must also be present in the overlay, and thus the
        // checkout code will take care of doing the right invalidation for
        // these.
        //
        {
          auto unloadSpan = ctx->createSpan("unloadChildrenUnreferencedByFs");
          rootInode->unloadChildrenUnreferencedByFs();
        }

        return serverState_->getFaultInjector()
            .checkAsync("inodeCheckout", getPath().view())
            .thenValue([ctx, treeResults = std::move(treeResults), rootInode](
                           auto&&) mutable {
              ctx->throwIfCanceled();

              auto& [fromTree, toTree] = treeResults;
              return rootInode->checkout(ctx.get(), fromTree.tree, toTree.tree);
            });
      })
      .thenValue([ctx, checkoutTimes, stopWatch, snapshotId](auto&&) {
        checkoutTimes->didCheckout = stopWatch.elapsed();

        // Complete the checkout
        return ctx->finish(snapshotId);
      })
      .thenTry([checkoutGuard =
                    CheckoutInProgressGuard{
                        *this, ctx, oldParent, snapshotId, oldState}](
                   folly::Try<std::vector<CheckoutConflict>>&& res) mutable {
        return checkoutGuard.finish(std::move(res));
      })
      .thenTry(
          [this,
           ctx,
           checkoutMode,
           checkoutTimes,
           stopWatch,
           oldParent,
           snapshotId,
           journalDiffCallback](Try<std::vector<CheckoutConflict>>&& result) {
            return finalizeCheckout(
                ctx,
                checkoutMode,
                *checkoutTimes,
                stopWatch,
                oldParent,
                snapshotId,
                journalDiffCallback.get(),
                std::move(result));
          });
}

folly::coro::now_task<CheckoutResult> EdenMount::co_checkout(
    TreeInodePtr rootInode,
    const RootId& snapshotId,
    const ObjectFetchContextPtr& fetchContext,
    folly::StringPiece thriftMethodCaller,
    CheckoutMode checkoutMode) {
  auto setupTry =
      beginCheckout(snapshotId, fetchContext, thriftMethodCaller, checkoutMode);
  if (setupTry.hasException()) {
    co_yield folly::coro::co_error(std::move(setupTry).exception());
  }
  auto setup = std::move(setupTry).value();
  folly::stop_watch<> stopWatch{};
  CheckoutTimes checkoutTimes{};

  std::shared_ptr<CheckoutContext> ctx = std::move(setup.ctx);
  RootId oldParent = std::move(setup.oldParent);
  ParentCommitState::CheckoutState oldState = std::move(setup.oldState);
  CheckoutInProgressGuard checkoutGuard{
      *this, ctx, oldParent, snapshotId, oldState};
  ImmediateFuture<folly::Unit> firstFaultCheck =
      std::move(setup.firstFaultCheck);

  XLOGF(
      DBG1,
      "starting checkout for {}: {} to {}",
      this->getPath(),
      objectStore_->displayRootId(oldParent),
      objectStore_->displayRootId(snapshotId));

  setLastCheckoutTime(EdenTimestamp{clock_->getRealtime()});
  setWalkDetectorParent(snapshotId);

  auto journalDiffCallback = std::make_shared<JournalDiffCallback>();
  bool resumingCheckout =
      std::holds_alternative<ParentCommitState::InterruptedCheckout>(oldState);

  // Capture the result of the inner checkout work (everything from
  // acquireRenameLock through ctx->finish) so we can always reset the
  // parent state below, on both the success and failure paths.
  folly::Try<std::vector<CheckoutConflict>> conflictsResult;
  try {
    co_await std::move(firstFaultCheck).semi();

    XLOG(DBG7, "Checkout: getRoots");
    auto [fromTreeRes, toTreeRes] = co_await folly::coro::collectAll(
        folly::coro::co_invoke(
            [this,
             oldParent,
             ctx]() -> folly::coro::Task<ObjectStore::GetRootTreeResult> {
              co_return co_await objectStore_->co_getRootTree(
                  oldParent, ctx->getFetchContext());
            }),
        folly::coro::co_invoke(
            [this,
             snapshotId,
             ctx]() -> folly::coro::Task<ObjectStore::GetRootTreeResult> {
              co_return co_await objectStore_->co_getRootTree(
                  snapshotId, ctx->getFetchContext());
            }));

    XLOG(DBG7, "Checkout: waitForPendingWrites");
    co_await co_waitForPendingWrites();

    checkoutTimes.didLookupTrees = stopWatch.elapsed();

    XLOG(DBG7, "Checkout: performDiff");
    if (!ctx->isDryRun()) {
      ctx->throwIfCanceled();

      auto trees = std::vector{fromTreeRes.tree};
      if (resumingCheckout) {
        trees.push_back(toTreeRes.tree);
      }
      auto diffSpan = ctx->createSpan("performDiff");
      auto diffFetchContext = co_await journalDiffCallback->co_performDiff(
          this, rootInode, std::move(trees), ctx);
      ctx->getStatsContext().merge(diffFetchContext);
    }

    checkoutTimes.didDiff = stopWatch.elapsed();

    {
      auto renameLockSpan = ctx->createSpan("acquireRenameLock");
      auto renameLock = acquireRenameLock();
      ctx->start(
          std::move(renameLock),
          parentState_.wlock(),
          oldParent,
          snapshotId,
          toTreeRes.tree);
    }

    checkoutTimes.didAcquireRenameLock = stopWatch.elapsed();

    {
      auto unloadSpan = ctx->createSpan("unloadChildrenUnreferencedByFs");
      rootInode->unloadChildrenUnreferencedByFs();
    }

    co_await serverState_->getFaultInjector().co_checkAsync(
        "inodeCheckout", getPath().view());
    ctx->throwIfCanceled();

    co_await rootInode->co_checkout(
        ctx.get(), fromTreeRes.tree, toTreeRes.tree);

    checkoutTimes.didCheckout = stopWatch.elapsed();

    auto conflicts = co_await ctx->co_finish(snapshotId);
    conflictsResult =
        folly::Try<std::vector<CheckoutConflict>>{std::move(conflicts)};
  } catch (...) {
    conflictsResult = folly::Try<std::vector<CheckoutConflict>>{
        folly::exception_wrapper{std::current_exception()}};
  }

  conflictsResult = checkoutGuard.finish(std::move(conflictsResult));

  auto finalResult = finalizeCheckout(
      ctx,
      checkoutMode,
      checkoutTimes,
      stopWatch,
      oldParent,
      snapshotId,
      journalDiffCallback.get(),
      std::move(conflictsResult));

  if (finalResult.hasException()) {
    finalResult.exception().throw_exception();
  }
  co_return std::move(finalResult).value();
}

void EdenMount::forgetStaleInodes() {
  inodeMap_->forgetStaleInodes();
}

ImmediateFuture<folly::Unit> EdenMount::flushInvalidations() {
  XLOG(DBG4, "waiting for inode invalidations to complete");
  auto ch = channel_.load();
  if (ch) {
    return ch->completeInvalidations().thenValue([ch](folly::Unit) {
      XLOG(DBG4, "finished processing inode invalidations");
    });
  } else {
    return folly::unit;
  }
}

folly::coro::now_task<folly::Unit> EdenMount::co_flushInvalidations() {
  XLOG(DBG4, "waiting for inode invalidations to complete");
  auto ch = channel_.load();
  if (ch) {
    co_await ch->co_completeInvalidations();
    XLOG(DBG4, "finished processing inode invalidations");
  }
  co_return folly::unit;
}

#ifndef _WIN32
ImmediateFuture<folly::Unit>
EdenMount::chown(uid_t uid, gid_t gid, const ObjectFetchContextPtr& context) {
  // 1) Ensure we are running in either a fuse or nfs mount
  Nfsd3* nfsChannel = nullptr;
  FuseChannel* fuseChannel = getFuseChannel();
  if (!fuseChannel) {
    nfsChannel = getNfsdChannel();
    if (!nfsChannel) {
      return makeFuture<Unit>(newEdenError(
          EdenErrorType::GENERIC_ERROR,
          "chown is currently implemented for FUSE and NFS mounts only"));
    }
  }

  // 2) Ensure that all future opens will by default provide this owner
  setOwner(uid, gid);

  // 3) Modify all uids/gids of files stored in the overlay
  auto metadataTable = getInodeMetadataTable();
  XDCHECK(metadataTable) << "Unexpected null Metadata Table";
  metadataTable->forEachModify([&](auto& /* unused */, auto& record) {
    record.uid = uid;
    record.gid = gid;
  });

  // Note that any files being created at this point are not
  // guaranteed to have the requested uid/gid, but that raciness is
  // consistent with the behavior of chown

  // 4) Invalidate all inodes that the kernel holds a reference to
  auto inodeMap = getInodeMap();
  auto inodesToInvalidate = inodeMap->getReferencedInodes();
  if (fuseChannel) {
    fuseChannel->invalidateInodes(folly::range(inodesToInvalidate));
    return fuseChannel->completeInvalidations();
  } else {
    // Load all Inodes - there should only be a few
    // as chown is called primarily in Sandcastle workflows
    // where the repo has just been cloned.
    std::vector<ImmediateFuture<InodePtr>> futures;
    futures.reserve(inodesToInvalidate.size());
    for (const auto& ino : inodesToInvalidate) {
      futures.emplace_back(inodeMap->lookupInode(ino, context));
    }

    return collectAllSafe(std::move(futures))
        .thenValue([this, nfsChannel, metadataTable](auto&& inodes) {
          auto renameLock = acquireRenameLock();
          auto root = getPath();

          std::vector<std::pair<AbsolutePath, mode_t>> pathsAndModes;
          for (auto& inode : inodes) {
            auto metadata = metadataTable->getOptional(inode->getNodeId());
            if (!metadata.has_value()) {
              XLOGF(
                  WARNING,
                  "Inode ({}) not found in metadata table",
                  inode->getNodeId());
              continue;
            }

            auto path = inode->getPath();
            if (path.has_value()) {
              pathsAndModes.emplace_back(
                  root + path.value(), metadata.value().mode);
            }
          }

          nfsChannel->invalidateInodes(pathsAndModes);
          return nfsChannel->completeInvalidations();
        });
  }
}
#endif

std::unique_ptr<DiffContext> EdenMount::createDiffContext(
    DiffCallback* callback,
    const folly::CancellationToken& cancellation,
    const ObjectFetchContextPtr& fetchContext,
    bool listIgnored) const {
  return make_unique<DiffContext>(
      callback,
      cancellation,
      fetchContext,
      listIgnored,
      getCheckoutConfig()->getCaseSensitive(),
      getObjectStore(),
      serverState_->getTopLevelIgnores(),
      getEdenConfig()->throwOnCancel.getValue(),
      serverState_->getGlobMatchOptions());
}

folly::coro::now_task<std::unique_ptr<ScmStatus>> EdenMount::co_diff(
    TreeInodePtr rootInode,
    const RootId& commitId,
    folly::CancellationToken cancellation,
    const ObjectFetchContextPtr& fetchContext,
    bool listIgnored,
    bool enforceCurrentParent) {
  // Step 1: Create the callback (from public diff overload)
  auto callback = std::make_unique<ScmStatusDiffCallback>();
  auto callbackPtr = callback.get();

  // Step 2: Parent validation (from full diff overload, synchronous)
  RootId currentWorkingCopyParentRootId;
  {
    auto parentInfo = parentState_.rlock();
    currentWorkingCopyParentRootId = parentInfo->workingCopyParentRootId;
    if (enforceCurrentParent) {
      if (std::holds_alternative<ParentCommitState::CheckoutInProgress>(
              parentInfo->checkoutState)) {
        throw newEdenError(
            EdenErrorType::CHECKOUT_IN_PROGRESS,
            "cannot compute status while a checkout is currently in progress");
      } else if (
          auto* interrupted =
              std::get_if<ParentCommitState::InterruptedCheckout>(
                  &parentInfo->checkoutState)) {
        throw newEdenError(
            EdenErrorType::CHECKOUT_IN_PROGRESS,
            fmt::format(
                interruptedCheckoutAdvice,
                folly::hexlify(
                    objectStore_->renderRootId(interrupted->toCommit))));
      }

      if (objectStore_->compareRootsById(
              currentWorkingCopyParentRootId, commitId) !=
          ObjectComparison::Identical) {
        auto renderedParentRootId =
            objectStore_->renderRootId(currentWorkingCopyParentRootId);
        auto renderedCommitId = objectStore_->renderRootId(commitId);

        getServerState()->getEdenFsEventsLogger()->logEvent(
            ParentMismatch{
                commitId.value(), currentWorkingCopyParentRootId.value()});
        throw newEdenError(
            EdenErrorType::OUT_OF_DATE_PARENT,
            "error computing status: requested parent commit is out-of-date: requested ",
            folly::hexlify(renderedCommitId),
            ", but current parent commit is ",
            folly::hexlify(renderedParentRootId),
            ".\nTry running `eden doctor` to remediate");
      }
    }
  }

  // Step 3: Create DiffContext
  auto context =
      createDiffContext(callbackPtr, cancellation, fetchContext, listIgnored);
  DiffContext* ctxPtr = context.get();

  // Helper lambda to perform the inner diff as a coroutine
  auto doInnerDiff = [&]() -> folly::coro::now_task<void> {
    auto faultTry = serverState_->getFaultInjector().checkTry(
        "EdenMount::diff", commitId.value());
    if (faultTry.hasException()) {
      faultTry.throwUnlessValue();
    }
    auto rootTree = co_await objectStore_->co_getRootTree(
        commitId, ctxPtr->getFetchContext());
    co_await co_waitForPendingWrites();
    // GCC 11 hits an internal compiler error when this vector is built inside
    // the co_await expression.
    std::vector<TreePtr> trees{std::move(rootTree.tree)};
    co_await rootInode->co_diff(
        ctxPtr,
        RelativePathPiece{},
        std::move(trees),
        ctxPtr->getToplevelIgnore(),
        false);
  };

  // Step 4: Caching logic
  if (getEdenConfig()->hgEnableCachedResultForStatusRequest.getValue()) {
    auto latestInfo = getJournal().peekLatest();
    if (latestInfo.has_value()) {
      auto key = ScmStatusCache::makeKey(commitId, listIgnored);
      XLOGF(
          DBG7,
          "ScmStatusCache: id={}, listIgnored={}, key={}",
          commitId.value(),
          listIgnored,
          key);
      auto curSequenceID = latestInfo.value().sequenceID;
      std::variant<StatusResultFuture, StatusResultPromise> getResult{nullptr};
      {
        auto lockedCachePtr = scmStatusCache_.wlock();
        auto& cache = *lockedCachePtr;

        if (!cache->isCachedWorkingDirValid(currentWorkingCopyParentRootId)) {
          cache->clear();
          cache->resetCachedWorkingDir(currentWorkingCopyParentRootId);
        }
        getResult = cache->get(key, curSequenceID);
      }

      if (std::holds_alternative<StatusResultFuture>(getResult)) {
        auto future = std::move(std::get<StatusResultFuture>(getResult));
        getStats()->increment(&JournalStats::journalStatusCacheHit);
        if (future.isReady()) {
          callback->setStatus(std::move(future).get());
          co_return std::make_unique<ScmStatus>(callback->extractStatus());
        }
        getStats()->increment(&JournalStats::journalStatusCachePend);
        auto status = co_await std::move(future).semi();
        callback->setStatus(std::move(status));
        co_return std::make_unique<ScmStatus>(callback->extractStatus());
      }

      getStats()->increment(&JournalStats::journalStatusCacheMiss);

      auto promise = std::get<StatusResultPromise>(getResult);

      if (promise.get() != nullptr) {
        try {
          co_await doInnerDiff();
        } catch (...) {
          promise->setTry(
              folly::Try<ScmStatus>{
                  folly::exception_wrapper{std::current_exception()}});
          {
            auto lockedCachePtr = scmStatusCache_.wlock();
            (*lockedCachePtr)->dropPromise(key, promise);
          }
          throw;
        }

        if (ctxPtr->isCancelled()) {
          promise->setException(folly::OperationCancelled{});
          {
            auto lockedCachePtr = scmStatusCache_.wlock();
            (*lockedCachePtr)->dropPromise(key, promise);
          }
          co_return std::make_unique<ScmStatus>(callback->extractStatus());
        }

        bool shouldInsert = true;

        ScmStatus newStatus = callbackPtr->peekStatus();

        if (newStatus.errors()->size() > 0) {
          shouldInsert = false;
        }

        if (newStatus.entries().value().size() >
            getEdenConfig()->scmStatusCacheMaxEntriesPerItem.getValue()) {
          getStats()->increment(&JournalStats::journalStatusCacheSkip);
          shouldInsert = false;
        }

        // FaultInjector check point: for testing only
        serverState_->getFaultInjector().check(
            "scmStatusCache", "blocking setValue");

        promise->setValue(newStatus);

        // FaultInjector check point: for testing only
        serverState_->getFaultInjector().check(
            "scmStatusCache", "blocking insert");
        {
          auto lockedCachePtr = scmStatusCache_.wlock();
          if (shouldInsert) {
            (*lockedCachePtr)->insert(key, curSequenceID, std::move(newStatus));
          }

          // FaultInjector check point: for testing only
          serverState_->getFaultInjector().check(
              "scmStatusCache", "blocking dropPromise");

          (*lockedCachePtr)->dropPromise(key, promise);
        }
        co_return std::make_unique<ScmStatus>(callback->extractStatus());
      }
      XLOGF(
          ERR,
          "ScmStatusCache returned nullptr for promise: key={}, commitId={}, listIgnored={}, curSequenceID={}. Falling back to no-cache path for this request",
          key,
          commitId,
          listIgnored,
          curSequenceID);
    }
  }

  // Step 5: No-cache fallback path
  co_await doInnerDiff();

  // Step 6: Extract and return status
  co_return std::make_unique<ScmStatus>(callback->extractStatus());
}

void EdenMount::resetParent(const RootId& parent) {
  // Hold the snapshot lock around the entire operation.
  auto parentLock = parentState_.wlock();

  if (parentLock->isCheckoutInProgressOrInterrupted()) {
    throw newEdenError(
        EdenErrorType::CHECKOUT_IN_PROGRESS,
        "cannot reset parent while a checkout is currently in progress");
  }

  auto oldParent = parentLock->workingCopyParentRootId;
  XLOGF(
      DBG1,
      "resetting snapshot for {} from {} to {}",
      this->getPath(),
      objectStore_->displayRootId(oldParent),
      objectStore_->displayRootId(parent));

  // TODO: Maybe we should walk the inodes and see if we can dematerialize
  // some files using the new source control state.

  checkoutConfig_->setWorkingCopyParentCommit(parent);
  parentLock->workingCopyParentRootId = parent;
  setWalkDetectorParent(parent);

  journal_->recordRootUpdate(oldParent, parent);
}

EdenTimestamp EdenMount::getLastCheckoutTime() const {
  static_assert(std::atomic<EdenTimestamp>::is_always_lock_free);
  return lastCheckoutTime_.load(std::memory_order_acquire);
}

void EdenMount::setLastCheckoutTime(EdenTimestamp time) {
  lastCheckoutTime_.store(time, std::memory_order_release);
}

bool EdenMount::isCheckoutInProgress() {
  auto parentLock = parentState_.rlock();
  return parentLock->isCheckoutInProgressOrInterrupted();
}

RenameLock EdenMount::acquireRenameLock() {
  return RenameLock{this};
}

SharedRenameLock EdenMount::acquireSharedRenameLock() {
  return SharedRenameLock{this};
}

std::string EdenMount::getCounterName(CounterName name) {
  const auto& mountPath = getPath();
  const auto base = basename(mountPath.view());
  switch (name) {
    case CounterName::INODEMAP_LOADED:
      return folly::to<std::string>("inodemap.", base, ".loaded");
    case CounterName::INODEMAP_UNLOADED:
      return folly::to<std::string>("inodemap.", base, ".unloaded");
    case CounterName::JOURNAL_MEMORY:
      return folly::to<std::string>("journal.", base, ".memory");
    case CounterName::JOURNAL_ENTRIES:
      return folly::to<std::string>("journal.", base, ".count");
    case CounterName::JOURNAL_DURATION:
      return folly::to<std::string>("journal.", base, ".duration_secs");
    case CounterName::JOURNAL_MAX_FILES_ACCUMULATED:
      return folly::to<std::string>("journal.", base, ".files_accumulated.max");
    case CounterName::PERIODIC_INODE_UNLOAD:
      return folly::to<std::string>(
          "inodemap.", base, ".unloaded_linked_inodes");
    case CounterName::PERIODIC_UNLINKED_INODE_UNLOAD:
      return folly::to<std::string>(
          "inodemap.", base, ".unloaded_unlinked_inodes");
  }
  EDEN_BUG() << "unknown counter name "
             << static_cast<std::underlying_type_t<CounterName>>(name);
}

folly::Future<TakeoverData::MountInfo>
EdenMount::getFsChannelCompletionFuture() {
  return fsChannelCompletionPromise_.getFuture();
}

#ifndef _WIN32
namespace {
std::unique_ptr<FuseChannel, FsChannelDeleter> makeFuseChannel(
    EdenMount* mount,
    folly::File fuseFd) {
  auto edenConfig = mount->getEdenConfig();
  return makeFuseChannel(
      mount->getServerState()->getPrivHelper(),
      std::move(fuseFd),
      mount->getPath(),
      mount->getServerState()->getFsChannelThreadPool(),
      mount->getServerState()
          ->getEdenConfig()
          ->fuseNumDispatcherThreads.getValue(),
      EdenDispatcherFactory::makeFuseDispatcher(mount),
      &mount->getStraceLogger(),
      mount->getServerState()->getProcessInfoCache(),
      mount->getServerState()->getEdenFsEventsLogger(),
      mount->getServerState()->getErrorLogger(),
      std::chrono::duration_cast<folly::Duration>(
          edenConfig->fuseRequestTimeout.getValue()),
      mount->getServerState()->getNotifier(),
      mount->getCheckoutConfig()->getRequireUtf8Path(),
      edenConfig->fuseMaximumBackgroundRequests.getValue(),
      edenConfig->maxFsChannelInflightRequests.getValue(),
      edenConfig->highFsRequestsLogInterval.getValue(),
      edenConfig->longRunningFSRequestThreshold.getValue(),
      mount->getCheckoutConfig()->getUseWriteBackCache(),
      mount->getServerState()->getEdenConfig()->FuseTraceBusCapacity.getValue(),
      edenConfig->fuseBdiReadAheadKb.getValue(),
      edenConfig->fuseMaxPages.getValue(),
      edenConfig->experimentalFuseHandleKillPrivV2.getValue(),
      edenConfig->fuseUseIoUring.getValue(),
      edenConfig->fuseIoUringKernelReleaseRegex.getValue(),
      edenConfig->fuseIoUringQueueDepth.getValue(),
      edenConfig->fuseIoUringDisableIoWait.getValue(),
      edenConfig->experimentalFuseIoUringSkipSelfWakeup.getValue(),
      edenConfig->fuseIoUringPreCreateQueues.getValue(),
      edenConfig->fuseNumInvalidationThreads.getValue(),
      edenConfig->fuseBusyPoll.getValue());
}
} // namespace
#endif

namespace {
folly::Future<NfsServer::NfsMountInfo> makeNfsChannel(
    EdenMount* mount,
    std::optional<folly::File> connectedSocket = std::nullopt) {
  auto edenConfig = mount->getEdenConfig();
  auto nfsServer = mount->getServerState()->getNfsServer();
  auto mountPath = mount->getPath();
  // Make sure that we are running on the EventBase while registering
  // the mount point.
  return via(nfsServer->getEventBase(),
             [mount, mountPath, nfsServer, edenConfig]() {
               return nfsServer->registerMount(
                   mountPath,
                   mount->getRootInode()->getNodeId(),
                   EdenDispatcherFactory::makeNfsDispatcher(mount),
                   &mount->getStraceLogger(),
                   mount->getServerState()->getProcessInfoCache(),
                   mount->getServerState()->getEdenFsEventsLogger(),
                   mount->getServerState()->getErrorLogger(),
                   std::chrono::duration_cast<folly::Duration>(
                       edenConfig->nfsRequestTimeout.getValue()),
                   mount->getServerState()->getNotifier(),
                   mount->getCheckoutConfig()->getCaseSensitive(),
                   edenConfig->nfsReadIoSize.getValue(),
                   edenConfig->nfsWriteIoSize.getValue(),
                   edenConfig->nfsTraceBusCapacity.getValue(),
                   edenConfig->nfsFastPathRPCs.getValue(),
                   mount->getServerState()->getReloadableConfig(),
                   mount->getServerState()->getFaultInjector());
             })
      .thenValue(
          [mount, nfsServer, connectedSocket = std::move(connectedSocket)](
              NfsServer::NfsMountInfo mountInfo) mutable {
            auto [channel, mountdAddr] = std::move(mountInfo);

            if (connectedSocket) {
              XLOGF(
                  DBG4,
                  "Mount takeover: Initiating nfsd with socket: {}",
                  connectedSocket.value().fd());
              channel->initialize(std::move(connectedSocket.value()));
              // TODO: we should register the NFS server on takeover too. but
              // we only transfer the connected socket not the listening socket.
              // the listening one is the one we wanna register. So we need to
              // transfer that socket to be able to register it.
            } else {
              // The kernel requires a checkout's mountd and nfsd sockets to be
              // of the same type, so follow the running mountd rather than
              // nfs:use-uds, which only applies when a mountd is created.
              std::optional<AbsolutePath> unixSocketPath;
              if (nfsServer->getMountdAddr().getFamily() == AF_UNIX) {
                unixSocketPath =
                    mount->getCheckoutConfig()->getClientDirectory() +
                    kNfsdSocketName;
                XLOGF(
                    DBG4,
                    "Normal Start: Initiating nfsd from scratch: {}",
                    unixSocketPath.value());
              }
              channel->initialize(makeNfsSocket(std::move(unixSocketPath)));
            }
            return NfsServer::NfsMountInfo{
                std::move(channel), std::move(mountdAddr)};
          });
}
} // namespace

folly::Future<folly::Unit> EdenMount::fsChannelMount(bool readOnly) {
  return folly::makeFutureWith([&] { return &beginMount(); })
      .thenValue([this, readOnly](folly::Promise<folly::Unit>* mountPromise) {
        AbsolutePath mountPath = getPath();
        auto edenConfig = getEdenConfig();

        if (shouldBeOrIsNfsChannel()) {
          NFSMountOptions options;
          options.useReaddirplus = edenConfig->useReaddirplus.getValue();
          options.useSoftMount = edenConfig->useSoftMounts.getValue();
          options.readOnly = readOnly;
          options.readIOSize = edenConfig->nfsReadIoSize.getValue();
          options.writeIOSize = edenConfig->nfsWriteIoSize.getValue();
          options.directoryReadSize =
              edenConfig->nfsDirectoryReadSize.getValue();
          options.readAheadSize = edenConfig->nfsReadAhead.getValue();
          options.retransmitTimeoutTenthSeconds =
              edenConfig->nfsRetransmitTimeoutTenthSeconds.getValue();
          options.retransmitAttempts =
              edenConfig->nfsRetransmitAttempts.getValue();
          options.deadTimeoutSeconds =
              edenConfig->nfsDeadTimeoutSeconds.getValue();
          options.dumbtimer = edenConfig->nfsDumbtimer.getValue();

          // Make sure that we are running on the EventBase while registering
          // the mount point.
          auto fut = makeNfsChannel(this);
          return std::move(fut).thenValue(
              [this,
               options = std::move(options),
               mountPromise = std::move(mountPromise),
               mountPath = std::move(mountPath)](
                  NfsServer::NfsMountInfo mountInfo) mutable {
                auto [channel, mountdAddr] = std::move(mountInfo);
                options.mountdAddr = mountdAddr;
#ifndef _WIN32
                // Channel is later moved. We must assign addr to a local var
                // to avoid the possibility of a use-after-move bug.
                auto addr = channel->getAddr();
                options.nfsdAddr = addr;

                // For testing purposes only: allow tests to force an exception
                // that mimics privhelper mount failing
                serverState_->getFaultInjector().check(
                    "failMountInitialization", mountPath.view());

                // TODO: teach privhelper or something to mount on Windows
                return serverState_->getPrivHelper()
                    ->nfsMount(mountPath.view(), options)
                    .thenTry([this,
                              mountPromise = std::move(mountPromise),
                              channel_2 = std::move(channel)](
                                 Try<folly::Unit>&& try_) mutable {
                      if (try_.hasException()) {
                        mountPromise->setException(try_.exception());
                        return folly::makeFuture<folly::Unit>(try_.exception());
                      }

                      mountPromise->setValue();
                      setChannel(std::move(channel_2));
                      return makeFuture(folly::unit);
                    });
#else
                (void)options;
                mountPromise->setValue();
                setChannel(std::move(channel));
                return folly::makeFutureWith([]() { NOT_IMPLEMENTED(); });
#endif
              });
        }

#ifdef _WIN32
        return folly::makeFutureWith([this,
                                      mountPath = std::move(mountPath),
                                      edenConfig]() {
                 auto channel = std::unique_ptr<PrjfsChannel, FsChannelDeleter>(
                     new PrjfsChannel(
                         mountPath,
                         EdenDispatcherFactory::makePrjfsDispatcher(this),
                         serverState_->getReloadableConfig(),
                         &getStraceLogger(),
                         serverState_->getEdenFsEventsLogger(),
                         serverState_->getFaultInjector(),
                         serverState_->getProcessInfoCache(),
                         getCheckoutConfig()->getRepoGuid(),
                         true,
                         this->getServerState()->getNotifier(),
                         this->getInvalidationThreadPool()));
                 return FsChannelPtr{std::move(channel)};
               })
            .thenTry([this, mountPromise](Try<FsChannelPtr>&& channel) {
              if (channel.hasException()) {
                mountPromise->setException(channel.exception());
                return makeFuture<folly::Unit>(channel.exception());
              }

              // TODO(xavierd): similarly to the non-Windows code below, we
              // need to handle the case where mount was cancelled.

              mountPromise->setValue();
              setChannel(std::move(channel).value());
              return makeFuture(folly::unit);
            });
#else

        // For testing purposes only: allow tests to force an exception
        // that mimics privhelper mount failing
        serverState_->getFaultInjector().check(
            "failMountInitialization", mountPath.view());
        return serverState_->getPrivHelper()
            ->fuseMount(
                mountPath.view(), readOnly, edenConfig->fuseVfsType.getValue())
            .thenTry(
                [mountPath, mountPromise, this](Try<folly::File>&& fuseDevice)
                    -> folly::Future<folly::Unit> {
                  if (fuseDevice.hasException()) {
                    mountPromise->setException(fuseDevice.exception());
                    return folly::makeFuture<folly::Unit>(
                        fuseDevice.exception());
                  }
                  if (mountingUnmountingState_.rlock()
                          ->fsChannelUnmountStarted()) {
                    fuseDevice->close();
                    return serverState_->getPrivHelper()
                        ->fuseUnmount(mountPath.view(), {})
                        .thenError([](folly::exception_wrapper unmountError) {
                          // TODO(strager): Should we make
                          // EdenMount::unmount() also fail with the same
                          // exception?
                          XLOGF(
                              ERR,
                              "fuseMount was cancelled, but rollback (fuseUnmount) failed: {}",
                              unmountError.what());
                          return folly::makeFuture<folly::Unit>(
                              std::move(unmountError));
                        })
                        .thenValue([mountPath, mountPromise](folly::Unit&&) {
                          auto error = FuseDeviceUnmountedDuringInitialization{
                              mountPath};
                          mountPromise->setException(error);
                          return folly::makeFuture<folly::Unit>(error);
                        });
                  }

                  mountPromise->setValue();
                  setChannel(
                      makeFuseChannel(this, std::move(fuseDevice).value()));
                  return folly::makeFuture(folly::unit);
                });
#endif
      });
}

folly::Future<folly::Unit> EdenMount::startFsChannel(bool readOnly) {
  return folly::makeFutureWith([&] {
           transitionState(
               /*expected=*/State::INITIALIZED, /*newState=*/State::STARTING);

           // Just in case the mount point directory doesn't exist,
           // make a best effort attempt to automatically create it.
           boost::filesystem::path boostMountPath{getPath().value()};
           try {
             boost::filesystem::create_directories(boostMountPath);
           } catch (const boost::filesystem::filesystem_error& e) {
             // If the error is caused by a hanging mount, then we can ignore
             // it. The hanging mount will be dealt with later.
             if (isErrnoFromHangingMount(
                     e.code().value(), this->isNfsdChannel())) {
               XLOGF(
                   ERR,
                   "Failed to create mount point (hanging mount): {}: {}",
                   e.code().value(),
                   e.what());
             } else {
               XLOGF(
                   ERR,
                   "Failed to create mount point: {}: {}",
                   e.code().value(),
                   e.what());
               throw;
             }
           }
           return fsChannelMount(readOnly);
         })
      .thenValue([this](auto&&) -> folly::Future<folly::Unit> {
        auto ch = channel_.load();
        if (!ch) {
          return EDEN_BUG_FUTURE(folly::Unit)
              << "EdenMount::channel_ is not constructed";
        }
        return ch->initialize().thenValue(
            [this](FsChannel::StopFuture mountCompleteFuture) {
              fsChannelInitSuccessful(std::move(mountCompleteFuture));
            });
      })
      .thenError([this](folly::exception_wrapper&& ew) {
        transitionToFsChannelInitializationErrorState();
        return makeFuture<folly::Unit>(std::move(ew));
      });
}

folly::Promise<folly::Unit>& EdenMount::beginMount() {
  auto mountingUnmountingState = mountingUnmountingState_.wlock();
  if (mountingUnmountingState->fsChannelMountPromise.has_value()) {
    EDEN_BUG() << __func__ << " unexpectedly called more than once";
  }
  if (mountingUnmountingState->fsChannelUnmountStarted()) {
    throw EdenMountCancelled{};
  }
  mountingUnmountingState->fsChannelMountPromise.emplace();
  // N.B. Return a reference to the lock-protected fsChannelMountPromise member,
  // then release the lock. This is safe for two reasons:
  //
  // * *fsChannelMountPromise will never be destructed (e.g. by calling
  //   std::optional<>::reset()) or reassigned. (fsChannelMountPromise never
  //   goes from `has_value() == true` to `has_value() == false`.)
  //
  // * folly::Promise is self-synchronizing; getFuture() can be called
  //   concurrently with setValue()/setException().
  return *mountingUnmountingState->fsChannelMountPromise;
}

void EdenMount::preparePostFsChannelCompletion(
    EdenMount::StopFuture fsChannelCompleteFuture) {
  folly::futures::detachOn(
      getServerThreadPool().get(),
      std::move(fsChannelCompleteFuture)
          .deferValue([this](FsStopDataPtr stopData) {
            // TODO: This dynamic_cast is janky. How should we decide whether
            // to tell NfsServer to unregister the mount?
            if (dynamic_cast<Nfsd3::StopData*>(stopData.get())) {
              serverState_->getNfsServer()->unregisterMount(getPath());
            }

            if (stopData->isUnmounted()) {
              inodeMap_->setUnmounted();
            }
            *fsChannelStop_.wlock() = FsChannelStop{
                stopData->getStopReason(),
                stopData->stopRequestedAt.value_or(
                    std::chrono::steady_clock::now())};

            fsChannelCompletionPromise_.setWith([&] {
              return TakeoverData::MountInfo{
                  getPath(),
                  checkoutConfig_->getClientDirectory(),
                  stopData->extractTakeoverInfo(),
                  SerializedInodeMap{} // placeholder
              };
            });
          })
          .deferError([this](folly::exception_wrapper&& ew) {
            XLOGF(ERR, "session complete with err: {}", ew.what());
            fsChannelCompletionPromise_.setException(std::move(ew));
          }));
}

void EdenMount::fsChannelInitSuccessful(
    EdenMount::StopFuture channelCompleteFuture) {
  // Try to transition to the RUNNING state.
  // This state transition could fail if shutdown() was called before we saw
  // the FUSE_INIT message from the kernel.
  transitionState(State::STARTING, State::RUNNING);
  preparePostFsChannelCompletion(std::move(channelCompleteFuture));
}

folly::Future<folly::Unit> EdenMount::takeoverFuse(
    FuseChannelData takeoverData) {
#ifndef _WIN32
  transitionState(State::INITIALIZED, State::STARTING);

  try {
    beginMount().setValue();

    auto channel = makeFuseChannel(this, std::move(takeoverData.fd));
    channel->logTakeoverTransportMismatch(takeoverData.connInfo);
    auto fuseReadyFuture = channel->takeoverReadyFuture();
    auto fuseCompleteFuture =
        channel->initializeFromTakeover(takeoverData.connInfo);
    setChannel(std::move(channel));
    return std::move(fuseReadyFuture)
        .thenValue([self = shared_from_this(),
                    fuseCompleteFuture =
                        std::move(fuseCompleteFuture)](folly::Unit) mutable {
          self->fsChannelInitSuccessful(std::move(fuseCompleteFuture));
        })
        .thenError([self = shared_from_this()](folly::exception_wrapper&& ew) {
          self->transitionToFsChannelInitializationErrorState();
          return folly::makeFuture<folly::Unit>(std::move(ew));
        });
  } catch (const std::exception&) {
    transitionToFsChannelInitializationErrorState();
    return folly::makeFuture<folly::Unit>(
        folly::exception_wrapper(std::current_exception()));
  }
#else
  (void)takeoverData;
  return folly::makeFuture<folly::Unit>(
      std::runtime_error("FUSE not supported on this platform."));
#endif
}

folly::Future<folly::Unit> EdenMount::takeoverNfs(NfsChannelData takeoverData) {
#ifndef _WIN32
  transitionState(State::INITIALIZED, State::STARTING);
  try {
    beginMount().setValue();

    return makeNfsChannel(this, std::move(takeoverData.nfsdSocketFd))
        .thenValue([this](NfsServer::NfsMountInfo mountInfo) {
          auto& channel = mountInfo.nfsd;

          auto stopFuture = channel->getStopFuture();
          this->setChannel(std::move(channel));
          this->fsChannelInitSuccessful(std::move(stopFuture));
        })
        .thenError([this](auto&& err) {
          this->transitionToFsChannelInitializationErrorState();
          return folly::makeFuture<folly::Unit>(std::move(err));
        });
  } catch (const std::exception& err) {
    transitionToFsChannelInitializationErrorState();
    return folly::makeFuture<folly::Unit>(err);
  }
#else
  (void)takeoverData;
  throw std::runtime_error("NFS not supported on this platform.");
#endif
}

InodeMetadata EdenMount::getInitialInodeMetadata(mode_t mode) const {
  auto owner = getOwner();
  return InodeMetadata{
      mode, owner.uid, owner.gid, InodeTimestamps{getLastCheckoutTime()}};
}

struct stat EdenMount::initStatData() const {
  struct stat st = {};

  auto owner = getOwner();
  st.st_uid = owner.uid;
  st.st_gid = owner.gid;
#ifndef _WIN32
  // We don't really use the block size for anything.
  // 4096 is fairly standard for many file systems.
  st.st_blksize = 4096;
#endif

  return st;
}

std::optional<ActivityBuffer<InodeTraceEvent>>
EdenMount::initInodeActivityBuffer() {
  if (serverState_->getEdenConfig()->enableActivityBuffer.getValue()) {
    return std::make_optional<ActivityBuffer<InodeTraceEvent>>(
        serverState_->getEdenConfig()->activityBufferMaxEvents.getValue());
  }
  return std::nullopt;
}

void EdenMount::subscribeInodeActivityBuffer() {
  if (inodeActivityBuffer_.has_value()) {
    inodeTraceHandle_ = inodeTraceBus_->subscribeFunction(
        fmt::format("inode-activitybuffer-{}", getPath().basename()),
        [this](const InodeTraceEvent& event) {
          // Use full path name for the inode event if available, otherwise
          // default to the filename already stored
          try {
            // Note calling getPathForInode acquires the InodeMap data_ lock
            // and an InodeBase's location_ lock. This is safe since we ensure
            // to never publish to tracebus holding the data_ or a location_
            // lock. However, we do still publish holding the EdenMount's
            // Rename and TreeInode's contents_ locks, so we must make sure to
            // NEVER acquire those locks in this subscriber.
            auto relativePath = inodeMap_->getPathForInode(event.ino);
            if (relativePath.has_value()) {
              InodeTraceEvent newTraceEvent = event;
              newTraceEvent.setPath(relativePath->view());
              inodeActivityBuffer_->addEvent(std::move(newTraceEvent));
              return;
            }
          } catch (const std::system_error& /* e */) {
          }
          inodeActivityBuffer_->addEvent(event);
        });
  }
}

void EdenMount::publishInodeTraceEvent(InodeTraceEvent&& event) noexcept {
  if (!getEdenConfig()->enableInodeTraceBus.getValue()) {
    return;
  }
  try {
    inodeTraceBus_->publish(event);
  } catch (const std::exception& e) {
    XLOGF(DBG3, "Error publishing inode event to tracebus: {}", e.what());
  }
}

namespace {
ImmediateFuture<TreeInodePtr> ensureDirectoryExistsHelper(
    TreeInodePtr parent,
    PathComponentPiece childName,
    RelativePathPiece rest,
    const ObjectFetchContextPtr& context) {
  auto contents = parent->lockContentsRead();
  if (auto* child = folly::get_ptr(contents->entries, childName)) {
    if (!child->isDirectory()) {
      throw InodeError(EEXIST, parent, childName);
    }

    contents.unlock();

    if (rest.empty()) {
      return parent->getOrLoadChildTree(childName, context);
    }
    return parent->getOrLoadChildTree(childName, context)
        .thenValue([rest = RelativePath{rest},
                    context = context.copy()](TreeInodePtr child) {
          auto [nextChildName, nextRest] = splitFirst(rest);
          return ensureDirectoryExistsHelper(
              child, nextChildName, nextRest, context);
        });
  }

  contents.unlock();
  TreeInodePtr child;
  try {
    child = parent->mkdir(childName, S_IFDIR | 0755, InvalidationRequired::Yes);
  } catch (std::system_error& e) {
    // If two threads are racing to create the subdirectory, that's fine,
    // just try again.
    if (e.code().value() == EEXIST) {
      return ensureDirectoryExistsHelper(parent, childName, rest, context);
    }
    throw;
  }
  if (rest.empty()) {
    return child;
  }
  auto [nextChildName, nextRest] = splitFirst(rest);
  return ensureDirectoryExistsHelper(child, nextChildName, nextRest, context);
}
} // namespace

ImmediateFuture<TreeInodePtr> EdenMount::ensureDirectoryExists(
    RelativePathPiece fromRoot,
    const ObjectFetchContextPtr& context) {
  if (fromRoot.empty()) {
    return getRootInode();
  }
  auto [childName, rest] = splitFirst(fromRoot);
  return ensureDirectoryExistsHelper(getRootInode(), childName, rest, context);
}

std::optional<EdenMount::InodeGCLease> EdenMount::tryStartInodeGC(
    bool pressureBased) {
  auto mount = shared_from_this();
  auto failureLimit = getEdenConfig()->gcMaxTreeLoadFailures.getValue();
  auto state = inodeGCState_.wlock();
  if (state->gcRunning || state->inhibitorCount != 0) {
    return std::nullopt;
  }

  state->cancellationSource = folly::CancellationSource{};
  state->treeLoadFailureLimit = failureLimit;
  state->remainingTreeLoadFailures = failureLimit;
  auto cancellationToken = state->cancellationSource.getToken();
  state->gcRunning = true;
  state->pressureBased = pressureBased;
  state->startTime = std::chrono::steady_clock::now();
  return InodeGCLease{
      std::move(mount),
      /*representsRunningGc=*/true,
      std::move(cancellationToken)};
}

bool EdenMount::isInodeGCRunning() const {
  return inodeGCState_.rlock()->gcRunning;
}

std::optional<EdenMount::RunningInodeGC> EdenMount::getRunningInodeGC() const {
  auto state = inodeGCState_.rlock();
  if (!state->gcRunning) {
    return std::nullopt;
  }
  return RunningInodeGC{
      std::chrono::steady_clock::now() - state->startTime,
      state->pressureBased};
}

void EdenMount::recordInodeGCTreeLoadFailure() {
  auto cancellationSource = folly::CancellationSource::invalid();
  uint64_t failureLimit;
  {
    auto state = inodeGCState_.wlock();
    if (!state->gcRunning || state->remainingTreeLoadFailures == 0 ||
        --state->remainingTreeLoadFailures != 0) {
      return;
    }
    pressureGcBackoff_.store(true, std::memory_order_relaxed);
    cancellationSource = state->cancellationSource;
    failureLimit = state->treeLoadFailureLimit;
  }
  // Cancellation callbacks may run inline and acquire the GC state lock.
  cancellationSource.requestCancellation();
  XLOGF(
      WARN,
      "Cancelling inode GC for {} after {} tree-load failures; pressure-based "
      "GC will wait the regular GC period before running again",
      getPath(),
      failureLimit);
}

EdenMount::InodeGCLease EdenMount::stealInodeGCLease() {
  auto mount = shared_from_this();
  folly::CancellationSource cancellationSource;
  {
    auto state = inodeGCState_.wlock();
    cancellationSource = state->cancellationSource;
    ++state->inhibitorCount;
  }
  // Cancellation callbacks may run inline, so request cancellation only after
  // reserving GC admission and releasing the state lock.
  cancellationSource.requestCancellation();
  return InodeGCLease{
      std::move(mount),
      /*representsRunningGc=*/false,
      folly::CancellationToken{}};
}

EdenMount::InodeGCLease::~InodeGCLease() {
  release();
}

EdenMount::InodeGCLease::InodeGCLease(InodeGCLease&& other) noexcept
    : mount_{std::move(other.mount_)},
      representsRunningGc_{other.representsRunningGc_},
      cancellationToken_{std::move(other.cancellationToken_)} {}

void EdenMount::InodeGCLease::release() noexcept {
  if (mount_) {
    mount_->releaseInodeGCLease(representsRunningGc_);
    mount_.reset();
  }
}

void EdenMount::releaseInodeGCLease(bool representsRunningGc) noexcept {
  auto state = inodeGCState_.wlock();
  if (representsRunningGc) {
    XDCHECK(state->gcRunning);
    state->gcRunning = false;
  } else {
    XDCHECK_GT(state->inhibitorCount, 0u);
    --state->inhibitorCount;
  }
}

bool EdenMount::MountingUnmountingState::fsChannelMountStarted()
    const noexcept {
  return fsChannelMountPromise.has_value();
}

bool EdenMount::MountingUnmountingState::fsChannelUnmountStarted()
    const noexcept {
  return fsChannelUnmountPromise.has_value();
}

EdenMountCancelled::EdenMountCancelled()
    : std::runtime_error{"EdenMount was unmounted during initialization"} {}

} // namespace facebook::eden
