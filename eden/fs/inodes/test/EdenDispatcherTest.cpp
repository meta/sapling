/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/fuse/FuseDispatcher.h"

#include <fmt/format.h>
#include <array>
#include <atomic>
#include <limits>
#include <optional>
#include <stdexcept>
#include <string_view>
#include <thread>

#include <folly/test/TestUtils.h>
#include <folly/testing/TestUtil.h>
#include <gtest/gtest.h>
#include "eden/common/utils/FaultInjector.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/InodeMap.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/model/Blob.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/FakeFuse.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/StoredObject.h"
#include "eden/fs/testharness/TestMount.h"
#include "eden/fs/utils/LinuxKernelVersion.h"

using namespace facebook::eden;
using namespace std::chrono_literals;
using namespace folly::string_piece_literals;

namespace {
struct EdenDispatcherTest : ::testing::Test {
  EdenDispatcherTest() : mount{FakeTreeBuilder{}} {}
  TestMount mount;
};

constexpr auto kTooLongPiece = folly::StringPiece{
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"};
static_assert(256 == kTooLongPiece.size(), "256 is one larger than the max!");

static const auto kTooLong = PathComponentPiece{kTooLongPiece};

// Mirrors the INVAL_ENTRY mitigation in
// FuseDispatcherImpl::computeNegativeEntryTtl().
uint64_t expectedNegativeTtl(uint64_t configured) {
#ifdef __linux__
  auto version = getRunningLinuxKernelVersion();
  if (version.major > 6 || (version.major == 6 && version.minor >= 19)) {
    return 0;
  }
#endif
  return configured;
}

constexpr std::array<std::string_view, 5> kFuseDispatcherFaultPoints{
    "FuseDispatcherImpl::lookup",
    "FuseDispatcherImpl::negativeLookup",
    "FuseDispatcherImpl::getattr",
    "FuseDispatcherImpl::read",
    "FuseDispatcherImpl::write",
};

template <typename Trigger>
void expectFaultPointReachable(
    TestMount& mount,
    std::string_view faultPoint,
    Trigger&& trigger) {
  auto& injector = mount.getServerState()->getFaultInjector();
  injector.injectBlock(faultPoint, ".*", 1);

  std::optional<std::string> error;
  std::thread request([future = trigger(), &error]() mutable {
    try {
      std::move(future).get();
    } catch (const std::exception& ex) {
      error = ex.what();
    }
  });

  const auto reached = injector.waitUntilBlocked(faultPoint, 5s);
  injector.removeFault(faultPoint, ".*");
  const auto unblocked = injector.unblockWithError(
      faultPoint, ".*", std::runtime_error("checkpoint reached"));
  request.join();

  ASSERT_TRUE(reached) << faultPoint;
  EXPECT_EQ(1, unblocked);
  ASSERT_TRUE(error.has_value());
  EXPECT_EQ("checkpoint reached", *error);
}
} // namespace

TEST_F(EdenDispatcherTest, mknodReturnsNameTooLong) {
  try {
    mount.getDispatcher()
        ->mknod(
            kRootNodeId,
            kTooLong,
            S_IFREG | 0644,
            0,
            ObjectFetchContext::getNullContext())
        .get(0ms);
    FAIL() << "should throw";
  } catch (std::system_error& e) {
    EXPECT_EQ(ENAMETOOLONG, e.code().value());
  }
}

TEST_F(EdenDispatcherTest, mkdirReturnsNameTooLong) {
  try {
    mount.getDispatcher()
        ->mkdir(
            kRootNodeId,
            kTooLong,
            S_IFDIR | 0755,
            ObjectFetchContext::getNullContext())
        .get(0ms);
    FAIL() << "should throw";
  } catch (std::system_error& e) {
    EXPECT_EQ(ENAMETOOLONG, e.code().value());
  }
}

TEST_F(EdenDispatcherTest, symlinkReturnsNameTooLong) {
  try {
    mount.getDispatcher()
        ->symlink(
            kRootNodeId,
            kTooLong,
            "aoeu"_sp,
            ObjectFetchContext::getNullContext())
        .get(0ms);
    FAIL() << "should throw";
  } catch (std::system_error& e) {
    EXPECT_EQ(ENAMETOOLONG, e.code().value());
  }
}

TEST_F(EdenDispatcherTest, renameReturnsNameTooLong) {
  try {
    mount.getDispatcher()
        ->rename(
            kRootNodeId,
            "oldname"_pc,
            kRootNodeId,
            kTooLong,
            ObjectFetchContext::getNullContext())
        .get(0ms);
    FAIL() << "should throw";
  } catch (std::system_error& e) {
    EXPECT_EQ(ENAMETOOLONG, e.code().value());
  }
}

TEST_F(EdenDispatcherTest, linkReturnsNameTooLong) {
  try {
    // Eden doesn't support hard links yet and this link call could never work
    // in the first place, but at least validate the target name length.
    mount.getDispatcher()->link(kRootNodeId, kRootNodeId, kTooLong).get(0ms);
    FAIL() << "should throw";
  } catch (std::system_error& e) {
    EXPECT_EQ(ENAMETOOLONG, e.code().value());
  }
}

TEST_F(EdenDispatcherTest, createStripsPrivilegeBits) {
  // The kernel is told (FUSE_HANDLE_KILLPRIV_V2) that files never carry
  // setuid, setgid or sticky bits, so create must not store them.
  auto entry = mount.getDispatcher()
                   ->create(
                       kRootNodeId,
                       "f"_pc,
                       S_IFREG | 04755,
                       O_WRONLY,
                       ObjectFetchContext::getNullContext())
                   .get(0ms);
  EXPECT_EQ(S_IFREG | 0755, entry.attr.mode);
}

TEST_F(EdenDispatcherTest, mknodStripsPrivilegeBits) {
  auto entry = mount.getDispatcher()
                   ->mknod(
                       kRootNodeId,
                       "f"_pc,
                       S_IFREG | 02644,
                       0,
                       ObjectFetchContext::getNullContext())
                   .get(0ms);
  EXPECT_EQ(S_IFREG | 0644, entry.attr.mode);
}

TEST_F(EdenDispatcherTest, createReturnsNameTooLong) {
  try {
    mount.getDispatcher()
        ->create(
            kRootNodeId,
            kTooLong,
            S_IFREG | 0644,
            0,
            ObjectFetchContext::getNullContext())
        .get(0ms);
    FAIL() << "should throw";
  } catch (std::system_error& e) {
    EXPECT_EQ(ENAMETOOLONG, e.code().value());
  }
}

TEST(RawEdenDispatcherTest, lookup_returns_valid_inode_for_good_file) {
  FakeTreeBuilder builder;
  builder.setFile("good", "contents");
  TestMount mount{builder};

  auto entry =
      mount.getDispatcher()
          ->lookup(
              0, kRootNodeId, "good"_pc, ObjectFetchContext::getNullContext())
          .get(0ms);
  EXPECT_NE(0u, entry.nodeid);
  EXPECT_NE(0, entry.attr.ino);
  EXPECT_EQ(entry.nodeid, entry.attr.ino);
}

TEST(RawEdenDispatcherTest, fuseDispatcherFaultPointsAreReachable) {
  FakeTreeBuilder builder;
  builder.setFile("file", "contents");
  TestMount mount{builder};
  auto* dispatcher = mount.getDispatcher();
  const auto entry =
      dispatcher
          ->lookup(
              0, kRootNodeId, "file"_pc, ObjectFetchContext::getNullContext())
          .get(0ms);
  const auto ino = InodeNumber{entry.nodeid};

  expectFaultPointReachable(mount, kFuseDispatcherFaultPoints[0], [&] {
    return dispatcher->lookup(
        0, kRootNodeId, "file"_pc, ObjectFetchContext::getNullContext());
  });
  expectFaultPointReachable(mount, kFuseDispatcherFaultPoints[1], [&] {
    return dispatcher->lookup(
        0, kRootNodeId, "missing"_pc, ObjectFetchContext::getNullContext());
  });
  expectFaultPointReachable(mount, kFuseDispatcherFaultPoints[2], [&] {
    return dispatcher->getattr(ino, ObjectFetchContext::getNullContext());
  });
  expectFaultPointReachable(mount, kFuseDispatcherFaultPoints[3], [&] {
    return dispatcher->read(ino, 1, 0, ObjectFetchContext::getNullContext());
  });
  expectFaultPointReachable(mount, kFuseDispatcherFaultPoints[4], [&] {
    return dispatcher->write(ino, "x", 0, ObjectFetchContext::getNullContext());
  });
}

TEST(RawEdenDispatcherTest, negativeLookupFaultPointIsReachableOnEnoentPath) {
  FakeTreeBuilder builder;
  builder.setFile("restricted/file", "contents");
  builder.setDirIsRestricted("restricted");
  TestMount mount{builder};
  auto restricted = mount.getTreeInode("restricted"_relpath);
  ASSERT_TRUE(restricted->isRestricted());

  // getOrLoadChildIfExists() hands restricted directories to getOrLoadChild().
  // Once the permission recheck grants access, that path reports the missing
  // name as ENOENT instead of returning a null inode.
  auto restrictedTreeId =
      builder.getRoot()->get().find("restricted"_pc)->second.getObjectId();
  mount.getBackingStore()->setCheckPermissionResult(restrictedTreeId, true);

  expectFaultPointReachable(mount, kFuseDispatcherFaultPoints[1], [&] {
    return mount.getDispatcher()->lookup(
        0,
        restricted->getNodeId(),
        "missing"_pc,
        ObjectFetchContext::getNullContext());
  });
  EXPECT_FALSE(restricted->isRestricted());
}

TEST(RawEdenDispatcherTest, lookup_updates_last_used_time) {
  FakeTreeBuilder builder;
  builder.setFile("hello", "world");
  TestMount mount{builder};

  // Lookup to load the inode and get its number
  auto entry =
      mount.getDispatcher()
          ->lookup(
              0, kRootNodeId, "hello"_pc, ObjectFetchContext::getNullContext())
          .get(0ms);
  auto ino = InodeNumber{entry.nodeid};
  auto inode = mount.getEdenMount()
                   ->getInodeMap()
                   ->lookupInode(ino, ObjectFetchContext::getNullContext())
                   .get(0ms);
  auto timeAfterLookup = inode->getLastGcRefTime();

  // Advance the clock
  mount.getClock().advance(60s);

  // Another lookup should update lastGcRefTime
  mount.getDispatcher()
      ->lookup(0, kRootNodeId, "hello"_pc, ObjectFetchContext::getNullContext())
      .get(0ms);
  auto timeAfterSecondLookup = inode->getLastGcRefTime();
  EXPECT_GT(
      timeAfterSecondLookup.toTimespec().tv_sec,
      timeAfterLookup.toTimespec().tv_sec);
}

TEST(RawEdenDispatcherTest, getattr_updates_last_used_time) {
  FakeTreeBuilder builder;
  builder.setFile("hello", "world");
  TestMount mount{builder};

  // Lookup to load the inode
  auto entry =
      mount.getDispatcher()
          ->lookup(
              0, kRootNodeId, "hello"_pc, ObjectFetchContext::getNullContext())
          .get(0ms);
  auto ino = InodeNumber{entry.nodeid};
  auto inode = mount.getEdenMount()
                   ->getInodeMap()
                   ->lookupInode(ino, ObjectFetchContext::getNullContext())
                   .get(0ms);

  // Advance the clock
  mount.getClock().advance(60s);

  // getattr should update lastGcRefTime
  auto timeBefore = inode->getLastGcRefTime();
  mount.getDispatcher()
      ->getattr(ino, ObjectFetchContext::getNullContext())
      .get(0ms);
  auto timeAfter = inode->getLastGcRefTime();
  EXPECT_GT(timeAfter.toTimespec().tv_sec, timeBefore.toTimespec().tv_sec);
}

TEST(RawEdenDispatcherTest, lookup_returns_infinite_ttl_without_pressure_gc) {
  FakeTreeBuilder builder;
  builder.setFile("hello", "world");
  TestMount mount{builder};

  auto entry =
      mount.getDispatcher()
          ->lookup(
              0, kRootNodeId, "hello"_pc, ObjectFetchContext::getNullContext())
          .get(0ms);
  // Without pressure-based GC, TTL should be the default infinite value
  EXPECT_EQ(
      static_cast<uint64_t>(std::numeric_limits<int32_t>::max()),
      entry.entry_valid);
  EXPECT_EQ(
      static_cast<uint64_t>(std::numeric_limits<int32_t>::max()),
      entry.attr_valid);
}

TEST(RawEdenDispatcherTest, lookup_returns_default_negative_dcache_ttl) {
  TestMount mount{FakeTreeBuilder{}};

  auto entry = mount.getDispatcher()
                   ->lookup(
                       0,
                       kRootNodeId,
                       "missing"_pc,
                       ObjectFetchContext::getNullContext())
                   .get(0ms);

  EXPECT_EQ(0u, entry.nodeid);
  EXPECT_EQ(expectedNegativeTtl(60), entry.entry_valid);
  EXPECT_EQ(expectedNegativeTtl(60), entry.attr_valid);
}

TEST(RawEdenDispatcherTest, lookup_returns_configured_negative_dcache_ttl) {
  TestMount mount{FakeTreeBuilder{}};

  mount.updateEdenConfig({
      {"fuse:negative-dcache-ttl-seconds", "7"},
  });

  auto entry = mount.getDispatcher()
                   ->lookup(
                       0,
                       kRootNodeId,
                       "missing"_pc,
                       ObjectFetchContext::getNullContext())
                   .get(0ms);

  EXPECT_EQ(0u, entry.nodeid);
  EXPECT_EQ(expectedNegativeTtl(7), entry.entry_valid);
  EXPECT_EQ(expectedNegativeTtl(7), entry.attr_valid);
}

TEST(RawEdenDispatcherTest, lookup_returns_dynamic_ttl_with_pressure_gc) {
  FakeTreeBuilder builder;
  builder.setFile("hello", "world");
  TestMount mount{builder};

  // Enable pressure-based GC with known settings
  mount.updateEdenConfig({
      {"experimental:enable-pressure-based-gc", "true"},
      {"mount:gc-pressure-min-inodes", "10"},
      {"mount:gc-pressure-max-inodes", "10000"},
      {"mount:fuse-ttl-max-seconds", "3600"},
      {"mount:fuse-ttl-min-seconds", "1"},
  });
  mount.getEdenMount()->updateInodePressurePolicy();

  auto entry =
      mount.getDispatcher()
          ->lookup(
              0, kRootNodeId, "hello"_pc, ObjectFetchContext::getNullContext())
          .get(0ms);
  // With very few inodes (below min of 10), TTL should be max value of 3600
  EXPECT_EQ(3600u, entry.entry_valid);
  EXPECT_EQ(3600u, entry.attr_valid);
}

TEST(RawEdenDispatcherTest, getattr_returns_dynamic_ttl_with_pressure_gc) {
  FakeTreeBuilder builder;
  builder.setFile("hello", "world");
  TestMount mount{builder};

  mount.updateEdenConfig({
      {"experimental:enable-pressure-based-gc", "true"},
      {"mount:gc-pressure-min-inodes", "10"},
      {"mount:gc-pressure-max-inodes", "10000"},
      {"mount:fuse-ttl-max-seconds", "3600"},
      {"mount:fuse-ttl-min-seconds", "1"},
  });
  mount.getEdenMount()->updateInodePressurePolicy();

  // First lookup to get the inode number
  auto entry =
      mount.getDispatcher()
          ->lookup(
              0, kRootNodeId, "hello"_pc, ObjectFetchContext::getNullContext())
          .get(0ms);

  // Now getattr on that inode
  auto attr =
      mount.getDispatcher()
          ->getattr(
              InodeNumber{entry.nodeid}, ObjectFetchContext::getNullContext())
          .get(0ms);
  EXPECT_EQ(3600u, attr.timeout_seconds);
}

TEST(
    RawEdenDispatcherTest,
    pressure_gc_loads_unloaded_trees_and_invalidates_unloaded_stale_files) {
#ifndef __linux__
  GTEST_SKIP() << "FakeFuse invalidation tests are Linux-only";
#else
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "contents");
  builder.setFile("dir/cold.txt", "not loaded");
  TestMount mount{builder};

