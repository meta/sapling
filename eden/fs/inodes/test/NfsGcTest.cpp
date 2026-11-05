/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <chrono>
#include <memory>
#include <set>
#include <string>
#include <thread>
#include <vector>

#include <fb303/ServiceData.h>
#include <fb303/ThreadCachedServiceData.h>
#include <folly/CancellationToken.h>
#include <folly/container/F14Set.h>
#include <folly/executors/ManualExecutor.h>
#include <folly/io/Cursor.h>
#include <folly/io/IOBuf.h>
#include <folly/io/async/EventBase.h>
#include <gtest/gtest.h>

#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/fs/inodes/EdenDispatcherFactory.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/InodeMap.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/nfs/Nfsd3.h"
#include "eden/fs/nfs/NfsdRpc.h"
#include "eden/fs/nfs/rpc/Rpc.h"
#include "eden/fs/nfs/testharness/NfsRequestUtils.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/TestMount.h"

using namespace facebook::eden;

namespace {

constexpr auto kTimeout = std::chrono::seconds{10};

/**
 * Exercises the NFS invalidation pass of inode GC
 * (TreeInode::invalidateChildrenNotMaterializedNFS) against a real Nfsd3
 * attached to a TestMount.
 *
 * The Nfsd3 invalidates a directory by chmod'ing the directory's path under
 * the mount path, which here is a plain directory on local disk, so a
 * directory's invalidation succeeds only when the test created that directory
 * on disk first. The requests an NFS client would send are written to the
 * Nfsd3 over a socketpair.
 *
 * Every invalidation chmod is held at the nfsInvalidation fault, and the
 * fixture plays the kernel: for a directory that exists on disk it sends the
 * SETATTR the chmod would have turned into, then lets the chmod go. A test
 * that wants to send requests while a directory's chmod is pending holds
 * that directory and releases it itself.
 *
 * The mount path exists on disk, so the root's own chmod succeeds and the
 * root clears its children's references when pins are known; the walk
 * leaves the root's ".eden" directory alone.
 */
class NfsGcTest : public ::testing::Test {
 protected:
  void SetUp() override {
    builder_.setFile("parent/child/one.txt", "1\n");
    builder_.setFile("parent/child/two.txt", "2\n");
    builder_.setFile("parent/sibling.txt", "3\n");
    testMount_ = std::make_unique<TestMount>(builder_);
    configureMount();
    attachNfsChannel();
    faultInjector().injectBlock(kInvalidationFault, ".*");

    // Load every inode and give each an FS reference, as if the NFS client
    // had looked them all up.
    testMount_->getEdenMount()->getRootInode()->incFsRefcount();
    for (const char* dir : {"parent", "parent/child"}) {
      testMount_->getTreeInode(dir)->incFsRefcount();
    }
    for (const char* file :
         {"parent/child/one.txt",
          "parent/child/two.txt",
          "parent/sibling.txt"}) {
      testMount_->getFileInode(file)->incFsRefcount();
    }
  }

  void TearDown() override {
    // SetUp may have failed before attachNfsChannel() ran.
    if (clientFd_ != -1) {
      close(clientFd_);
    }
    if (stopFuture_.valid()) {
      // The Nfsd3 stops once it observes the socket's EOF, asynchronously.
      auto deadline = std::chrono::steady_clock::now() + kTimeout;
      while (!stopFuture_.isReady() &&
             std::chrono::steady_clock::now() < deadline) {
        manualExecutor_->run();
        evb_.loopOnce(EVLOOP_NONBLOCK);
      }
      EXPECT_TRUE(stopFuture_.isReady());
    }
    testMount_.reset();
    // Nfsd3 deletes itself on its EventBase.
    evb_.loopOnce(EVLOOP_NONBLOCK);
  }

  void attachNfsChannel() {
    auto& mount = *testMount_->getEdenMount();
    auto& serverState = *testMount_->getServerState();
    manualExecutor_ = std::make_shared<folly::ManualExecutor>();
    auto nfsd3 = std::unique_ptr<Nfsd3, FsChannelDeleter>(new Nfsd3(
        /*privHelper=*/nullptr,
        mount.getPath(),
        &evb_,
        manualExecutor_,
        EdenDispatcherFactory::makeNfsDispatcher(&mount),
        &mount.getStraceLogger(),
        serverState.getProcessInfoCache(),
        serverState.getEdenFsEventsLogger(),
        serverState.getErrorLogger(),
        /*requestTimeout=*/std::chrono::seconds{30},
        serverState.getNotifier(),
        CaseSensitivity::Sensitive,
        /*readIoSize=*/16 * 1024,
        /*writeIoSize=*/16 * 1024,
        /*maximumInFlightRequests=*/1000,
        /*highNfsRequestsLogInterval=*/std::chrono::minutes{10},
        /*longRunningFSRequestThreshold=*/std::chrono::nanoseconds{0},
        /*traceBusCapacity=*/1000,
        /*fastPathRPCs=*/false,
        serverState.getReloadableConfig(),
        serverState.getFaultInjector()));

    int fds[2];
    PCHECK(0 == socketpair(AF_UNIX, SOCK_STREAM, 0, fds));
    nfsd3->initialize(folly::File{fds[0], /*ownsFd=*/true});
    clientFd_ = fds[1];
    stopFuture_ = nfsd3->getStopFuture();
    mount.setTestFsChannel(std::move(nfsd3));
  }

