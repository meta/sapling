/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/InodeMap.h"

#include <folly/Portability.h>
#include <folly/String.h>
#include <folly/test/TestUtils.h>
#include <gtest/gtest.h>
#include <thread>

#include "eden/common/utils/Bug.h"
#include "eden/common/utils/FaultInjector.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/store/StatsFetchContext.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/telemetry/test/CapturingXplatLogger.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/TestMount.h"
#include "eden/fs/testharness/TestUtil.h"

using namespace std::chrono_literals;
using namespace facebook::eden;

constexpr auto loadTimeoutLimit = 1000ms;
// we will wait up to loadTimeoutLimit, maxWaitForLoads times
// for EdenFS to finish loading all it's initial inodes.
constexpr auto maxWaitForLoads = 60;

TEST(InodeMap, invalidInodeNumber) {
  FakeTreeBuilder builder;
  builder.setFile("Makefile", "all:\necho success\n");
  builder.setFile("src/noop.c", "int main() { return 0; }\n");
  TestMount testMount{builder};

  EdenBugDisabler noCrash;
  auto* inodeMap = testMount.getEdenMount()->getInodeMap();
  auto future = inodeMap->lookupFileInode(
      0x12345678_ino, ObjectFetchContext::getNullContext());
  EXPECT_THROW_RE(
      std::move(future).get(), std::runtime_error, "unknown inode number");
}

TEST(InodeMap, forgetOnUnknownInodeFromUserspaceFsPeer) {
  // NFS and FSKit peers are userspace processes whose lookup-reference
  // accounting can race the wire contract in ways a kernel client cannot;
  // like lookups of unknown inode numbers (throwEstaleIfInodeIsMissing),
  // their forgets must never abort the daemon.
  TestMount testMount;
  testMount.setMountProtocol(MountProtocol::NFS);
  FakeTreeBuilder builder;
  builder.setFile("file.txt", "contents\n");
  testMount.initialize(builder);

  // Tolerated: logged and counted, no abort. The mount keeps working.
  auto* inodeMap = testMount.getEdenMount()->getInodeMap();
  inodeMap->decFsRefcount(0x12345678_ino, 1);
  auto root = testMount.getEdenMount()->getRootInode();
  EXPECT_NE(nullptr, root.get());
}

TEST(InodeMap, loadFaultFailsAllPendingLookupsAndAllowsRetry) {
  FakeTreeBuilder builder;
  builder.setFile("file.txt", "contents");
  TestMount mount{builder, true, false};
  auto inode = mount.getFileInode("file.txt"_relpath);
  auto number = inode->getNodeId();
  inode->incFsRefcount();
  auto* inodeMap = mount.getEdenMount()->getInodeMap();
  auto& injector = mount.getServerState()->getFaultInjector();
  const auto key = fmt::format("{}", number);
  injector.injectError("inodemap-load", key, std::runtime_error("loaded"));
  EXPECT_EQ(
      inode,
      inodeMap->lookupFileInode(number, ObjectFetchContext::getNullContext())
          .get(0ms));
  injector.removeFault("inodemap-load", key);
  inode.reset();
  mount.getEdenMount()->getRootInode()->unloadChildrenNow();
  ASSERT_FALSE(inodeMap->lookupLoadedInode(number));
  injector.injectError("inodemap-load", "0", std::runtime_error("wrong key"));
  EXPECT_EQ(
      number,
      inodeMap->lookupInode(number, ObjectFetchContext::getNullContext())
          .get(0ms)
          ->getNodeId());
  injector.removeFault("inodemap-load", "0");
  mount.getEdenMount()->getRootInode()->unloadChildrenNow();
  ASSERT_FALSE(inodeMap->lookupLoadedInode(number));
  injector.injectBlock("inodemap-load", key, 1);
  std::optional<std::string> firstError;
  std::thread request([&] {
    try {
      inodeMap->lookupInode(number, ObjectFetchContext::getNullContext())
          .get(5s);
    } catch (const std::exception& ex) {
      firstError = ex.what();
    }
  });
  const auto reached = injector.waitUntilBlocked("inodemap-load", 5s);
  const auto blocked = injector.getBlockedFaults("inodemap-load");
  auto second =
      inodeMap->lookupInode(number, ObjectFetchContext::getNullContext());
  injector.removeFault("inodemap-load", key);
  const auto unblocked = injector.unblockWithError(
      "inodemap-load", key, std::runtime_error("inode load blocked"));
  request.join();
  EXPECT_TRUE(reached);
  EXPECT_EQ(std::vector<std::string>{key}, blocked);
  EXPECT_EQ(1, unblocked);
  EXPECT_EQ(std::optional<std::string>{"inode load blocked"}, firstError);
  EXPECT_THROW_RE(
      std::move(second).get(5s), std::runtime_error, "inode load blocked");
  EXPECT_EQ(
      number,
      inodeMap->lookupInode(number, ObjectFetchContext::getNullContext())
          .get(0ms)
          ->getNodeId());
}

TEST(InodeMap, rejectsConflictingConcurrentChildLoad) {
  FakeTreeBuilder builder;
  TestMount testMount{builder};
  auto* inodeMap = testMount.getEdenMount()->getInodeMap();
  auto parent = testMount.getEdenMount()->getRootInode();
  const InodeNumber child{1234};

  folly::Promise<InodePtr> firstPromise;
  auto firstFuture = firstPromise.getSemiFuture();
  EXPECT_TRUE(inodeMap->startLoadingChildIfNotLoading(
      parent.get(),
      "first"_pc,
      child,
      S_IFREG | 0644,
      std::move(firstPromise)));

  folly::Promise<InodePtr> conflictingPromise;
  auto conflictingFuture = conflictingPromise.getSemiFuture();
  EXPECT_FALSE(inodeMap->startLoadingChildIfNotLoading(
      parent.get(),
      "second"_pc,
      child,
      S_IFREG | 0644,
      std::move(conflictingPromise)));
  EXPECT_THROW_RE(
      std::move(conflictingFuture).get(),
      std::system_error,
      "conflicting inode reference to inode 1234");

  inodeMap->inodeLoadFailed(
      child,
      folly::make_exception_wrapper<std::runtime_error>("stop test load"));
  EXPECT_THROW(std::move(firstFuture).get(), std::runtime_error);
}