  mount.updateEdenConfig({
      {"experimental:enable-pressure-based-gc", "false"},
  });

  auto fuse = std::make_shared<FakeFuse>();
  mount.startFuseAndWait(fuse);

  auto dirEntry =
      mount.getDispatcher()
          ->lookup(
              0, kRootNodeId, "dir"_pc, ObjectFetchContext::getNullContext())
          .get(0ms);
  auto fileEntry = mount.getDispatcher()
                       ->lookup(
                           0,
                           InodeNumber{dirEntry.nodeid},
                           "file.txt"_pc,
                           ObjectFetchContext::getNullContext())
                       .get(0ms);

  mount.getClock().advance(11s);
  auto cutoff = folly::to<std::chrono::system_clock::time_point>(
                    mount.getClock().getRealtime()) -
      10s;
  auto dirNumber = InodeNumber{dirEntry.nodeid};
  auto fileNumber = InodeNumber{fileEntry.nodeid};
  EXPECT_GT(
      mount.getEdenMount()->getRootInode()->unloadChildrenLastAccessedBefore(
          folly::to<timespec>(cutoff)),
      0);
  EXPECT_FALSE(
      mount.getEdenMount()->getInodeMap()->lookupLoadedInode(dirNumber));
  EXPECT_FALSE(
      mount.getEdenMount()->getInodeMap()->lookupLoadedInode(fileNumber));

