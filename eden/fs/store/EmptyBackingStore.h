/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/coro/Task.h>
#include <folly/coro/safe/NowTask.h>
#include "eden/fs/model/RootId.h"
#include "eden/fs/store/BackingStore.h"
#include "eden/fs/store/ObjectFetchContext.h"

namespace facebook::eden {

/*
 * A dummy BackingStore implementation, that always throws std::domain_error
 * for any ID that is looked up.
 */
class EmptyBackingStore final : public BijectiveBackingStore {
 public:
  EmptyBackingStore();
  ~EmptyBackingStore() override;

  RootId parseRootId(folly::StringPiece rootId) override;
  std::string renderRootId(const RootId& rootId) override;
  ObjectId parseObjectId(folly::StringPiece objectId) override;
  std::string renderObjectId(const ObjectId& objectId) override;

  // TODO(T119221752): Implement for all BackingStore subclasses
  int64_t dropAllPendingRequestsFromQueue() override {
    XLOG(
        WARN,
        "dropAllPendingRequestsFromQueue() is not implemented for EmptyBackingStore");
    return 0;
  }

 private:
  folly::coro::now_task<GetRootTreeResult> co_getRootTree(
      const RootId& /* rootId */,
      const ObjectFetchContextPtr& /* context */) override {
    throw std::domain_error("empty backing store");
  }
  ImmediateFuture<std::shared_ptr<TreeEntry>> getTreeEntryForObjectId(
      const ObjectId& /* objectId */,
      TreeEntryType /* treeEntryType */,
      const ObjectFetchContextPtr& /* context */) override {
    throw std::domain_error("unimplemented");
  }
  folly::SemiFuture<GetTreeResult> getTree(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;
  folly::coro::now_task<GetTreeResult> co_getTree(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;
  folly::coro::now_task<GetTreeAuxResult> co_getTreeAuxData(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;
  folly::SemiFuture<GetBlobResult> getBlob(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;
  folly::coro::Task<GetBlobResult> co_getBlob(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;
  folly::coro::now_task<GetBlobAuxResult> co_getBlobAuxData(
      const ObjectId& id,
      const ObjectFetchContextPtr& context) override;
};

} // namespace facebook::eden