  /**
   * Create the directory on local disk under the mount path so that the
   * chmod invalidating it succeeds.
   */
  void createOnDisk(folly::StringPiece dir) {
    ensureDirectoryExists(
        testMount_->getEdenMount()->getPath() + RelativePathPiece{dir});
  }

  using PinnedInodeSet = std::shared_ptr<const folly::F14FastSet<InodeNumber>>;

  /**
   * Pin information saying that nothing is pinned, as opposed to the null
   * set meaning pins are unknown.
   */
  static PinnedInodeSet noPins() {
    return std::make_shared<const folly::F14FastSet<InodeNumber>>();
  }

  /**
   * Start the invalidation pass of GC. finishGc() waits for it and returns
   * what it reports as the number of invalidated entries.
   */
  folly::Future<uint64_t> startGc(
      std::chrono::system_clock::time_point cutoff,
      PinnedInodeSet pinnedInodes = noPins(),
      folly::CancellationToken cancellationToken = {}) {
    // Bound to the server executor, which the wait helpers drain, so the
    // walk's deferred continuations run while a test waits on it; a
    // SemiFuture's would only run once something drove it.
    return testMount_->getEdenMount()
        ->getRootInode()
        ->handleChildrenNotAccessedRecently(
            cutoff,
            ObjectFetchContext::getNullContext(),
            /*pressureBased=*/true,
            std::move(cancellationToken),
            std::move(pinnedInodes))
        .semi()
        .via(testMount_->getServerExecutor().get());
  }

  /**
   * Drive the walk to completion, playing the kernel for every chmod that
   * is not held, and return what it reports.
   */
  uint64_t finishGc(folly::Future<uint64_t> gc) {
    auto deadline = std::chrono::steady_clock::now() + kTimeout;
    while (!gc.isReady()) {
      pump();
      emulateClient();
      if (std::chrono::steady_clock::now() > deadline) {
        ADD_FAILURE() << "GC did not finish";
        return 0;
      }
      gc.wait(std::chrono::milliseconds{1});
    }
    return std::move(gc).value();
  }

  uint64_t runGc(
      std::chrono::system_clock::time_point cutoff,
      PinnedInodeSet pinnedInodes = noPins()) {
    return finishGc(startGc(cutoff, std::move(pinnedInodes)));
  }

  static constexpr folly::StringPiece kInvalidationFault = "nfsInvalidation";

  FaultInjector& faultInjector() {
    return testMount_->getServerState()->getFaultInjector();
  }

  /**
   * Keep the fixture from playing the kernel for this directory's chmod: it
   * stays held until release(), so the test can send requests of its own
   * while it is pending.
   */
  void hold(std::string path) {
    held_.insert(std::move(path));
  }

  /**
   * Let the held chmod go, playing the kernel for it first: the SETATTR it
   * turns into, if the directory exists on disk, precedes its return. A
   * second SETATTR after one the test sent itself is answered normally and
   * changes nothing.
   */
  void release(const std::string& path) {
    held_.erase(path);
    emulateClientFor(path);
  }

  /**
   * Run the server's event loops and the mount's executor once.
   */
  void pump() {
    evb_.loopOnce(EVLOOP_NONBLOCK);
    manualExecutor_->run();
    testMount_->drainServerExecutor();
  }

  /**
   * Adjust the mount's configuration before the NFS channel reads it.
   */
  virtual void configureMount() {}

  /**
   * Wait, driving the mount's server executor, until GC has issued at least
   * the given number of chmods.
   */
  void waitForInvalidationAttempts(int64_t count) {
    auto deadline = std::chrono::steady_clock::now() + kTimeout;
    while (numInvalidationAttempts() < count) {
      pump();
      emulateClient();
      ASSERT_LT(std::chrono::steady_clock::now(), deadline)
          << "GC never issued " << count << " invalidations";
    }
  }

