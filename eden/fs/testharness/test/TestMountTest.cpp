/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/testharness/TestMount.h"
#include <folly/Range.h>
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/model/Hash.h"
#include "eden/fs/model/TestOps.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"

#include <gtest/gtest.h>

using namespace facebook::eden;
using folly::ByteRange;
using folly::StringPiece;

TEST(TestMount, createEmptyMount) {
  FakeTreeBuilder builder;
  TestMount testMount{builder};
  auto rootTree = testMount.getRootTree();
  EXPECT_EQ(0, rootTree->size()) << "Initially, the tree should be empty.";
}

TEST(TestMount, createSimpleTestMount) {
  FakeTreeBuilder builder;
  builder.setFile("path1", "first!");
  builder.setFiles({
      // clang-format off
      {"path2", "hello"},
      {"path3", "world"},
      // clang-format on
  });
  TestMount testMount{builder};

  auto path1Inode = testMount.getFileInode("path1");
  EXPECT_NE(nullptr, path1Inode.get())
      << "Should be able to find FileInode for path1";

  auto blobId = path1Inode->getObjectId();
  ASSERT_TRUE(blobId.has_value());
  auto expectedId = ObjectId::fromHex(
      "3f" + Hash20::sha1(ByteRange(StringPiece("first!"))).toString());
  EXPECT_EQ(expectedId, blobId.value())
      << "For simplicity, TestMount uses its type byte and the SHA-1 of the "
      << "contents as the id for a Blob.";

  auto dirTreeEntry = testMount.getTreeInode("");
  {
    auto dir = dirTreeEntry->lockContentsRead();
    auto& rootEntries = dir->entries;
    auto& path1Entry = rootEntries.at("path1"_pc);
    ASSERT_FALSE(path1Entry.isMaterialized());
    EXPECT_EQ(expectedId, path1Entry.getObjectId())
        << "Getting the Entry from the root Dir should also work.";
  }

  auto rootTree = testMount.getRootTree();
  EXPECT_EQ(3, rootTree->size())
      << "Root Tree object should have 3 entries: path1, path2, path3";
}

TEST(TestMount, addFileAfterMountIsCreated) {
  FakeTreeBuilder builder;
  builder.setFile(
      "file1.txt", "I am in the original commit that is backing the mount.");
  TestMount testMount{builder};

  testMount.addFile("file2.txt", "I am added by the user after mounting.");
  auto dirTreeEntry = testMount.getTreeInode("");
  {
    auto dir = dirTreeEntry->lockContentsRead();
    auto& rootEntries = dir->entries;
    EXPECT_EQ(3, rootEntries.size()) << "New entry is visible in MountPoint";
  }

  auto rootTree = testMount.getRootTree();
  EXPECT_EQ(1, rootTree->size()) << "New entry is not in the Tree, though.";
}

TEST(TestMount, overwriteFile) {
  FakeTreeBuilder builder;
  builder.setFile("file.txt", "original contents");
  TestMount testMount{builder};
  EXPECT_EQ("original contents", testMount.readFile("file.txt"));

  testMount.overwriteFile("file.txt", "new contents");
  EXPECT_EQ("new contents", testMount.readFile("file.txt"));
}

TEST(TestMount, hasFileAt) {
  FakeTreeBuilder builder;
  builder.setFile("file.txt", "contents");
  builder.setFile("a/file.txt", "contents");
  TestMount testMount{builder};

  // Verify hasFileAt() works properly on files added to the Tree.
  EXPECT_TRUE(testMount.hasFileAt("file.txt"));
  EXPECT_FALSE(testMount.hasFileAt("iDoNotExist.txt"));
  EXPECT_TRUE(testMount.hasFileAt("a/file.txt"));
  EXPECT_FALSE(testMount.hasFileAt("a"))
      << "hasFileAt(directory) should return false rather than throw";

  testMount.addFile("newFile.txt", "contents");
  testMount.mkdir("b");
  testMount.addFile("b/newFile.txt", "contents");

  // Verify hasFileAt() works properly on files added to the Overlay.
  EXPECT_TRUE(testMount.hasFileAt("newFile.txt"));
  EXPECT_FALSE(testMount.hasFileAt("iDoNotExist.txt"));
  EXPECT_TRUE(testMount.hasFileAt("b/newFile.txt"));
  EXPECT_FALSE(testMount.hasFileAt("b"))
      << "hasFileAt(directory) should return false rather than throw";
  EXPECT_FALSE(testMount.hasFileAt("b/c/oneLevelBeyondLastExistingDirectory"))
      << "hasFileAt(directory) should return false rather than throw";
}

