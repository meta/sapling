/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/config/CheckoutConfig.h"

#include <folly/test/TestUtils.h>
#include <folly/testing/TestUtil.h>
#include <gtest/gtest.h>

#include "eden/common/utils/FileUtils.h"
#include "eden/common/utils/PathFuncs.h"

using namespace facebook::eden;
using namespace facebook::eden::path_literals;

using folly::StringPiece;

namespace {

using folly::test::TemporaryDirectory;

class CheckoutConfigTest : public ::testing::Test {
 protected:
  std::unique_ptr<TemporaryDirectory> edenDir_;
  AbsolutePath clientDir_;
  AbsolutePath mountPoint_;
  AbsolutePath configDotToml_;

  void SetUp() override {
    edenDir_ = std::make_unique<TemporaryDirectory>("eden_config_test_");
    auto clientDir = edenDir_->path() / "client";
    folly::fs::create_directory(clientDir);
    clientDir_ = canonicalPath(clientDir.string());
    mountPoint_ = canonicalPath("/tmp/someplace");

    auto snapshotPath = clientDir_ + "SNAPSHOT"_pc;
    auto snapshotContents = folly::StringPiece{
        "eden\00\00\00\01"
        "\x12\x34\x56\x78\x12\x34\x56\x78\x12\x34"
        "\x56\x78\x12\x34\x56\x78\x12\x34\x56\x78",
        28};
    writeFile(snapshotPath, snapshotContents).value();

    configDotToml_ = clientDir_ + "config.toml"_pc;
    auto localData =
        "[repository]\n"
        "path = \"/data/users/carenthomas/fbsource\"\n"
        "type = \"git\"\n";
    writeFile(configDotToml_, folly::StringPiece{localData}).value();
  }

  void TearDown() override {
    edenDir_.reset();
  }

  template <typename ExceptionType = std::runtime_error>
  void testBadSnapshot(StringPiece contents, const char* errorRegex);
};
} // namespace

TEST_F(CheckoutConfigTest, testLoadFromClientDirectory) {
  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);

  auto parent = config->getParentCommit();
  auto rootId = RootId{"1234567812345678123456781234567812345678"};
  EXPECT_EQ(
      ParentCommit(
          ParentCommit::WorkingCopyParentAndCheckedOutRevision{rootId, rootId}),
      parent);
  if (folly::kIsWindows) {
    EXPECT_EQ("\\\\?\\tmp\\someplace", config->getMountPath());
  } else {
    EXPECT_EQ("/tmp/someplace", config->getMountPath());
  }
}

TEST_F(CheckoutConfigTest, testLoadWithIgnoredSettings) {
  // Overwrite config.toml with extra ignored data in the config file
  auto data =
      "[repository]\n"
      "path = \"/data/users/carenthomas/fbsource\"\n"
      "type = \"git\"\n"
      "color = \"blue\"\n"
      "[bind-mounts]\n"
      "my-path = \"path/to-my-path\"\n";
  writeFile(configDotToml_, folly::StringPiece{data}).value();

  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);

  auto parent = config->getParentCommit();
  auto rootId = RootId{"1234567812345678123456781234567812345678"};
  EXPECT_EQ(
      ParentCommit(
          ParentCommit::WorkingCopyParentAndCheckedOutRevision{rootId, rootId}),
      parent);
  if (folly::kIsWindows) {
    EXPECT_EQ("\\\\?\\tmp\\someplace", config->getMountPath());
  } else {
    EXPECT_EQ("/tmp/someplace", config->getMountPath());
  }
}

namespace {
class CheckoutConfigProtocolTest
    : public ::testing::TestWithParam<MountProtocol> {
 protected:
  void SetUp() override {
    edenDir_ = std::make_unique<TemporaryDirectory>("eden_config_test_");
    auto clientDir = edenDir_->path() / "client";
    folly::fs::create_directory(clientDir);
    clientDir_ = canonicalPath(clientDir.string());
    mountPoint_ = canonicalPath("/tmp/someplace");

    auto snapshotPath = clientDir_ + "SNAPSHOT"_pc;
    auto snapshotContents = folly::StringPiece{
        "eden\00\00\00\01"
        "\x12\x34\x56\x78\x12\x34\x56\x78\x12\x34"
        "\x56\x78\x12\x34\x56\x78\x12\x34\x56\x78",
        28};
    writeFile(snapshotPath, snapshotContents).value();

    configDotToml_ = clientDir_ + "config.toml"_pc;
  }

  void TearDown() override {
    edenDir_.reset();
  }

  std::unique_ptr<TemporaryDirectory> edenDir_;
  AbsolutePath clientDir_;
  AbsolutePath mountPoint_;
  AbsolutePath configDotToml_;
};
} // namespace