  /**
   * Play the kernel for every pending chmod that is not held: a directory
   * that exists on disk gets the SETATTR of its current mode, which GC
   * answers with a stale handle error and uses to forget its children; one
   * that does not exist gets nothing, and the chmod fails with ENOENT.
   */
  void emulateClient() {
    for (const auto& path :
         faultInjector().getBlockedFaults(kInvalidationFault)) {
      if (!held_.count(path)) {
        emulateClientFor(path);
      }
    }
  }

  void emulateClientFor(const std::string& path) {
    auto onDisk =
        testMount_->getEdenMount()->getPath() + RelativePathPiece{path};
    if (access(onDisk.c_str(), F_OK) == 0) {
      auto dir = testMount_->getTreeInode(path);
      setattrMode(dir->getNodeId(), dir->getMetadata().mode & 07777);
    }
    faultInjector().unblock(kInvalidationFault, path);
  }

  /**
   * Wait until GC has queued the chmod of this held directory and the worker
   * is holding it, playing the kernel for everything else meanwhile.
   */
  void waitUntilInvalidationBlocked(const std::string& path) {
    auto deadline = std::chrono::steady_clock::now() + kTimeout;
    while (true) {
      pump();
      emulateClient();
      auto blocked = faultInjector().getBlockedFaults(kInvalidationFault);
      if (std::find(blocked.begin(), blocked.end(), path) != blocked.end()) {
        return;
      }
      ASSERT_LT(std::chrono::steady_clock::now(), deadline)
          << "GC never reached the chmod of " << path;
      faultInjector().waitUntilBlocked(
          kInvalidationFault, std::chrono::milliseconds{1});
    }
  }

  /**
   * Run the unload sweep that follows the invalidation pass in GC.
   */
  size_t sweep() {
    return testMount_->getEdenMount()
        ->getRootInode()
        ->unloadChildrenUnreferencedByFs();
  }

  InodeNumber inodeNumberOf(folly::StringPiece path) {
    return testMount_->getInode(path)->getNodeId();
  }

  bool isLoaded(InodeNumber ino) {
    auto* inodeMap = testMount_->getEdenMount()->getInodeMap();
    return inodeMap->isInodeLoadedOrRemembered(ino) &&
        !inodeMap->isInodeRemembered(ino);
  }

  /**
   * Send one NFS request to the Nfsd3 over the socketpair and return its
   * reply, driving the server's executors and the mount's server executor
   * until the reply arrives.
   */
  std::unique_ptr<folly::IOBuf> sendAndReceive(
      std::unique_ptr<folly::IOBuf> request) {
    auto bytes = request->coalesce();
    while (!bytes.empty()) {
      auto written = write(clientFd_, bytes.data(), bytes.size());
      if (written <= 0) {
        ADD_FAILURE() << "writing the request failed";
        return folly::IOBuf::create(0);
      }
      bytes.advance(static_cast<size_t>(written));
    }

    std::vector<uint8_t> reply;
    size_t expectedSize = 0;
    auto deadline = std::chrono::steady_clock::now() + kTimeout;
    while (std::chrono::steady_clock::now() < deadline) {
      evb_.loopOnce(EVLOOP_NONBLOCK);
      manualExecutor_->run();
      testMount_->drainServerExecutor();

      struct pollfd pfd{};
      pfd.fd = clientFd_;
      pfd.events = POLLIN;
      if (poll(&pfd, 1, 10) <= 0) {
        continue;
      }
      uint8_t buf[64 * 1024];
      auto n = read(clientFd_, buf, sizeof(buf));
      if (n <= 0) {
        break;
      }
      reply.insert(reply.end(), buf, buf + n);
      if (expectedSize == 0 && reply.size() >= sizeof(uint32_t)) {
        // Record mark: high bit is the last-fragment flag, the rest the
        // fragment length.
        uint32_t mark = (uint32_t{reply[0]} << 24) |
            (uint32_t{reply[1]} << 16) | (uint32_t{reply[2]} << 8) |
            uint32_t{reply[3]};
        // Nfsd3 sends every reply as one fragment, which is all this reads.
        if (!(mark & 0x80000000)) {
          ADD_FAILURE() << "multi-fragment RPC reply";
          return folly::IOBuf::create(0);
        }
        expectedSize = sizeof(uint32_t) + (mark & 0x7fffffff);
      }
      if (expectedSize != 0 && reply.size() >= expectedSize) {
        break;
      }
    }
    if (expectedSize == 0 || reply.size() < expectedSize) {
      ADD_FAILURE() << "no complete RPC reply";
      return folly::IOBuf::create(0);
    }
    return folly::IOBuf::copyBuffer(reply.data(), reply.size());
  }