TEST(TestMount, mkdir) {
  FakeTreeBuilder builder;
  TestMount testMount{builder};

  testMount.mkdir("a");
  testMount.addFile("a/file.txt", "original contents");
  EXPECT_EQ("original contents", testMount.readFile("a/file.txt"));
}

TEST(TestMount, deleteFile) {
  FakeTreeBuilder builder;
  builder.setFile("file.txt", "original contents");
  TestMount testMount{builder};
  EXPECT_TRUE(testMount.hasFileAt("file.txt"));

  testMount.deleteFile("file.txt");
  EXPECT_FALSE(testMount.hasFileAt("file.txt"));
}

TEST(TestMount, rmdir) {
  FakeTreeBuilder builder;
  builder.setFile("dir/file.txt", "original contents");
  TestMount testMount{builder};
  EXPECT_TRUE(testMount.hasFileAt("dir/file.txt"));
  EXPECT_NE(nullptr, testMount.getTreeInode("dir"));

  testMount.deleteFile("dir/file.txt");
  EXPECT_NE(nullptr, testMount.getTreeInode("dir"));
  testMount.rmdir("dir");

  try {
    testMount.getTreeInode("dir");
    FAIL() << "ENOENT should be thrown";
  } catch (const std::system_error& expected) {
    ASSERT_EQ(ENOENT, expected.code().value());
  }
}

TEST(TestMount, createFileInSubdirectory) {
  FakeTreeBuilder builder;
  builder.setFile("a/b/c.txt", "I am in the a/b/ directory.");
  TestMount testMount{builder};

  testMount.addFile("a/b/d.txt", "Another file in the a/b directory.");
}

TEST(TestMount, mkdirWithoutParentShouldThrowENOENT) {
  FakeTreeBuilder builder;
  TestMount testMount{builder};

  try {
    testMount.mkdir("x/y/z");
    FAIL() << "ENOENT should be thrown";
  } catch (const std::system_error& expected) {
    ASSERT_EQ(ENOENT, expected.code().value());
  }
}

TEST(TestMount, addFileDoesNotLeakFuseRefcount) {
  FakeTreeBuilder builder;
  TestMount testMount{builder};
  testMount.addFile("f", "contents");
  auto f = testMount.getFileInode("f");
  EXPECT_EQ(0, f->debugGetFsRefcount());
}

#ifndef _WIN32
TEST(TestMount, addSymlinkDoesNotLeakFuseRefcount) {
  FakeTreeBuilder builder;
  TestMount testMount{builder};
  testMount.addSymlink("l", "targets");
  auto link = testMount.getFileInode("l");
  EXPECT_EQ(0, link->debugGetFsRefcount());
}
#endif

TEST(TestMount, mkdirDoesNotLeakFuseRefcount) {
  FakeTreeBuilder builder;
  TestMount testMount{builder};
  testMount.mkdir("d");
  auto d = testMount.getTreeInode("d");
  EXPECT_EQ(0, d->debugGetFsRefcount());
}

TEST(TestMount, can_update_EdenConfig) {
  FakeTreeBuilder builder;
  TestMount testMount{builder};
  testMount.updateEdenConfig({
      {"fuse:max-concurrent-requests", "52"},
  });

  EXPECT_EQ(
      52,
      testMount.getServerState()
          ->getEdenConfig()
          ->fuseMaximumBackgroundRequests.getValue());
}
