/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <folly/Try.h>
#include <gtest/gtest.h>
#include <memory>

#include "eden/common/utils/CaseSensitivity.h"
#include "eden/fs/model/ObjectId.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/service/ThriftGetObjectImpl.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/TestMount.h"

namespace facebook::eden {

TEST(ThriftGetObjectImplTest, emptyTreeFromOrigin) {
  FakeTreeBuilder builder;
  builder.setFile("a", "x");
  TestMount mount{builder};
  auto id = ObjectId::fromHex("0000000000000000000000000000000000000001");
  auto emptyTree = std::make_shared<const Tree>(
      Tree::container{kPathMapDefaultCaseSensitive}, id);

  auto result = transformToTreeFromOrigin(
      mount.getEdenMount(),
      id,
      folly::Try<std::shared_ptr<const Tree>>{emptyTree},
      DataFetchOrigin::MEMORY_CACHE);

  EXPECT_EQ(ScmTreeOrError::Type::treeEntries, result.scmTreeData()->getType());
  EXPECT_TRUE(result.scmTreeData()->treeEntries()->empty());
}

} // namespace facebook::eden