  /**
   * Parse an accepted RPC reply and deserialize the procedure's result.
   */
  template <typename Res>
  Res parseReply(std::unique_ptr<folly::IOBuf> reply) {
    if (reply->computeChainDataLength() == 0) {
      // sendAndReceive already recorded the failure.
      return Res{};
    }
    folly::io::Cursor cursor(reply.get());
    cursor.skip(sizeof(uint32_t)); // record mark
    auto msg = XdrTrait<rpc_msg_reply>::deserialize(cursor);
    if (msg.rbody.tag != reply_stat::MSG_ACCEPTED) {
      ADD_FAILURE() << "RPC reply rejected";
      return Res{};
    }
    if (std::get<accepted_reply>(msg.rbody.v).stat != accept_stat::SUCCESS) {
      ADD_FAILURE() << "RPC reply not successful";
      return Res{};
    }
    return XdrTrait<Res>::deserialize(cursor);
  }

  opaque_auth credentials() {
    return makeAuthSysCred(
        authsys_parms{/*stamp=*/0, "nfs-gc-test", getuid(), getgid(), {}});
  }

  /**
   * GETATTR the inode, as the NFS client does for a stat, returning the NFS
   * status of the reply.
   */
  nfsstat3 getattrStatus(InodeNumber ino) {
    auto res = parseReply<GETATTR3res>(sendAndReceive(buildNfsRequest(
        nextXid_++,
        nfsv3Procs::getattr,
        credentials(),
        GETATTR3args{nfs_fh3{ino}})));
    return res.tag;
  }

  /**
   * LOOKUP `name` in `dir`, as the NFS client does when a process walks into
   * the directory, returning the NFS status of the reply.
   */
  nfsstat3 lookupStatus(InodeNumber dir, std::string name) {
    auto res = parseReply<LOOKUP3res>(sendAndReceive(buildNfsRequest(
        nextXid_++,
        nfsv3Procs::lookup,
        credentials(),
        LOOKUP3args{diropargs3{nfs_fh3{dir}, std::move(name)}})));
    return res.tag;
  }

  /**
   * SETATTR the inode's permission bits, as the NFS client does for a chmod,
   * returning the NFS status of the reply.
   */
  nfsstat3 setattrMode(InodeNumber ino, mode_t mode) {
    SETATTR3args args{nfs_fh3{ino}, sattr3{}, sattrguard3{}};
    args.new_attributes.mode.tag = true;
    args.new_attributes.mode.v = uint32_t{mode};
    auto res = parseReply<SETATTR3res>(sendAndReceive(
        buildNfsRequest(nextXid_++, nfsv3Procs::setattr, credentials(), args)));
    return res.tag;
  }

  /**
   * Number of chmods GC has issued to invalidate directories so far.
   */
  int64_t numInvalidationAttempts() {
    testMount_->getServerState()->getStats()->flush();
    facebook::fb303::ThreadCachedServiceData::get()->publishStats();
    return facebook::fb303::ServiceData::get()
        ->getCounterIfExists("nfs.invalidation.gc.attempt.sum")
        .value_or(0);
  }

  FakeTreeBuilder builder_;
  folly::EventBase evb_;
  std::shared_ptr<folly::ManualExecutor> manualExecutor_;
  std::unique_ptr<TestMount> testMount_;
  folly::SemiFuture<FsStopDataPtr> stopFuture_{
      folly::SemiFuture<FsStopDataPtr>::makeEmpty()};
  int clientFd_{-1};
  uint32_t nextXid_{1};
  std::set<std::string> held_;
};

/**
 * The same tree with two more directories under "parent", so that several
 * directories' invalidations can be queued at the same time.
 */
class NfsGcSiblingsTest : public NfsGcTest {
 protected:
  void SetUp() override {
    builder_.setFile("parent/second/four.txt", "4\n");
    builder_.setFile("parent/third/five.txt", "5\n");
    NfsGcTest::SetUp();
    for (const char* dir : {"parent/second", "parent/third"}) {
      testMount_->getTreeInode(dir)->incFsRefcount();
    }
    for (const char* file :
         {"parent/second/four.txt", "parent/third/five.txt"}) {
      testMount_->getFileInode(file)->incFsRefcount();
    }
  }
};

} // namespace

TEST_F(NfsGcTest, failedInvalidationIsNotCountedAsProgress) {
  auto one = inodeNumberOf("parent/child/one.txt");
  auto two = inodeNumberOf("parent/child/two.txt");
  auto sibling = inodeNumberOf("parent/sibling.txt");

  // Neither "parent" nor "parent/child" exists on disk, so the chmod that
  // invalidates each of them fails with ENOENT and the callback that clears
  // their children's FS references never runs.
  auto numInvalidated = runGc(std::chrono::system_clock::time_point::max());
  sweep();

  // Neither chmod succeeded, so nothing had its FS reference cleared and GC
  // must report no progress.
  EXPECT_EQ(0, numInvalidated);
  EXPECT_TRUE(isLoaded(one));
  EXPECT_TRUE(isLoaded(two));
  EXPECT_TRUE(isLoaded(sibling));
}

