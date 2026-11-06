/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <fb303/ServiceData.h>
#include <folly/executors/ManualExecutor.h>
#include <gtest/gtest.h>

#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/model/TestOps.h"
#include "eden/fs/service/PrettyPrinters.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/TestMount.h"

using namespace facebook::eden;
using namespace std::chrono_literals;

namespace {

TEST(
    Dematerialize,
    checkout_dematerializes_when_working_copy_matches_destination) {
  TestMount mount;
  auto backingStore = mount.getBackingStore();

  folly::StringPiece contents1{"contents 1\n"};
  folly::StringPiece contents2{"contents 2\n"};

  FakeTreeBuilder builder1;
  builder1.setFile("a/test.txt"_relpath, contents1, false, ObjectId{"object1"});

  FakeTreeBuilder builder2;
  builder2.setFile("a/test.txt"_relpath, contents2, false, ObjectId{"object2"});
  builder2.finalize(backingStore, /*setReady=*/true);
  auto commit2 = backingStore->putCommit("2", builder2);
  commit2->setReady();

  // Initialize the mount with the tree data from builder1
  mount.initialize(RootId{"1"}, builder1);

  auto executor = mount.getServerExecutor().get();

  // Load a/test.txt
  auto preInode = mount.overwriteFile("a/test.txt", contents2);
  EXPECT_EQ(
      "contents 2\n",
      preInode->readAll(ObjectFetchContext::getNullContext()).get());

  EXPECT_EQ(std::nullopt, preInode->getObjectId());
  EXPECT_TRUE(mount.getTreeInode("a")->isMaterialized());

  auto& stats = mount.getEdenMount()->getStats();
  stats->flush();
  auto data = facebook::fb303::ServiceData::get();
  constexpr folly::StringPiece key =
      "checkout.avoided_destination_conflicts.sum.60";
  auto initialAvoidedConflicts = data->getCounterIfExists(key).value_or(0);

  // Now checkout 2.

  auto result = mount
                    .checkout(
                        RootId{"2"},
                        ObjectFetchContext::getNullContext(),
                        __func__,
                        CheckoutMode::FORCE)
                    .getVia(executor);
  EXPECT_TRUE(result.conflicts.empty());

  stats->flush();
  EXPECT_EQ(
      initialAvoidedConflicts + 1, data->getCounterIfExists(key).value_or(0));

  // Checkout replaces the inode, so we need to look up the file again.

  EXPECT_FALSE(mount.getTreeInode("a")->isMaterialized());
  EXPECT_EQ(
      std::make_optional(ObjectId{"object2"}),
      mount.getFileInode("a/test.txt")->getObjectId());

  // The only inode should be unlinked!
  EXPECT_TRUE(preInode->isUnlinked());
}

TEST(Dematerialize, test_dematerialization_migrates_to_the_new_ID_scheme) {
  TestMount mount;
  auto backingStore = mount.getBackingStore();

  FakeTreeBuilder builder1;
  builder1.setFile(
      "foo/bar/file.txt"_relpath, "contents", false, ObjectId{"scheme 1"});
  auto* root1 = builder1.finalize(backingStore, /*setReady=*/true);

  FakeTreeBuilder builder2;
  builder2.setFile(
      "foo/bar/file.txt"_relpath, "contents", false, ObjectId{"scheme 2"});
  auto* root2 = builder2.finalize(backingStore, /*setReady=*/true);

  // The two trees should have different IDs, even if contents are identical.
  EXPECT_NE(root1->get().getObjectId(), root2->get().getObjectId());

  backingStore->putCommit("1", builder1)->setReady();
  backingStore->putCommit("2", builder2)->setReady();

  // Start the mount at commit 1 using the old scheme.
  mount.initialize(RootId{"1"});

  auto executor = mount.getServerExecutor().get();

  // We are testing dematerialization, so force the file to be materialized.
  // But don't change the contents.
  auto inode = mount.overwriteFile("foo/bar/file.txt", "contents");

  EXPECT_EQ(std::nullopt, inode->getObjectId());
  EXPECT_TRUE(mount.getTreeInode("foo")->isMaterialized());
  EXPECT_TRUE(mount.getTreeInode("foo/bar")->isMaterialized());

  // Now checkout 2.

  auto result =
      mount
          .checkout(RootId{"2"}, ObjectFetchContext::getNullContext(), __func__)
          .getVia(executor);

  // There should be no conflicts, as the file is not modified.
  EXPECT_EQ(0, result.conflicts.size());

  // Checkout replaces the inode, so we need to look up the file again.

  EXPECT_FALSE(mount.getTreeInode("foo")->isMaterialized());
  EXPECT_FALSE(mount.getTreeInode("foo/bar")->isMaterialized());
  EXPECT_EQ(
      std::make_optional(ObjectId{"scheme 2"}),
      mount.getFileInode("foo/bar/file.txt")->getObjectId());

  // The original inode should be unlinked!
  EXPECT_TRUE(inode->isUnlinked());
}

} // namespace