#ifdef __linux__
TEST(InodeMap, forgetMultipleReferencesFromLoadedInode) {
  FakeTreeBuilder builder;
  builder.setFile("file.txt", "contents\n");
  TestMount testMount{builder};

  auto file = testMount.getFileInode("file.txt"_relpath);
  file->incFsRefcount(3);
  testMount.getEdenMount()->getInodeMap()->decFsRefcount(file->getNodeId(), 3);

  EXPECT_EQ(0, file->debugGetFsRefcount());
}
#endif

TEST(InodeMap, decFsRefcountOnRememberedInodeWithoutReference) {
  if (folly::kIsLinux) {
    GTEST_SKIP() << "FUSE reference counts are exact";
  }
  auto builder = FakeTreeBuilder();
  builder.setFile("src/test.txt", "this is a test file");
  TestMount testMount{builder, false};

  // Leave the load of "src" pending so the InodeMap remembers the inode
  // without an FS reference.
  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto srcFuture =
      rootInode->getOrLoadChild("src"_pc, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  ASSERT_FALSE(srcFuture.isReady());

  auto* inodeMap = testMount.getEdenMount()->getInodeMap();
  const auto srcIno = rootInode->getChildInodeNumber("src"_pc);
  ASSERT_TRUE(inodeMap->isInodeRemembered(srcIno));

  // A PrjFS invalidation of an entry without an on-disk placeholder reaches
  // this call while the load is in flight.
  inodeMap->decFsRefcount(srcIno);
  EXPECT_TRUE(inodeMap->isInodeRemembered(srcIno));

  builder.setReady("src");
  testMount.drainServerExecutor();
  ASSERT_TRUE(srcFuture.isReady());
  auto srcTree = std::move(srcFuture).get(std::chrono::seconds(1));
  EXPECT_EQ(srcIno, srcTree->getNodeId());
}

TEST(InodeMap, simpleLookups) {
  // Test simple lookups that succeed immediately from the LocalStore
  FakeTreeBuilder builder;
  builder.setFile("Makefile", "all:\necho success\n");
  builder.setFile("src/noop.c", "int main() { return 0; }\n");
  TestMount testMount{builder};
  auto* inodeMap = testMount.getEdenMount()->getInodeMap();

  // Look up the tree inode by name first
  auto root = testMount.getEdenMount()->getRootInode();
  auto srcTreeFut =
      root->getOrLoadChild("src"_pc, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  auto srcTree = std::move(srcTreeFut).get(0ms);

  // Next look up the tree by inode number
  auto tree2Fut =
      inodeMap
          ->lookupTreeInode(
              srcTree->getNodeId(), ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  auto tree2 = std::move(tree2Fut).get(0ms);
  EXPECT_EQ(srcTree, tree2);
  EXPECT_EQ(RelativePath{"src"}, tree2->getPath());

  // Next look up src/noop.c by name
  auto noopFut =
      tree2->getOrLoadChild("noop.c"_pc, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  auto noop = std::move(noopFut).get(0ms);
  EXPECT_NE(srcTree->getNodeId(), noop->getNodeId());

  // And look up src/noop.c by inode ID
  auto noop2Fut =
      inodeMap
          ->lookupFileInode(
              noop->getNodeId(), ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  auto noop2 = std::move(noop2Fut).get(0ms);
  EXPECT_EQ(noop, noop2);
  EXPECT_EQ(RelativePath{"src/noop.c"}, noop2->getPath());

  // lookupTreeInode(, ObjectFetchContext::getNullContext()) and
  // lookupFileInode(, ObjectFetchContext::getNullContext()) should fail when
  // called on the wrong file type.
  EXPECT_THROW_ERRNO(
      inodeMap
          ->lookupFileInode(
              srcTree->getNodeId(), ObjectFetchContext::getNullContext())
          .get(),
      EISDIR);
  EXPECT_THROW_ERRNO(
      inodeMap
          ->lookupTreeInode(
              noop->getNodeId(), ObjectFetchContext::getNullContext())
          .get(),
      ENOTDIR);
}

TEST(InodeMap, asyncLookup) {
  auto builder = FakeTreeBuilder();
  builder.setFile("README", "docs go here\n");
  builder.setFile("src/runme.sh", "#!/bin/sh\necho hello world\n", true);
  builder.setFile("src/test.txt", "this is a test file");
  TestMount testMount{builder, false};

  // Look up the "src" tree inode by name
  // The future should only be fulfilled when after we make the tree ready
  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto srcFuture =
      rootInode->getOrLoadChild("src"_pc, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  EXPECT_FALSE(srcFuture.isReady());

  // Start a second lookup before the first is ready
  auto srcFuture2 =
      rootInode->getOrLoadChild("src"_pc, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  EXPECT_FALSE(srcFuture2.isReady());

  // Now make the tree ready
  builder.setReady("src");
  testMount.drainServerExecutor();
  ASSERT_TRUE(srcFuture.isReady());
  ASSERT_TRUE(srcFuture2.isReady());
  auto srcTree = std::move(srcFuture).get(std::chrono::seconds(1));
  auto srcTree2 = std::move(srcFuture2).get(std::chrono::seconds(1));
  EXPECT_EQ(srcTree.get(), srcTree2.get());
}

TEST(InodeMap, asyncError) {
  auto builder = FakeTreeBuilder();
  builder.setFile("README", "docs go here\n");
  builder.setFile("src/runme.sh", "#!/bin/sh\necho hello world\n", true);
  builder.setFile("src/test.txt", "this is a test file");
  TestMount testMount{builder, false};

  // Look up the "src" tree inode by name
  // The future should only be fulfilled when after we make the tree ready
  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto srcFuture =
      rootInode->getOrLoadChild("src"_pc, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  EXPECT_FALSE(srcFuture.isReady());

  // Start a second lookup before the first is ready
  auto srcFuture2 =
      rootInode->getOrLoadChild("src"_pc, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  EXPECT_FALSE(srcFuture2.isReady());

  // Now fail the tree lookup
  builder.triggerError(
      "src", std::domain_error("rejecting lookup for src tree"));
  testMount.drainServerExecutor();
  ASSERT_TRUE(srcFuture.isReady());
  ASSERT_TRUE(srcFuture2.isReady());
  EXPECT_THROW(std::move(srcFuture).get(), std::domain_error);
  EXPECT_THROW(std::move(srcFuture2).get(), std::domain_error);
}

TEST(InodeMap, recursiveLookup) {
  auto builder = FakeTreeBuilder();
  builder.setFile("a/b/c/d/file.txt", "this is a test file");
  TestMount testMount{builder, false};
  const auto& edenMount = testMount.getEdenMount();

  // Call EdenMount::getInodeSlow() to do a recursive lookup
  auto fileFuture =
      edenMount
          ->getInodeSlow(
              "a/b/c/d/file.txt"_relpath, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());

  builder.setReady("a/b/c");
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());
  builder.setReady("a");
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());
  builder.setReady("a/b");
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());
  builder.setReady("a/b/c/d/file.txt");
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());
  builder.setReady("a/b/c/d");
  testMount.drainServerExecutor();
  ASSERT_TRUE(fileFuture.isReady());
  auto fileInode = std::move(fileFuture).get();
  EXPECT_EQ("a/b/c/d/file.txt"_relpath, fileInode->getPath().value());
}

TEST(InodeMap, recursiveLookupError) {
  auto builder = FakeTreeBuilder();
  builder.setFile("a/b/c/d/file.txt", "this is a test file");
  TestMount testMount{builder, false};
  const auto& edenMount = testMount.getEdenMount();

  // Call EdenMount::getInodeSlow() to do a recursive lookup
  auto fileFuture =
      edenMount
          ->getInodeSlow(
              "a/b/c/d/file.txt"_relpath, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());

  builder.setReady("a");
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());
  builder.setReady("a/b/c");
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());
  builder.setReady("a/b");
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());
  builder.setReady("a/b/c/d/file.txt");
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());
  builder.triggerError(
      "a/b/c/d", std::domain_error("error for testing purposes"));
  testMount.drainServerExecutor();
  ASSERT_TRUE(fileFuture.isReady());
  EXPECT_THROW_RE(
      std::move(fileFuture).get(),
      std::domain_error,
      "error for testing purposes");
}

TEST(InodeMap, renameDuringRecursiveLookup) {
  auto builder = FakeTreeBuilder();
  builder.setFile("a/b/c/d/file.txt", "this is a test file");
  TestMount testMount{builder, false};
  const auto& edenMount = testMount.getEdenMount();

  // Call EdenMount::getInodeSlow() to do a recursive lookup
  auto fileFuture =
      edenMount
          ->getInodeSlow(
              "a/b/c/d/file.txt"_relpath, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());

  builder.setReady("a/b/c");
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());
  builder.setReady("a");
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());
  builder.setReady("a/b");
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());

  auto bInode = testMount.getTreeInode("a/b"_relpath);

  // Rename c to x after the recursive resolution should have
  // already looked it up
  auto renameFuture = bInode
                          ->rename(
                              "c"_pc,
                              bInode,
                              "x"_pc,
                              InvalidationRequired::No,
                              ObjectFetchContext::getNullContext())
                          .semi()
                          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  ASSERT_TRUE(renameFuture.isReady());
  EXPECT_FALSE(fileFuture.isReady());

  // Now mark the rest of the tree ready
  // Note that we don't actually have to mark the file itself ready.
  // The Inode lookup itself doesn't need the blob data yet.
  builder.setReady("a/b/c/d");
  testMount.drainServerExecutor();
  ASSERT_TRUE(fileFuture.isReady());
  auto fileInode = std::move(fileFuture).get();
  // We should have successfully looked up the inode, but it will report it
  // self (correctly) at its new path now.
  EXPECT_EQ("a/b/x/d/file.txt"_relpath, fileInode->getPath().value());
}