TEST_F(NfsGcTest, directoriesStayReferencedWithoutPinInformation) {
  createOnDisk("parent/child");
  auto child = inodeNumberOf("parent/child");
  auto one = inodeNumberOf("parent/child/one.txt");
  auto sibling = inodeNumberOf("parent/sibling.txt");

  // Without pin information, a directory might be some process's working
  // directory, so only the files are cleared: "parent/child" keeps its FS
  // reference even though "parent" was invalidated.
  EXPECT_EQ(
      3,
      runGc(
          std::chrono::system_clock::time_point::max(),
          /*pinnedInodes=*/nullptr));
  sweep();
  EXPECT_TRUE(isLoaded(child));
  EXPECT_FALSE(isLoaded(one));
  EXPECT_FALSE(isLoaded(sibling));

  // Knowing that nothing is pinned, the next run clears the directories too:
  // "parent" clears "child", and the root clears "parent".
  auto parent = inodeNumberOf("parent");
  EXPECT_EQ(2, runGc(std::chrono::system_clock::time_point::max(), noPins()));
  sweep();
  EXPECT_FALSE(isLoaded(child));
  EXPECT_FALSE(isLoaded(parent));
}

TEST_F(NfsGcTest, parentIsInvalidatedAfterItsChildWasInvalidated) {
  createOnDisk("parent/child");
  auto parent = inodeNumberOf("parent");
  auto child = inodeNumberOf("parent/child");
  auto one = inodeNumberOf("parent/child/one.txt");
  auto sibling = inodeNumberOf("parent/sibling.txt");
  auto childMode =
      testMount_->getTreeInode("parent/child")->getMetadata().mode & 07777;

  // Everything was last used at the clock's initial time. Move the clock
  // forward so all of it is older than the cutoff.
  auto& clock = testMount_->getClock();
  clock.advance(std::chrono::hours{2});
  auto cutoff = clock.getTimePoint() - std::chrono::hours{1};

  // The chmod that invalidates "parent/child" reaches EdenFS as the requests
  // the macOS kernel makes to resolve and authorize it: a GETATTR of the
  // directory when the client's attribute cache for it has expired, a LOOKUP
  // of the AppleDouble name "._child" in the parent, and the SETATTR of the
  // directory's current mode, which GC answers with a stale handle error.
  // Send them while the chmod is held.
  hold("parent/child");
  auto gc = startGc(cutoff);
  waitUntilInvalidationBlocked("parent/child");
  clock.advance(std::chrono::minutes{1});
  EXPECT_EQ(nfsstat3::NFS3_OK, getattrStatus(child));
  EXPECT_EQ(nfsstat3::NFS3ERR_NOENT, lookupStatus(parent, "._child"));
  EXPECT_EQ(nfsstat3::NFS3ERR_STALE, setattrMode(child, childMode));
  release("parent/child");
  auto numInvalidated = finishGc(std::move(gc));
  sweep();

  // The requests behind GC's own chmod must not make "parent" consider
  // itself or its child recently used: the child's invalidation cleared its
  // two files, the parent's cleared the child and its sibling file, and the
  // root's cleared the parent.
  EXPECT_EQ(5, numInvalidated);
  EXPECT_FALSE(isLoaded(one));
  EXPECT_FALSE(isLoaded(child));
  EXPECT_FALSE(isLoaded(sibling));
}

TEST_F(NfsGcTest, pinnedInodesAndTheirAncestorsKeepTheirReferences) {
  createOnDisk("parent/child");
  auto child = inodeNumberOf("parent/child");
  auto one = inodeNumberOf("parent/child/one.txt");
  auto two = inodeNumberOf("parent/child/two.txt");
  auto sibling = inodeNumberOf("parent/sibling.txt");

  // Some process holds "one.txt" and "sibling.txt" open.
  auto pins = std::make_shared<const folly::F14FastSet<InodeNumber>>(
      folly::F14FastSet<InodeNumber>{one, sibling});
  auto numInvalidated =
      runGc(std::chrono::system_clock::time_point::max(), pins);
  sweep();

  // The pinned files keep their references, and so does "parent/child"
  // because its subtree contains a pin. Only "two.txt" is reclaimed.
  EXPECT_EQ(1, numInvalidated);
  EXPECT_TRUE(isLoaded(one));
  EXPECT_TRUE(isLoaded(sibling));
  EXPECT_TRUE(isLoaded(child));
  EXPECT_NE(0, testMount_->getTreeInode("parent/child")->debugGetFsRefcount());
  EXPECT_FALSE(isLoaded(two));
}