TEST_P(CheckoutConfigProtocolTest, testProtocolRoundtrip) {
  auto protocol = GetParam();
  auto localData = fmt::format(
      "[repository]\n"
      "path = \"/data/users/carenthomas/fbsource\"\n"
      "type = \"git\"\n"
      "protocol = \"{}\"\n",
      FieldConverter<MountProtocol>{}.toDebugString(protocol));
  writeFile(configDotToml_, folly::StringPiece{localData}).value();

  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);
  EXPECT_EQ(config->getRawMountProtocol(), protocol);
}

INSTANTIATE_TEST_CASE_P(
    Protocol,
    CheckoutConfigProtocolTest,
    ::testing::Values(
        MountProtocol::FUSE,
        MountProtocol::PRJFS,
        MountProtocol::NFS),
    [](const ::testing::TestParamInfo<MountProtocol>& info) {
      return FieldConverter<MountProtocol>{}.toDebugString(info.param);
    });

TEST_F(CheckoutConfigTest, testInvalidProtocol) {
  auto localData =
      "[repository]\n"
      "path = \"/data/users/carenthomas/fbsource\"\n"
      "type = \"git\"\n"
      "protocol = \"INVALID\"\n";
  writeFile(configDotToml_, folly::StringPiece{localData}).value();

  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);
  EXPECT_EQ(config->getMountProtocol(), kMountProtocolDefault);
}

TEST_F(CheckoutConfigTest, testMountProtocolDefault) {
  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);
  EXPECT_EQ(config->getMountProtocol(), kMountProtocolDefault);
}

TEST_F(CheckoutConfigTest, testVersion1MultipleParents) {
  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);

  // Overwrite the SNAPSHOT file to indicate that there are two parents
  auto snapshotContents = folly::StringPiece{
      "eden\00\00\00\01"
      "\x99\x88\x77\x66\x55\x44\x33\x22\x11\x00"
      "\xaa\xbb\xcc\xdd\xee\xff\xab\xcd\xef\x99"
      "\xab\xcd\xef\x98\x76\x54\x32\x10\x01\x23"
      "\x45\x67\x89\xab\xcd\xef\x00\x11\x22\x33",
      48};
  auto snapshotPath = clientDir_ + "SNAPSHOT"_pc;
  writeFile(snapshotPath, snapshotContents).value();

  auto parent = config->getParentCommit();
  auto rootId = RootId{"99887766554433221100aabbccddeeffabcdef99"};
  EXPECT_EQ(
      ParentCommit(
          ParentCommit::WorkingCopyParentAndCheckedOutRevision{rootId, rootId}),
      parent);
}

TEST_F(CheckoutConfigTest, testVersion2ParentBinary) {
  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);

  // Overwrite the SNAPSHOT file to contain a binary hash.
  auto snapshotContents = folly::StringPiece{
      "eden\00\00\00\02"
      "\x00\x00\x00\x14"
      "\x99\x88\x77\x66\x55\x44\x33\x22\x11\x00"
      "\xaa\xbb\xcc\xdd\xee\xff\xab\xcd\xef\x99",
      32};
  auto snapshotPath = clientDir_ + "SNAPSHOT"_pc;
  writeFile(snapshotPath, snapshotContents).value();

  auto parent = config->getParentCommit();
  auto rootId =
      RootId{Hash20{"99887766554433221100aabbccddeeffabcdef99"}.toByteString()};
  EXPECT_EQ(
      ParentCommit(
          ParentCommit::WorkingCopyParentAndCheckedOutRevision{rootId, rootId}),
      parent);
}

TEST_F(CheckoutConfigTest, testIntentionallyUnmountedFlag) {
  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);
  EXPECT_FALSE(config->isIntentionallyUnmounted());

  config->setIntentionallyUnmountedFlag();
  EXPECT_TRUE(config->isIntentionallyUnmounted());

  config->clearIntentionallyUnmountedFlag();
  EXPECT_FALSE(config->isIntentionallyUnmounted());
}