TEST(InodeMap, renameDuringRecursiveLookupAndLoad) {
  auto builder = FakeTreeBuilder();
  builder.setFile("a/b/c/d/file.txt", "this is a test file");
  TestMount testMount{builder, false};
  const auto& edenMount = testMount.getEdenMount();

  // Call EdenMount::getInodeSlow() to do a recursive lookup
  auto fileFuture =
      edenMount
          ->getInodeSlow(
              "a/b/c/d/file.txt"_relpath, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());

  builder.setReady("a");
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());
  builder.setReady("a/b");
  testMount.drainServerExecutor();
  EXPECT_FALSE(fileFuture.isReady());

  auto bInode = testMount.getTreeInode("a/b"_relpath);

  // Rename c to x while the recursive resolution is still trying
  // to look it up.
  auto renameFuture = bInode
                          ->rename(
                              "c"_pc,
                              bInode,
                              "x"_pc,
                              InvalidationRequired::No,
                              ObjectFetchContext::getNullContext())
                          .semi()
                          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  // The rename will not complete until C becomes ready
  EXPECT_FALSE(renameFuture.isReady());
  EXPECT_FALSE(fileFuture.isReady());

  builder.setReady("a/b/c");
  testMount.drainServerExecutor();
  EXPECT_TRUE(renameFuture.isReady());
  EXPECT_FALSE(fileFuture.isReady());

  // Now mark the rest of the tree ready
  // Note that we don't actually have to mark the file itself ready.
  // The Inode lookup itself doesn't need the blob data yet.
  builder.setReady("a/b/c/d");
  testMount.drainServerExecutor();
  ASSERT_TRUE(fileFuture.isReady());
  auto fileInode = std::move(fileFuture).get();
  // We should have successfully looked up the inode, but it will report it
  // self (correctly) at its new path now.
  EXPECT_EQ("a/b/x/d/file.txt"_relpath, fileInode->getPath().value());
}