TEST_F(NfsGcTest, pinnedDirectoryStillHasItsChildrenReclaimed) {
  createOnDisk("parent/child");
  auto child = inodeNumberOf("parent/child");
  auto one = inodeNumberOf("parent/child/one.txt");
  auto sibling = inodeNumberOf("parent/sibling.txt");

  // Some process has "parent/child" as its working directory.
  auto pins = std::make_shared<const folly::F14FastSet<InodeNumber>>(
      folly::F14FastSet<InodeNumber>{child});
  auto numInvalidated =
      runGc(std::chrono::system_clock::time_point::max(), pins);
  sweep();

  // The working directory keeps its reference, so the process keeps a valid
  // handle, while its children and its sibling are reclaimed: relative
  // lookups through the directory reload them by name.
  EXPECT_EQ(3, numInvalidated);
  EXPECT_TRUE(isLoaded(child));
  EXPECT_FALSE(isLoaded(one));
  EXPECT_FALSE(isLoaded(sibling));
}

TEST_F(NfsGcTest, lookupAfterTheForgetReferencesTheChildAnew) {
  createOnDisk("parent/child");
  auto child = inodeNumberOf("parent/child");
  auto one = inodeNumberOf("parent/child/one.txt");
  auto two = inodeNumberOf("parent/child/two.txt");
  auto childMode =
      testMount_->getTreeInode("parent/child")->getMetadata().mode & 07777;

  // GC's chmod of "parent/child" reaches EdenFS as a SETATTR. The stale
  // reply is the moment the client forgets the directory's names and EdenFS
  // clears its children's references, like a FUSE FORGET. A process that
  // then looks "two.txt" up references it anew, like a FUSE lookup would; a
  // GETATTR with a handle the client already has hands out nothing new.
  hold("parent/child");
  auto gc = startGc(std::chrono::system_clock::time_point::max());
  waitUntilInvalidationBlocked("parent/child");
  EXPECT_EQ(nfsstat3::NFS3ERR_STALE, setattrMode(child, childMode));
  EXPECT_EQ(nfsstat3::NFS3_OK, lookupStatus(child, "two.txt"));
  EXPECT_EQ(nfsstat3::NFS3_OK, getattrStatus(one));
  release("parent/child");
  auto numInvalidated = finishGc(std::move(gc));
  sweep();

  // Both files were cleared, the parent cleared the child directory and the
  // sibling file, and the root cleared the parent; only "two.txt" is
  // referenced again and stays.
  EXPECT_EQ(5, numInvalidated);
  EXPECT_TRUE(isLoaded(two));
  EXPECT_FALSE(isLoaded(one));
}

TEST_F(NfsGcTest, withoutTheStaleReplyChildrenAreForgottenAfterTheChmod) {
  testMount_->updateEdenConfig({{"experimental:nfs-gc-stale-reply", "false"}});
  createOnDisk("parent/child");
  auto child = inodeNumberOf("parent/child");
  auto one = testMount_->getFileInode("parent/child/one.txt");
  auto sibling = inodeNumberOf("parent/sibling.txt");
  auto childMode =
      testMount_->getTreeInode("parent/child")->getMetadata().mode & 07777;

  // With the fallback on, GC's SETATTR is answered normally and clears
  // nothing; the children are forgotten once the chmod has succeeded, as
  // GC did before the stale reply.
  hold("parent/child");
  auto gc = startGc(std::chrono::system_clock::time_point::max());
  waitUntilInvalidationBlocked("parent/child");
  EXPECT_EQ(nfsstat3::NFS3_OK, setattrMode(child, childMode));
  EXPECT_NE(0, one->debugGetFsRefcount());
  release("parent/child");
  auto numInvalidated = finishGc(std::move(gc));
  EXPECT_EQ(0, one->debugGetFsRefcount());
  one.reset();
  sweep();

  EXPECT_EQ(5, numInvalidated);
  EXPECT_FALSE(isLoaded(child));
  EXPECT_FALSE(isLoaded(sibling));
}