TEST_F(CheckoutConfigTest, testInProgress) {
  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);

  // Overwrite the SNAPSHOT file to contain an in progress binary hash.
  auto snapshotContents = folly::StringPiece{
      "eden\00\00\00\03"
      "\x00\x00\x00\x01" // PID
      "\x00\x00\x00\x28" // Size of following hash
      "99887766554433221100aabbccddeeffabcdef99"
      "\x00\x00\x00\x28" // Size of following hash
      "fedcba99887766554433221100ffeeddccbbaa99",
      100};
  auto snapshotPath = clientDir_ + "SNAPSHOT"_pc;
  writeFile(snapshotPath, snapshotContents).value();

  ParentCommit inProgress = ParentCommit::CheckoutInProgress{
      RootId{"99887766554433221100aabbccddeeffabcdef99"},
      RootId{"fedcba99887766554433221100ffeeddccbbaa99"},
      123};

  auto parent = config->getParentCommit();
  EXPECT_EQ(inProgress, parent);
}

TEST_F(CheckoutConfigTest, testInProgressRoundtrip) {
  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);

  auto from = RootId{"99887766554433221100aabbccddeeffabcdef99"};
  auto to = RootId{"fedcba998887766554433221100ffeeddccbbaa99"};
  ParentCommit inProgress = ParentCommit::CheckoutInProgress{from, to, 123};

  config->setCheckoutInProgress(from, to);

  auto parent = config->getParentCommit();
  EXPECT_EQ(inProgress, parent);
}

TEST_F(CheckoutConfigTest, testCheckedOutAndReset) {
  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);

  auto from = RootId{"99887766554433221100aabbccddeeffabcdef99"};
  auto to = RootId{"fedcba998887766554433221100ffeeddccbbaa99"};

  config->setCheckedOutCommit(from);
  config->setWorkingCopyParentCommit(to);

  auto parent = config->getParentCommit();
  EXPECT_EQ(
      ParentCommit(
          ParentCommit::WorkingCopyParentAndCheckedOutRevision{to, from}),
      parent);

  // Make sure that setCheckedOutCommit changes both.
  config->setCheckedOutCommit(from);
  parent = config->getParentCommit();
  EXPECT_EQ(
      ParentCommit(
          ParentCommit::WorkingCopyParentAndCheckedOutRevision{from, from}),
      parent);
}

TEST_F(CheckoutConfigTest, testVersion2ParentHex) {
  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);

  // Overwrite the SNAPSHOT file to contain a hexadecimal hash.
  auto snapshotContents = folly::StringPiece{
      "eden\00\00\00\02"
      "\x00\x00\x00\x28"
      "99887766554433221100aabbccddeeffabcdef99",
      52};
  auto snapshotPath = clientDir_ + "SNAPSHOT"_pc;
  writeFile(snapshotPath, snapshotContents).value();

  auto parent = config->getParentCommit();
  auto rootId = RootId{"99887766554433221100aabbccddeeffabcdef99"};
  EXPECT_EQ(
      ParentCommit(
          ParentCommit::WorkingCopyParentAndCheckedOutRevision{rootId, rootId}),
      parent);
}

TEST_F(CheckoutConfigTest, testWriteSnapshot) {
  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);

  RootId id1{"99887766554433221100aabbccddeeffabcdef99"};
  RootId id2{"abcdef98765432100123456789abcdef00112233"};

  // Write out a single parent and read it back
  config->setCheckedOutCommit(id1);
  auto parent = config->getParentCommit();
  EXPECT_EQ(
      ParentCommit(
          ParentCommit::WorkingCopyParentAndCheckedOutRevision{id1, id1}),
      parent);

  // Change the parent
  config->setCheckedOutCommit(id2);
  parent = config->getParentCommit();
  EXPECT_EQ(
      ParentCommit(
          ParentCommit::WorkingCopyParentAndCheckedOutRevision{id2, id2}),
      parent);

  // Change the parent back
  config->setCheckedOutCommit(id1);
  parent = config->getParentCommit();
  EXPECT_EQ(
      ParentCommit(
          ParentCommit::WorkingCopyParentAndCheckedOutRevision{id1, id1}),
      parent);
}

template <typename ExceptionType>
void CheckoutConfigTest::testBadSnapshot(
    StringPiece contents,
    const char* errorRegex) {
  SCOPED_TRACE(
      folly::to<std::string>("SNAPSHOT contents: ", folly::hexlify(contents)));
  writeFile(clientDir_ + "SNAPSHOT"_pc, contents).value();

  auto config =
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_);
  EXPECT_THROW_RE(config->getParentCommit(), ExceptionType, errorRegex);
}