  auto numInvalidated =
      mount.getEdenMount()
          ->getRootInode()
          ->handleChildrenNotAccessedRecently(
              cutoff,
              ObjectFetchContext::getNullContext(),
              /*pressureBased=*/true,
              folly::CancellationToken{},
              /*pinnedInodes=*/
              std::make_shared<folly::F14FastSet<InodeNumber>>())
          .get(10s);
  EXPECT_EQ(2u, numInvalidated);
  EXPECT_TRUE(
      mount.getEdenMount()->getInodeMap()->lookupLoadedInode(dirNumber));
  EXPECT_FALSE(
      mount.getEdenMount()->getInodeMap()->lookupLoadedInode(fileNumber));

  mount.getEdenMount()->flushInvalidations().get(10s);
  fuse->close();
  mount.getEdenMount()->getFsChannelCompletionFuture().within(10s).getVia(
      mount.getServerExecutor().get());
#endif
}

TEST(
    RawEdenDispatcherTest,
    pressure_gc_invalidates_directories_larger_than_one_scan_batch) {
#ifndef __linux__
  GTEST_SKIP() << "FakeFuse invalidation tests are Linux-only";
#else
  // Larger than both the 1,024-entry directory scan batch and the
  // 1,024-entry invalidation queue limit.
  constexpr size_t kFileCount = 1'100;
  FakeTreeBuilder builder;
  for (size_t i = 0; i < kFileCount; ++i) {
    builder.setFile(fmt::format("dir/file{:04}.txt", i), "contents");
  }
  TestMount mount{builder};

  mount.updateEdenConfig({
      {"experimental:enable-pressure-based-gc", "true"},
  });

  auto fuse = std::make_shared<FakeFuse>();
  mount.startFuseAndWait(fuse);

  auto dirEntry =
      mount.getDispatcher()
          ->lookup(
              0, kRootNodeId, "dir"_pc, ObjectFetchContext::getNullContext())
          .get(0ms);
  for (size_t i = 0; i < kFileCount; ++i) {
    PathComponent name{fmt::format("file{:04}.txt", i)};
    mount.getDispatcher()
        ->lookup(
            0,
            InodeNumber{dirEntry.nodeid},
            name.piece(),
            ObjectFetchContext::getNullContext())
        .get(0ms);
  }

  mount.getClock().advance(11s);
  auto cutoff = folly::to<std::chrono::system_clock::time_point>(
                    mount.getClock().getRealtime()) -
      10s;

  // Queue backpressure blocks GC submission until the invalidation workers
  // write notifications to the FUSE device, and the device's socket buffer
  // holds far fewer than kFileCount messages, so drain it concurrently.
  fuse->setTimeout(100ms);
  std::atomic<bool> doneInvalidating{false};
  std::thread drainer([&] {
    while (!doneInvalidating.load(std::memory_order_acquire)) {
      try {
        fuse->recvResponse();
      } catch (const std::exception&) {
        // Timed out waiting for the next notification; check for completion.
      }
    }
  });

  auto numInvalidated =
      mount.getEdenMount()
          ->getRootInode()
          ->handleChildrenNotAccessedRecently(
              cutoff,
              ObjectFetchContext::getNullContext(),
              /*pressureBased=*/true,
              folly::CancellationToken{},
              /*pinnedInodes=*/
              std::make_shared<folly::F14FastSet<InodeNumber>>())
          .get(30s);
  mount.getEdenMount()->flushInvalidations().get(30s);
  doneInvalidating.store(true, std::memory_order_release);
  drainer.join();
  EXPECT_EQ(kFileCount + 1, numInvalidated);

  fuse->close();
  mount.getEdenMount()->getFsChannelCompletionFuture().within(10s).getVia(
      mount.getServerExecutor().get());
#endif
}

TEST(RawEdenDispatcherTest, lookup_returns_valid_inode_for_bad_file) {
  FakeTreeBuilder builder;
  builder.setFile("bad", "contents");
  TestMount mount{builder, /*startReady=*/false};
  auto entryFuture =
      mount.getDispatcher()
          ->lookup(
              0, kRootNodeId, "bad"_pc, ObjectFetchContext::getNullContext())
          .semi()
          .via(mount.getServerExecutor().get());
  builder.getStoredBlob("bad"_relpath)
      ->triggerError(std::runtime_error("failed to load"));
  builder.setAllReady();
  mount.drainServerExecutor();
  auto entry = std::move(entryFuture).get(0ms);
  EXPECT_NE(0, entry.nodeid);
  EXPECT_NE(0, entry.attr.ino);
  EXPECT_EQ(entry.nodeid, entry.attr.ino);
}

#endif