TEST_F(NfsGcTest, lookupBeforeTheForgetIsForgottenWithTheRest) {
  createOnDisk("parent/child");
  auto parent = inodeNumberOf("parent");
  auto child = inodeNumberOf("parent/child");
  auto sibling = inodeNumberOf("parent/sibling.txt");
  auto parentMode =
      testMount_->getTreeInode("parent")->getMetadata().mode & 07777;

  // A process looks "child" up in "parent" and fetches its attributes after
  // GC decided on "parent" but before its chmod reaches EdenFS. The stale
  // reply then makes the client drop the names it cached for "parent",
  // including that one, so the child is cleared with the rest.
  hold("parent");
  auto gc = startGc(std::chrono::system_clock::time_point::max());
  waitUntilInvalidationBlocked("parent");
  EXPECT_EQ(nfsstat3::NFS3_OK, lookupStatus(parent, "child"));
  EXPECT_EQ(nfsstat3::NFS3_OK, getattrStatus(child));
  EXPECT_EQ(nfsstat3::NFS3ERR_STALE, setattrMode(parent, parentMode));
  release("parent");
  auto numInvalidated = finishGc(std::move(gc));
  sweep();

  EXPECT_EQ(5, numInvalidated);
  EXPECT_FALSE(isLoaded(child));
  EXPECT_FALSE(isLoaded(sibling));
}

TEST_F(NfsGcTest, cancellationWaitsForTheChmodItAlreadyQueued) {
  createOnDisk("parent/child");
  hold("parent/child");
  folly::CancellationSource cancellation;
  auto gc = startGc(
      std::chrono::system_clock::time_point::max(),
      noPins(),
      cancellation.getToken());
  waitUntilInvalidationBlocked("parent/child");

  // The chmod of "parent/child" is in flight. Cancelling the walk must not
  // let it finish before that chmod has: its forget callback holds inode
  // references, and the walk's completion releases the GC lease.
  cancellation.requestCancellation();
  pump();
  EXPECT_FALSE(gc.isReady());
  release("parent/child");
  finishGc(std::move(gc));
  EXPECT_TRUE(faultInjector().getBlockedFaults(kInvalidationFault).empty());
}

TEST_F(NfsGcTest, directoryWithNothingToClearIsNotInvalidatedAgain) {
  createOnDisk("parent/child");
  // Some process has "parent/child" as its working directory, so it keeps
  // its FS reference and stays loaded across GC runs.
  auto child = inodeNumberOf("parent/child");
  auto pins = std::make_shared<const folly::F14FastSet<InodeNumber>>(
      folly::F14FastSet<InodeNumber>{child});
  auto one = inodeNumberOf("parent/child/one.txt");
  auto two = inodeNumberOf("parent/child/two.txt");

  // The first run clears the files under "parent/child" and next to it,
  // and the sweep forgets them.
  EXPECT_EQ(3, runGc(std::chrono::system_clock::time_point::max(), pins));
  sweep();
  EXPECT_FALSE(isLoaded(one));
  EXPECT_FALSE(isLoaded(two));
  EXPECT_TRUE(isLoaded(child));

  // A second run has no FS reference left to clear under "parent/child".
  auto attemptsBefore = numInvalidationAttempts();
  EXPECT_EQ(0, runGc(std::chrono::system_clock::time_point::max(), pins));
  // GC must not chmod "parent/child" again.
  EXPECT_EQ(attemptsBefore, numInvalidationAttempts());
}

TEST_F(NfsGcTest, materializedDirectoriesAreReclaimedToo) {
  createOnDisk("parent/child");
  // Writing a file materializes "parent", whose state is then in the
  // overlay. That is no reason to leave it, or anything under it, loaded.
  testMount_->addFile("parent/untracked.txt", "u\n");
  auto untracked = inodeNumberOf("parent/untracked.txt");
  testMount_->getFileInode("parent/untracked.txt")->incFsRefcount();
  auto child = inodeNumberOf("parent/child");
  auto one = inodeNumberOf("parent/child/one.txt");
  auto sibling = inodeNumberOf("parent/sibling.txt");

  // "parent/child" clears its two files; "parent" clears the child
  // directory, the sibling and the untracked file; the root clears "parent".
  auto parent = inodeNumberOf("parent");
  EXPECT_EQ(6, runGc(std::chrono::system_clock::time_point::max()));
  sweep();
  EXPECT_FALSE(isLoaded(one));
  EXPECT_FALSE(isLoaded(child));
  EXPECT_FALSE(isLoaded(parent));
  EXPECT_FALSE(isLoaded(sibling));
  EXPECT_FALSE(isLoaded(untracked));

  // The unloaded untracked file is still there, from the overlay.
  EXPECT_EQ("u\n", testMount_->readFile("parent/untracked.txt"));
}