TEST_F(CheckoutConfigTest, testBadSnapshotV1) {
  testBadSnapshot("edge", "SNAPSHOT file is too short");
  testBadSnapshot("eden", "SNAPSHOT file is too short");
  testBadSnapshot(StringPiece{"eden\0\0\0", 7}, "SNAPSHOT file is too short");
  testBadSnapshot(
      StringPiece{"eden\0\0\0\1", 8},
      "unexpected length for eden SNAPSHOT file");
  testBadSnapshot(
      StringPiece{"eden\0\0\0\x0exyza", 12},
      "unsupported eden SNAPSHOT file format \\(version 14\\)");
  testBadSnapshot(
      StringPiece{
          "eden\00\00\00\01"
          "\x99\x88\x77\x66\x55\x44\x33\x22\x11\x00"
          "\xaa\xbb\xcc\xdd\xee\xff\xab\xcd\xef\x99"
          "\xab\xcd\xef\x98\x76\x54\x32\x10\x01\x23"
          "\x45\x67\x89\xab\xcd\xef\x00\x11\x22",
          47},
      "unexpected length for eden SNAPSHOT file");
  testBadSnapshot(
      StringPiece{
          "eden\00\00\00\01"
          "\x99\x88\x77\x66\x55\x44\x33\x22\x11\x00"
          "\xaa\xbb\xcc\xdd\xee\xff\xab\xcd\xef\x99"
          "\xab\xcd\xef\x98\x76\x54\x32\x10\x01\x23"
          "\x45\x67\x89\xab\xcd\xef\x00\x11\x22\x33\x44",
          49},
      "unexpected length for eden SNAPSHOT file");

  // The error type and message for this will probably change in the future
  // when we drop support for the legacy SNAPSHOT file format (of a 40-byte
  // ASCII string containing the snapshot id).
  testBadSnapshot(
      StringPiece{
          "xden\00\00\00\01"
          "\x99\x88\x77\x66\x55\x44\x33\x22\x11\x00"
          "\xaa\xbb\xcc\xdd\xee\xff\xab\xcd\xef\x99"
          "\xab\xcd\xef\x98\x76\x54\x32\x10\x01\x23"
          "\x45\x67\x89\xab\xcd\xef\x00\x11\x22\x33",
          48},
      "unsupported legacy SNAPSHOT file");
}

TEST_F(CheckoutConfigTest, testBadSnapshotV2) {
  testBadSnapshot<std::out_of_range>(
      StringPiece{"eden\0\0\0\2", 8}, "underflow");
  testBadSnapshot<std::out_of_range>(
      StringPiece{"eden\0\0\0\2\x00\x00\x00", 11}, "underflow");
  testBadSnapshot<std::out_of_range>(
      StringPiece{"eden\0\0\0\2\x00\x00\x00\x02\x32", 13}, "string underflow");
}

TEST_F(CheckoutConfigTest, testMissingRepositorySection) {
  writeFile(
      configDotToml_,
      folly::StringPiece{"[redirection-targets]\nfoo = \"bar\"\n"})
      .value();
  EXPECT_THROW_RE(
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_),
      std::runtime_error,
      "config.toml: missing \\[repository\\] table");
}

TEST_F(CheckoutConfigTest, testRepositoryNotATable) {
  writeFile(configDotToml_, folly::StringPiece{"repository = \"hg\"\n"})
      .value();
  EXPECT_THROW_RE(
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_),
      std::runtime_error,
      "config.toml: missing \\[repository\\] table");
}

TEST_F(CheckoutConfigTest, testMissingRepositoryType) {
  writeFile(
      configDotToml_,
      folly::StringPiece{"[repository]\npath = \"/data/users/x/fbsource\"\n"})
      .value();
  EXPECT_THROW_RE(
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_),
      std::runtime_error,
      "config.toml: \\[repository\\] has no string key type");
}

TEST_F(CheckoutConfigTest, testNonStringRepositoryPath) {
  writeFile(
      configDotToml_,
      folly::StringPiece{"[repository]\ntype = \"hg\"\npath = 42\n"})
      .value();
  EXPECT_THROW_RE(
      CheckoutConfig::loadFromClientDirectory(mountPoint_, clientDir_),
      std::runtime_error,
      "config.toml: \\[repository\\] has no string key path");
}