// Tests InodeMap::lookupInode when loading an unloaded inode by inode number
TEST(InodeMap, lookingUpAnUnloadedInodeAddsLoadsToTraceBus) {
  folly::UnboundedQueue<InodeTraceEvent, true, true, false> queue;
  auto builder = FakeTreeBuilder();
  builder.setFile("a/b/file.txt", "this is a test file");
  TestMount testMount{builder, false};
  const auto& edenMount = testMount.getEdenMount();
  auto* inodeMap = edenMount->getInodeMap();
  auto& trace_bus = edenMount->getInodeTraceBus();

  // Detect inode load events and add events to synchronized queue
  auto handle = trace_bus.subscribeFunction(
      fmt::format("inodeMapTest-{}", edenMount->getPath().basename()),
      [&](const InodeTraceEvent& event) {
        if (event.eventType == InodeEventType::LOAD) {
          std::cout << fmt::format(
                           "Event: {} {} {} ",
                           event.getPath(),
                           event.ino,
                           ((event.progress == InodeEventProgress::END)
                                ? "End"
                                : "Start"))
                    << std::endl;
          queue.enqueue(event);
        }
      });

  // Wait for any initial load events to complete
  size_t iteration_count = 0;
  while (queue.try_dequeue_for(loadTimeoutLimit).has_value() &&
         iteration_count < maxWaitForLoads) {
    ++iteration_count;
    if (iteration_count >= maxWaitForLoads) {
      throw std::runtime_error{
          "EdenFS not settling after startup, too many loads"};
    }
  };

  // In order to get inode numbers, we load "a" and "a/b" by path
  auto root = edenMount->getRootInode();
  auto a_future =
      edenMount->getInodeSlow("a"_relpath, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  auto b_future =
      edenMount
          ->getInodeSlow("a/b"_relpath, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  builder.setReady("a");
  builder.setReady("a/b");
  testMount.drainServerExecutor();

  // Get inode numbers and move inodes to the InodeMap's UnloadedInodes
  InodeNumber an, bn;
  {
    auto a = std::move(a_future).get(0ms).asTreePtr();
    an = a->getNodeId();
    a->incFsRefcount();
    {
      auto b = std::move(b_future).get(0ms);
      bn = b->getNodeId();
      b->incFsRefcount();
    }
    a->unloadChildrenNow(); // Unloads b
  }
  root->unloadChildrenNow(); // Unloads a

  // With inode numbers, we ensure the initial loads came in expected order
  EXPECT_TRUE(isInodeMaterializedInQueue(queue, InodeEventProgress::START, an));
  EXPECT_TRUE(isInodeMaterializedInQueue(queue, InodeEventProgress::END, an));
  EXPECT_TRUE(isInodeMaterializedInQueue(queue, InodeEventProgress::START, bn));
  EXPECT_TRUE(isInodeMaterializedInQueue(queue, InodeEventProgress::END, bn));

  // Finally, we lookup the inodes by InodeNumber
  auto secondAFuture =
      inodeMap->lookupTreeInode(an, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  auto secondBFuture =
      inodeMap->lookupTreeInode(bn, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  std::move(secondAFuture).get(0ms);
  std::move(secondBFuture).get(0ms);
  EXPECT_TRUE(isInodeMaterializedInQueue(queue, InodeEventProgress::START, an));
  EXPECT_TRUE(isInodeMaterializedInQueue(queue, InodeEventProgress::END, an));
  EXPECT_TRUE(isInodeMaterializedInQueue(queue, InodeEventProgress::START, bn));
  EXPECT_TRUE(isInodeMaterializedInQueue(queue, InodeEventProgress::END, bn));

  // Ensure we do not count any other loads a second time
  EXPECT_FALSE(queue.try_dequeue_for(loadTimeoutLimit).has_value());
}

TEST(InodeMap, unloadedUnlinkedTreesAreRemovedFromOverlay) {
  FakeTreeBuilder builder;
  builder.setFile("dir1/file.txt", "contents");
  builder.setFile("dir2/file.txt", "contents");
  TestMount mount{builder};
  auto edenMount = mount.getEdenMount();

  auto root = edenMount->getRootInode();
  auto dir1 = mount.getTreeInode("dir1"_relpath);
  auto dir2 = mount.getTreeInode("dir2"_relpath);
  auto dir1ino = dir1->getNodeId();
  auto dir2ino = dir2->getNodeId();

  auto fut = dir1->unlink(
                     "file.txt"_pc,
                     InvalidationRequired::No,
                     ObjectFetchContext::getNullContext())
                 .semi()
                 .via(mount.getServerExecutor().get());
  mount.drainServerExecutor();
  std::move(fut).get(0ms);
  fut = dir2->unlink(
                "file.txt"_pc,
                InvalidationRequired::No,
                ObjectFetchContext::getNullContext())
            .semi()
            .via(mount.getServerExecutor().get());
  mount.drainServerExecutor();
  std::move(fut).get(0ms);

  // Test both having a positive and zero fuse reference counts.
  dir2->incFsRefcount();

  fut = root->rmdir(
                "dir1"_pc,
                InvalidationRequired::No,
                ObjectFetchContext::getNullContext())
            .semi()
            .via(mount.getServerExecutor().get());
  mount.drainServerExecutor();
  std::move(fut).get(0ms);
  fut = root->rmdir(
                "dir2"_pc,
                InvalidationRequired::No,
                ObjectFetchContext::getNullContext())
            .semi()
            .via(mount.getServerExecutor().get());
  mount.drainServerExecutor();
  std::move(fut).get(0ms);

  dir1.reset();
  dir2.reset();

  edenMount->getInodeMap()->decFsRefcount(dir2ino);
  edenMount->getOverlay()->flushPendingAsync().get();
  EXPECT_FALSE(mount.hasOverlayDir(dir1ino));
  EXPECT_FALSE(mount.hasOverlayDir(dir2ino));
#ifndef _WIN32
  EXPECT_FALSE(mount.hasMetadata(dir1ino));
  EXPECT_FALSE(mount.hasMetadata(dir2ino));
#endif // !_WIN32
}

#ifndef _WIN32

TEST(InodeMap, unloadedUnlinkedFilesAreRemovedFromOverlay) {
  TestMount mount{FakeTreeBuilder{}};
  auto edenMount = mount.getEdenMount();
  auto* overlay = edenMount->getOverlay();
  auto root = edenMount->getRootInode();

  auto file1 =
      root->mknod("file1"_pc, S_IFREG | 0644, 0, InvalidationRequired::No);
  auto file2 =
      root->mknod("file2"_pc, S_IFREG | 0644, 0, InvalidationRequired::No);
  auto file1ino = file1->getNodeId();
  auto file2ino = file2->getNodeId();
  EXPECT_TRUE(overlay->hasOverlayFile(file1ino));
  EXPECT_TRUE(overlay->hasOverlayFile(file2ino));

  // Test both having a positive and zero fuse reference counts.
  file2->incFsRefcount();
  file1.reset();
  file2.reset();

  for (auto name : {"file1"_pc, "file2"_pc}) {
    auto fut = root->unlink(
                       name,
                       InvalidationRequired::No,
                       ObjectFetchContext::getNullContext())
                   .semi()
                   .via(mount.getServerExecutor().get());
    mount.drainServerExecutor();
    std::move(fut).get(0ms);
  }

  // Unlinked and unreferenced: removed once the overlay GC thread gets to it.
  overlay->flushPendingAsync().get();
  EXPECT_FALSE(overlay->hasOverlayFile(file1ino));
  // Still referenced by the filesystem: kept until that reference drops.
  EXPECT_TRUE(overlay->hasOverlayFile(file2ino));
  edenMount->getInodeMap()->decFsRefcount(file2ino);
  overlay->flushPendingAsync().get();
  EXPECT_FALSE(overlay->hasOverlayFile(file2ino));
}

TEST(InodeMap, lookupOfRemovedUnloadedInodeWhoseNameWasReused) {
  TestMount mount{FakeTreeBuilder{}};
  auto edenMount = mount.getEdenMount();
  auto* inodeMap = edenMount->getInodeMap();
  auto root = edenMount->getRootInode();

  auto file =
      root->mknod("file"_pc, S_IFREG | 0644, 0, InvalidationRequired::No);
  const auto oldIno = file->getNodeId();
  file->incFsRefcount();
  file.reset();
  root->unloadChildrenNow();
  ASSERT_TRUE(inodeMap->isInodeRemembered(oldIno));

  // Recursive removal drops an unloaded child without loading it, and the
  // InodeMap keeps remembering the inode for the FS reference.
  root->removeRecursively(
          "file"_pc,
          InvalidationRequired::No,
          ObjectFetchContext::getNullContext())
      .get(0ms);
  auto newFile =
      root->mknod("file"_pc, S_IFREG | 0644, 0, InvalidationRequired::No);
  ASSERT_NE(oldIno, newFile->getNodeId());

  // Looking the old inode up by number must fail instead of loading the new
  // file under the old number.
  auto lookup =
      inodeMap->lookupInode(oldIno, ObjectFetchContext::getNullContext())
          .semi()
          .via(mount.getServerExecutor().get());
  mount.drainServerExecutor();
  EXPECT_THROW_ERRNO(std::move(lookup).get(0ms), ENOENT);
  EXPECT_EQ(newFile->getNodeId(), root->getChildInodeNumber("file"_pc));

  // The old inode stays remembered until its FS reference is released.
  EXPECT_TRUE(inodeMap->isInodeRemembered(oldIno));
  inodeMap->decFsRefcount(oldIno);
  EXPECT_FALSE(inodeMap->isInodeRemembered(oldIno));
}

TEST(InodeMap, unloadedFileMetadataIsForgotten) {
  FakeTreeBuilder builder;
  builder.setFile("dir1/file.txt", "contents");
  builder.setFile("dir2/file.txt", "contents");
  TestMount mount{builder};
  auto edenMount = mount.getEdenMount();

  auto root = edenMount->getRootInode();
  auto dir1 = mount.getTreeInode("dir1"_relpath);
  auto dir2 = mount.getTreeInode("dir2"_relpath);

  auto file1 = mount.getFileInode("dir1/file.txt"_relpath);
  auto file1ino = file1->getNodeId();
  auto file2 = mount.getFileInode("dir2/file.txt"_relpath);
  auto file2ino = file2->getNodeId();

  EXPECT_TRUE(mount.hasMetadata(file1ino));
  EXPECT_TRUE(mount.hasMetadata(file2ino));

  // Try having both positive and zero FUSE reference counts.
  file1->incFsRefcount();
  file1.reset();
  file2.reset();

  dir1->unlink(
          PathComponentPiece{"file.txt"},
          InvalidationRequired::No,
          ObjectFetchContext::getNullContext())
      .get(0ms);
  dir2->unlink(
          PathComponentPiece{"file.txt"},
          InvalidationRequired::No,
          ObjectFetchContext::getNullContext())
      .get(0ms);

  edenMount->getOverlay()->flushPendingAsync().get();
  EXPECT_TRUE(mount.hasMetadata(file1ino));
  EXPECT_FALSE(mount.hasMetadata(file2ino));

  edenMount->getInodeMap()->decFsRefcount(file1ino);
  edenMount->getOverlay()->flushPendingAsync().get();
  EXPECT_FALSE(mount.hasMetadata(file1ino));
  EXPECT_FALSE(mount.hasMetadata(file2ino));
}
#endif

struct InodePersistenceTreeTest : ::testing::Test {
  InodePersistenceTreeTest() {
    builder.setFile("dir/file1.txt", "contents1");
    builder.setFile("dir/file2.txt", "contents2");
  }

  FakeTreeBuilder builder;
};

// Inode numbers only survive a remount when the overlay is durable. The
// Windows overlay is rebuilt from the on-disk PrjFS state, so these tests
// only apply to Linux and macOS.
#ifndef _WIN32
struct InodePersistenceTakeoverTest : InodePersistenceTreeTest {
  InodePersistenceTakeoverTest()
      : testMount{builder}, edenMount{testMount.getEdenMount()} {}

  void SetUp() override {
    InodePersistenceTreeTest::SetUp();

    auto tree = testMount.getInode("dir"_relpath);
    auto file1 = testMount.getInode("dir/file1.txt"_relpath);
    auto file2 = testMount.getInode("dir/file2.txt"_relpath);

    // Pretend FUSE is keeping references to these.
    tree->incFsRefcount();
    file1->incFsRefcount();
    file2->incFsRefcount();

    oldTreeId = tree->getNodeId();
    oldFile1Id = file1->getNodeId();
    oldFile2Id = file2->getNodeId();

    edenMount.reset();
    tree.reset();
    file1.reset();
    file2.reset();
    testMount.remountGracefully();
    edenMount = testMount.getEdenMount();
  }

  TestMount testMount;
  std::shared_ptr<EdenMount> edenMount;

  InodeNumber oldTreeId;
  InodeNumber oldFile1Id;
  InodeNumber oldFile2Id;
};

TEST_F(
    InodePersistenceTakeoverTest,
    preservesInodeNumbersForLoadedInodesDuringTakeover_lookupFirstByName) {
  // Look up in a different order to avoid allocating the same numbers.
  auto tree = testMount.getInode("dir"_relpath);
  auto file2 = testMount.getInode("dir/file2.txt"_relpath);
  auto file1 = testMount.getInode("dir/file1.txt"_relpath);

  EXPECT_EQ(1, tree->debugGetFsRefcount());
  EXPECT_EQ(1, file1->debugGetFsRefcount());
  EXPECT_EQ(1, file2->debugGetFsRefcount());

  EXPECT_EQ(oldTreeId, tree->getNodeId());
  EXPECT_EQ(oldFile1Id, file1->getNodeId());
  EXPECT_EQ(oldFile2Id, file2->getNodeId());

  // Now try looking up by inode number.
  EXPECT_EQ(
      "dir",
      edenMount->getInodeMap()
          ->lookupInode(oldTreeId, ObjectFetchContext::getNullContext())
          .get()
          ->getLogPath());
  EXPECT_EQ(
      "dir/file1.txt",
      edenMount->getInodeMap()
          ->lookupInode(oldFile1Id, ObjectFetchContext::getNullContext())
          .get()
          ->getLogPath());
  EXPECT_EQ(
      "dir/file2.txt",
      edenMount->getInodeMap()
          ->lookupInode(oldFile2Id, ObjectFetchContext::getNullContext())
          .get()
          ->getLogPath());
}

TEST_F(
    InodePersistenceTakeoverTest,
    preservesInodeNumbersForLoadedInodesDuringTakeover_lookupFirstByNumber) {
  // Look up by number first.
  auto oldTreeIdFut =
      edenMount->getInodeMap()
          ->lookupInode(oldTreeId, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  auto oldFile1IdFut =
      edenMount->getInodeMap()
          ->lookupInode(oldFile1Id, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  auto oldFile2IdFut =
      edenMount->getInodeMap()
          ->lookupInode(oldFile2Id, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();

  EXPECT_EQ("dir", std::move(oldTreeIdFut).get(0ms)->getLogPath());
  EXPECT_EQ("dir/file1.txt", std::move(oldFile1IdFut).get(0ms)->getLogPath());
  EXPECT_EQ("dir/file2.txt", std::move(oldFile2IdFut).get(0ms)->getLogPath());

  // Verify the same inodes can be looked up by name too.
  auto tree = testMount.getInode("dir"_relpath);
  auto file2 = testMount.getInode("dir/file2.txt"_relpath);
  auto file1 = testMount.getInode("dir/file1.txt"_relpath);

  EXPECT_EQ(1, tree->debugGetFsRefcount());
  EXPECT_EQ(1, file1->debugGetFsRefcount());
  EXPECT_EQ(1, file2->debugGetFsRefcount());

  EXPECT_EQ(oldTreeId, tree->getNodeId());
  EXPECT_EQ(oldFile1Id, file1->getNodeId());
  EXPECT_EQ(oldFile2Id, file2->getNodeId());
}

/**
 * clang and gcc use the inode number of a header to determine whether it's the
 * same file as one previously included and marked #pragma once.
 *
 * At least as long as the mount is up (including though graceful takeovers),
 * Eden must provide consistent inode numbers.
 */
TEST_F(
    InodePersistenceTreeTest,
    preservesInodeNumbersForUnloadedInodesDuringTakeover) {
  TestMount testMount{builder};
  auto edenMount = testMount.getEdenMount();

  auto tree = testMount.getInode("dir"_relpath);
  auto file1 = testMount.getInode("dir/file1.txt"_relpath);
  auto file2 = testMount.getInode("dir/file2.txt"_relpath);

  tree->incFsRefcount();
  file1->incFsRefcount();
  file2->incFsRefcount();

  auto oldTreeId = tree->getNodeId();
  auto oldFile1Id = file1->getNodeId();
  auto oldFile2Id = file2->getNodeId();

  tree->decFsRefcount();
  file1->decFsRefcount();
  file2->decFsRefcount();

  edenMount.reset();

  tree.reset();
  file1.reset();
  file2.reset();
  testMount.remountGracefully();

  edenMount = testMount.getEdenMount();

  // Look up in a different order.
  tree = testMount.getInode("dir"_relpath);
  file2 = testMount.getInode("dir/file2.txt"_relpath);
  file1 = testMount.getInode("dir/file1.txt"_relpath);

  EXPECT_EQ(oldTreeId, tree->getNodeId());
  EXPECT_EQ(oldFile1Id, file1->getNodeId());
  EXPECT_EQ(oldFile2Id, file2->getNodeId());
}

TEST_F(InodePersistenceTreeTest, preservesLastGcRefTimeDuringTakeover) {
  TestMount testMount{builder};
  auto edenMount = testMount.getEdenMount();
  auto file = testMount.getInode("dir/file1.txt"_relpath);
  file->incFsRefcount();
  const auto inodeNumber = file->getNodeId();
  const auto expectedLastGcRefTime = file->getLastGcRefTime();

  testMount.getClock().advance(120s);
  edenMount.reset();
  file.reset();
  testMount.remountGracefully();

  auto reloaded =
      testMount.getEdenMount()
          ->getInodeMap()
          ->lookupInode(inodeNumber, ObjectFetchContext::getNullContext())
          .get();
  EXPECT_EQ(expectedLastGcRefTime, reloaded->getLastGcRefTime());
}
#endif

// Verify createInodeLoadFailEvent publishes a FAIL event when a load fails
// for an inode in unloadedInodes_.
TEST(InodeMap, createInodeLoadFailEventPublishesFailEvent) {
  folly::UnboundedQueue<InodeTraceEvent, true, true, false> queue;
  auto builder = FakeTreeBuilder();
  builder.setFile("src/test.txt", "this is a test file");
  TestMount testMount{builder, false};
  const auto& edenMount = testMount.getEdenMount();
  auto& trace_bus = edenMount->getInodeTraceBus();

  auto handle = trace_bus.subscribeFunction(
      fmt::format("loadFailEventTest-{}", edenMount->getPath().basename()),
      [&](const InodeTraceEvent& event) {
        if (event.eventType == InodeEventType::LOAD) {
          queue.enqueue(event);
        }
      });

  size_t iteration_count = 0;
  while (queue.try_dequeue_for(loadTimeoutLimit).has_value() &&
         iteration_count < maxWaitForLoads) {
    ++iteration_count;
    if (iteration_count >= maxWaitForLoads) {
      throw std::runtime_error{
          "EdenFS not settling after startup, too many loads"};
    }
  }

  auto rootInode = edenMount->getRootInode();
  auto srcFuture =
      rootInode->getOrLoadChild("src"_pc, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  EXPECT_FALSE(srcFuture.isReady());

  auto startEvent = queue.try_dequeue_for(loadTimeoutLimit);
  ASSERT_TRUE(startEvent.has_value());
  EXPECT_EQ(InodeEventProgress::START, startEvent->progress);

  builder.triggerError("src", std::domain_error("injected error for testing"));
  testMount.drainServerExecutor();
  ASSERT_TRUE(srcFuture.isReady());
  EXPECT_THROW(std::move(srcFuture).get(), std::domain_error);

  auto failEvent = queue.try_dequeue_for(loadTimeoutLimit);
  ASSERT_TRUE(failEvent.has_value()) << "Expected a LOAD FAIL event";
  EXPECT_EQ(InodeEventProgress::FAIL, failEvent->progress);
  EXPECT_EQ(InodeEventType::LOAD, failEvent->eventType);
  EXPECT_EQ(startEvent->ino, failEvent->ino);
}

TEST(InodeMap, totalInodeCountFastMatchesInodeCounts) {
  FakeTreeBuilder builder;
  builder.setFile("src/a.cpp", "a");
  builder.setFile("src/b.cpp", "b");
  builder.setFile("doc/readme.md", "hi");
  TestMount testMount{builder};

  auto* edenMount = testMount.getEdenMount().get();
  auto* inodeMap = edenMount->getInodeMap();

  auto getTotalFromCounts = [&]() {
    auto counts = inodeMap->getInodeCounts();
    return counts.fileCount + counts.treeCount + counts.unloadedInodeCount;
  };

  // After loading inodes, fast count should match the detailed counts.
  auto srcA = testMount.getInode("src/a.cpp"_relpath);
  auto srcB = testMount.getInode("src/b.cpp"_relpath);
  auto doc = testMount.getInode("doc/readme.md"_relpath);

  EXPECT_EQ(getTotalFromCounts(), inodeMap->getTotalInodeCountFast());

  // Simulate FUSE references then unload — inodes become "unloaded" but
  // the total count should still be consistent.
  srcA->incFsRefcount();
  srcB->incFsRefcount();
  srcA.reset();
  srcB.reset();
  doc.reset();

  edenMount->getRootInode()->unloadChildrenNow();

  EXPECT_EQ(getTotalFromCounts(), inodeMap->getTotalInodeCountFast());

  // Decrement FS refcount to fully forget inodes — total count should
  // decrease.
  auto countBeforeForget = inodeMap->getTotalInodeCountFast();
  auto srcAIno = testMount.getInode("src/a.cpp"_relpath)->getNodeId();
  testMount.getInode("src/a.cpp"_relpath).reset();
  edenMount->getRootInode()->unloadChildrenNow();
  inodeMap->decFsRefcount(srcAIno, 1);

  EXPECT_EQ(getTotalFromCounts(), inodeMap->getTotalInodeCountFast());
  EXPECT_LT(inodeMap->getTotalInodeCountFast(), countBeforeForget);
}

TEST(InodeMap, forgottenCountExcludesReloadedInodes) {
  FakeTreeBuilder builder;
  builder.setFile("file", "contents");
  TestMount testMount{builder};
  auto* mount = testMount.getEdenMount().get();
  auto* inodeMap = mount->getInodeMap();
  auto file = testMount.getInode("file"_relpath);
  const auto ino = file->getNodeId();
  file->incFsRefcount();
  file.reset();
  mount->getRootInode()->unloadChildrenNow();
  ASSERT_TRUE(inodeMap->isInodeRemembered(ino));
  const auto before = inodeMap->getInodeCounts().forgottenInodeCount;

  file = inodeMap->lookupInode(ino, ObjectFetchContext::getNullContext()).get();
  EXPECT_EQ(before, inodeMap->getInodeCounts().forgottenInodeCount);
  file.reset();
  mount->getRootInode()->unloadChildrenNow();
  EXPECT_TRUE(inodeMap->clearFsRefcount(ino));
  EXPECT_EQ(before + 1, inodeMap->getInodeCounts().forgottenInodeCount);
  EXPECT_FALSE(inodeMap->isInodeLoadedOrRemembered(ino));
}

TEST(InodeMap, inodeLoadFailureLogsError) {
  CapturingXplatLogger xplatLogger;
  auto config = EdenConfig::createTestEdenConfig();
  config->enableErrorLogging.setValue(true, ConfigSourceType::UserConfig);
  auto reloadableConfig = std::make_shared<ReloadableConfig>(config);
  auto errorLogger =
      std::make_shared<ErrorLogger>(reloadableConfig, &xplatLogger);

  auto builder = FakeTreeBuilder();
  builder.setFile("src/main.c", "int main() { return 0; }\n");
  TestMount testMount{
      builder,
      /*startReady=*/false,
      /*enableActivityBuffer=*/true,
      kPathMapDefaultCaseSensitive,
      errorLogger};

  auto rootInode = testMount.getEdenMount()->getRootInode();
  auto srcFuture =
      rootInode->getOrLoadChild("src"_pc, ObjectFetchContext::getNullContext())
          .semi()
          .via(testMount.getServerExecutor().get());
  testMount.drainServerExecutor();
  EXPECT_FALSE(srcFuture.isReady());

  builder.triggerError(
      "src", std::domain_error("rejecting lookup for src tree"));
  testMount.drainServerExecutor();
  ASSERT_TRUE(srcFuture.isReady());
  EXPECT_THROW(std::move(srcFuture).get(), std::domain_error);

  ASSERT_EQ(xplatLogger.events().size(), 1);
  const auto& strings = xplatLogger.events()[0].event.getStringMap();
  EXPECT_EQ(strings.at("component"), "object_store");
  EXPECT_EQ(strings.at("error_type"), "inode_loading_failed");
}

TEST(InodeMap, loadingASecondNameForALoadedInodeFails) {
  FakeTreeBuilder builder;
  builder.setFile("dir/a.txt", "a\n");
  TestMount testMount{builder};

  auto dir = testMount.getTreeInode("dir");
  auto a = testMount.getFileInode("dir/a.txt");

  // The directory state an interrupted rename leaves on disk: a second entry
  // carries a.txt's inode number.
  dir->lockContentsWrite()->entries.emplace(
      "b.txt"_pc, S_IFREG | 0644, a->getNodeId());

  EXPECT_THROW_ERRNO(
      dir->getOrLoadChild("b.txt"_pc, ObjectFetchContext::getNullContext())
          .get(0ms),
      EIO);
  EXPECT_EQ(
      a->getNodeId(),
      dir->getOrLoadChild("a.txt"_pc, ObjectFetchContext::getNullContext())
          .get(0ms)
          ->getNodeId());
}

TEST(InodeMap, lookupByNumberLoadsWithCallerContext) {
  FakeTreeBuilder builder;
  builder.setFile("a/b/file.txt", "contents\n");
  TestMount testMount{builder};
  auto* inodeMap = testMount.getEdenMount()->getInodeMap();

  // Remember a, b and the file by number, then unload all three.
  auto a = testMount.getTreeInode("a");
  auto b = testMount.getTreeInode("a/b");
  auto file = testMount.getFileInode("a/b/file.txt");
  const auto fileNumber = file->getNodeId();
  a->incFsRefcount();
  b->incFsRefcount();
  file->incFsRefcount();
  a.reset();
  b.reset();
  file.reset();
  testMount.getEdenMount()->getRootInode()->unloadChildrenNow();
  ASSERT_FALSE(inodeMap->lookupLoadedInode(fileNumber));

  // Looking the file up by number loads a and b first, each from its tree;
  // both loads are fetched with the caller's context.
  auto context = makeRefPtr<StatsFetchContext>(
      std::nullopt,
      ObjectFetchContext::Cause::Thrift,
      ObjectFetchContext::StaticCauseDetail::fromLiteral("InodeMapTest"),
      nullptr);
  EXPECT_EQ(
      fileNumber,
      inodeMap->lookupInode(fileNumber, context.copy()).get(0ms)->getNodeId());
  EXPECT_EQ(2, context->countFetchesOfType(ObjectFetchContext::Tree));
}