TEST_F(
    NfsGcTest,
    directoryWhoseOnlyReferencedChildrenAreDirectoriesIsLeftAlone) {
  createOnDisk("parent/child");
  auto child = inodeNumberOf("parent/child");

  // Without pin information the first run clears the three files but leaves
  // "parent/child" referenced.
  EXPECT_EQ(3, runGc(std::chrono::system_clock::time_point::max(), nullptr));
  sweep();
  ASSERT_TRUE(isLoaded(child));

  // "parent" now has nothing this run may clear, since its only referenced
  // child is a directory, so it is not chmod'ed either.
  auto attemptsBefore = numInvalidationAttempts();
  EXPECT_EQ(0, runGc(std::chrono::system_clock::time_point::max(), nullptr));
  EXPECT_EQ(attemptsBefore, numInvalidationAttempts());
}

TEST_F(NfsGcTest, rememberedFilesAreReclaimedBeforeTheSweep) {
  createOnDisk("parent/child");
  auto child = testMount_->getTreeInode("parent/child");
  auto one = inodeNumberOf("parent/child/one.txt");
  auto* inodeMap = testMount_->getEdenMount()->getInodeMap();

  // The files under "parent/child" are unloaded but remembered, since the
  // client still references them. Clearing such a reference forgets the
  // inode right away, before the sweep, so the sweep never sees it.
  child->unloadChildrenNow();
  ASSERT_TRUE(inodeMap->isInodeRemembered(one));
  auto before = inodeMap->getInodeCounts().forgottenInodeCount;

  EXPECT_EQ(5, runGc(std::chrono::system_clock::time_point::max()));
  EXPECT_EQ(before + 2, inodeMap->getInodeCounts().forgottenInodeCount);
  EXPECT_FALSE(inodeMap->isInodeLoadedOrRemembered(one));
}

/**
 * Siblings, with three invalidation threads.
 */
class NfsGcParallelTest : public NfsGcSiblingsTest {
 protected:
  void configureMount() override {
    testMount_->updateEdenConfig({{"nfs:num-invalidation-threads", "3"}});
  }
};

TEST_F(NfsGcParallelTest, siblingsAreInvalidatedConcurrently) {
  for (const char* dir : {"parent/child", "parent/second", "parent/third"}) {
    createOnDisk(dir);
  }
  auto five = inodeNumberOf("parent/third/five.txt");

  // Hold the first sibling's chmod. The other two siblings do not wait for
  // it: their chmods run on the other threads and complete. "parent" waits
  // for all three, and the root for "parent".
  ASSERT_EQ(
      3u,
      testMount_->getEdenMount()->getNfsdChannel()->numInvalidationThreads());
  hold("parent/child");
  auto attemptsBefore = numInvalidationAttempts();
  auto gc = startGc(std::chrono::system_clock::time_point::max());
  waitUntilInvalidationBlocked("parent/child");
  waitForInvalidationAttempts(attemptsBefore + 3);
  EXPECT_EQ(attemptsBefore + 3, numInvalidationAttempts());
  EXPECT_TRUE(isLoaded(inodeNumberOf("parent")));

  release("parent/child");
  auto numInvalidated = finishGc(std::move(gc));
  sweep();

  EXPECT_EQ(attemptsBefore + 5, numInvalidationAttempts());
  EXPECT_EQ(9, numInvalidated);
  EXPECT_FALSE(isLoaded(five));
}

TEST_F(NfsGcSiblingsTest, queuedInvalidationsAreCapped) {
  testMount_->updateEdenConfig({{"nfs:max-queued-gc-invalidations", "1"}});
  for (const char* dir : {"parent/child", "parent/second", "parent/third"}) {
    createOnDisk(dir);
  }
  auto five = inodeNumberOf("parent/third/five.txt");

  // Hold the first sibling's chmod. The single invalidation worker takes what
  // is queued in one batch and is now stuck on it, so entries queued after
  // that stay in the queue, and the walk goes on to queue the siblings'
  // chmods, since each directory waits only for its own. "parent/child" is
  // walked first because entries are visited in name order.
  hold("parent/child");
  auto attemptsBefore = numInvalidationAttempts();
  auto gc = startGc(std::chrono::system_clock::time_point::max());
  waitUntilInvalidationBlocked("parent/child");

  // With a cap of one, at most one sibling's chmod is queued behind the one
  // the worker holds, and only chmods the worker has started count as
  // attempts; without the cap all three would be queued at once.
  EXPECT_GE(numInvalidationAttempts(), attemptsBefore + 1);
  EXPECT_LE(numInvalidationAttempts(), attemptsBefore + 2);

  release("parent/child");
  auto numInvalidated = finishGc(std::move(gc));
  sweep();

  // Once the queue drained the walk went on: the remaining siblings, then
  // "parent", then the root were invalidated, clearing everything.
  EXPECT_EQ(attemptsBefore + 5, numInvalidationAttempts());
  EXPECT_EQ(9, numInvalidated);
  EXPECT_FALSE(isLoaded(five));
}

#endif
