/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/inodes/Overlay.h"

#include <fmt/format.h>
#include <folly/Exception.h>
#include <folly/Expected.h>
#include <folly/FileUtil.h>
#include <folly/Range.h>
#include <folly/executors/CPUThreadPoolExecutor.h>
#include <folly/logging/test/TestLogHandler.h>
#include <folly/test/TestUtils.h>
#include <folly/testing/TestUtil.h>
#include <gtest/gtest.h>

#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/inodes/test/OverlayTestUtil.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/testharness/TestMount.h"

using namespace folly::string_piece_literals;

namespace facebook::eden {

class EphemeralFsOverlayTest : public ::testing::Test {
 protected:
  void deleteOverlay() {
    overlay->close();
    overlay.reset();
  }

  void createOverlay() {
    auto edenConfig = EdenConfig::createTestEdenConfig();
    edenConfig->inodeCatalogType.setValue(
        InodeCatalogType::LegacyEphemeral, ConfigSourceType::Default, true);

    overlay = Overlay::create(
        canonicalPath(testDir.path().string()),
        kPathMapDefaultCaseSensitive,
        InodeCatalogType::LegacyEphemeral,
        makeTestEdenFsEventsLogger(),
        /*errorLogger=*/noopErrorLogger_,
        makeRefPtr<EdenStats>(),
        *edenConfig);
    overlay
        ->initialize(std::make_shared<ReloadableConfig>(std::move(edenConfig)))
        .get();
  }

  void SetUp() override {
    createOverlay();
  }

  folly::test::TemporaryDirectory testDir;
  ErrorLogger noopErrorLogger_ = makeTestErrorLogger();
  std::shared_ptr<Overlay> overlay;
};

TEST_F(EphemeralFsOverlayTest, testOverlayCreatesEphemeralInodeCatalog) {
  EXPECT_NE(
      dynamic_cast<EphemeralFsInodeCatalog*>(overlay->getRawInodeCatalog()),
      nullptr);
}

TEST_F(EphemeralFsOverlayTest, roundTripThroughSaveAndLoad) {
  auto id = ObjectId::fromHex("0123456789012345678901234567890123456789");

  auto ino1 = overlay->allocateInodeNumber();
  auto ino2 = overlay->allocateInodeNumber();
  auto ino3 = overlay->allocateInodeNumber();

  DirContents dir(kPathMapDefaultCaseSensitive);
  dir.emplace("one"_pc, S_IFREG | 0644, ino2, id);
  dir.emplace("two"_pc, S_IFDIR | 0755, ino3);

  overlay->saveOverlayDir(ino1, dir);

  auto result = overlay->loadOverlayDir(ino1);
  ASSERT_TRUE(!result.empty());

  EXPECT_EQ(2, result.size());
  const auto& one = result.find("one"_pc)->second;
  const auto& two = result.find("two"_pc)->second;
  EXPECT_EQ(ino2, one.getInodeNumber());
  EXPECT_FALSE(one.isMaterialized());
  EXPECT_EQ(ino3, two.getInodeNumber());
  EXPECT_TRUE(two.isMaterialized());
}

TEST_F(EphemeralFsOverlayTest, maxInodeNumberIs1) {
  EXPECT_EQ(kRootNodeId, overlay->getMaxInodeNumber());
  auto ino2 = overlay->allocateInodeNumber();
  EXPECT_EQ(2_ino, ino2);
}

TEST_F(EphemeralFsOverlayTest, manualRecursiveDelete) {
  auto rootIno = kRootNodeId;
  EXPECT_EQ(1_ino, rootIno);
  auto subdirIno = overlay->allocateInodeNumber();
  EXPECT_EQ(2_ino, subdirIno);
  auto subdirIno2 = overlay->allocateInodeNumber();
  EXPECT_EQ(3_ino, subdirIno2);

  DirContents rootContents(kPathMapDefaultCaseSensitive);
  rootContents.emplace("subdir"_pc, S_IFDIR | 0755, subdirIno);
  overlay->saveOverlayDir(rootIno, rootContents);

  DirContents subdirContents(kPathMapDefaultCaseSensitive);
  subdirContents.emplace("subdir2"_pc, S_IFDIR | 0755, subdirIno2);
  overlay->saveOverlayDir(subdirIno, subdirContents);

  DirContents subdir2Contents(kPathMapDefaultCaseSensitive);
  overlay->saveOverlayDir(subdirIno2, subdir2Contents);

  auto subdir2 =
      static_cast<EphemeralFsInodeCatalog*>(overlay->getRawInodeCatalog())
          ->loadAndRemoveOverlayDir(subdirIno2);
  EXPECT_TRUE(subdir2.has_value());
  auto expectedSubdir2 =
      overlay->serializeOverlayDir(subdirIno2, subdir2Contents);
  EXPECT_EQ(expectedSubdir2, subdir2.value());

  auto subdir =
      static_cast<EphemeralFsInodeCatalog*>(overlay->getRawInodeCatalog())
          ->loadAndRemoveOverlayDir(subdirIno);
  EXPECT_TRUE(subdir.has_value());
  auto expectedSubdir = overlay->serializeOverlayDir(subdirIno, subdirContents);
  EXPECT_EQ(expectedSubdir, subdir.value());

  auto nextIno = overlay->allocateInodeNumber();
  auto nonExistSubdir =
      static_cast<EphemeralFsInodeCatalog*>(overlay->getRawInodeCatalog())
          ->loadAndRemoveOverlayDir(nextIno);
  EXPECT_FALSE(nonExistSubdir.has_value());
}

TEST_F(EphemeralFsOverlayTest, cannotCreateOverlayIfDirty) {
  EXPECT_EQ(kRootNodeId, overlay->getMaxInodeNumber());
  auto ino2 = overlay->allocateInodeNumber();
  EXPECT_EQ(2_ino, ino2);
  auto ino3 = overlay->allocateInodeNumber();
  auto ino4 = overlay->allocateInodeNumber();

  DirContents dir(kPathMapDefaultCaseSensitive);
  dir.emplace(PathComponentPiece{"f"}, S_IFREG | 0644, ino3);
  dir.emplace(PathComponentPiece{"d"}, S_IFDIR | 0755, ino4);
  overlay->saveOverlayDir(kRootNodeId, dir);

  deleteOverlay();

  try {
    createOverlay();
    FAIL() << "Expected createOverlay() to throw an exception";
  } catch (const std::runtime_error& e) {
    std::string errorMessage = e.what();
    EXPECT_NE(
        errorMessage.find(
            "EphemeralFsInodeCatalog only supports fresh overlays but a pre-existing overlay was found"),
        std::string::npos)
        << fmt::format(
               "Expected error message to contain the specific overlay error, but got: {}",
               errorMessage);
  }
}

} // namespace facebook::eden

#endif
