/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once
#include <type_traits>

#include <folly/ExceptionWrapper.h>
#include <folly/FBVector.h>
#include <folly/MapUtil.h>
#include <folly/coro/Collect.h>
#include <folly/coro/Invoke.h>
#include <folly/coro/safe/NowTask.h>
#include <folly/functional/Invoke.h>
#include <folly/futures/Future.h>

#include <folly/coro/ViaIfAsync.h>
#include "eden/common/utils/CaseSensitivity.h"
#include "eden/common/utils/PathMap.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/inodes/VirtualInode.h"
#include "eden/fs/store/ObjectFetchContext.h"

namespace facebook::eden {

namespace detail {

/** InodeLoader is a helper class for minimizing the number
 * of inode load calls that we need to emit when loading a list
 * of paths.
 */
class VirtualInodeLoader {
 public:
  VirtualInodeLoader() = default;

  // Arrange to load the inode for the input path (coroutine interface).
  // Returns a pointer to this node so the caller can read the result
  // after co_loaded() completes.
  VirtualInodeLoader* co_load(RelativePathPiece path) {
    VirtualInodeLoader* parent = this;
    for (auto name : path.components()) {
      parent = parent->getOrCreateChild(name);
    }
    return parent;
  }

  // Recursively resolves all children via co_getOrFindChild, storing results
  // directly in each node. Children are resolved in parallel via
  // collectAllRange.
  folly::coro::now_task<void> co_loaded(
      folly::Try<VirtualInode> inodeTry,
      RelativePathPiece path,
      const std::shared_ptr<ObjectStore>& store,
      const ObjectFetchContextPtr& fetchContext) {
    result_ = inodeTry;

    auto isTree = inodeTry.hasValue() ? inodeTry->isDirectory() : false;

    std::vector<folly::coro::Task<void>> childTasks;
    childTasks.reserve(children_.size());
    for (auto& [childName, childLoader] : children_) {
      childTasks.push_back(
          folly::coro::co_invoke(
              [loader = childLoader.get(),
               &name = childName,
               childPath = path + childName,
               &inodeTry,
               isTree,
               store,
               fetchContext =
                   fetchContext.copy()]() -> folly::coro::Task<void> {
                if (inodeTry.hasException()) {
                  co_await loader->co_loaded(
                      inodeTry, childPath, store, fetchContext);
                } else if (!isTree) {
                  co_await loader->co_loaded(
                      folly::Try<VirtualInode>(
                          folly::make_exception_wrapper<std::system_error>(
                              ENOENT, std::generic_category())),
                      childPath,
                      store,
                      fetchContext);
                } else {
                  auto childInodeTry = co_await folly::coro::co_awaitTry(
                      inodeTry.value().co_getOrFindChild(
                          name, childPath, store, fetchContext));
                  co_await loader->co_loaded(
                      std::move(childInodeTry), childPath, store, fetchContext);
                }
              }));
    }
    co_await folly::coro::collectAllRange(std::move(childTasks));
  }

  // Access the resolved result after co_loaded() completes.
  const folly::Try<VirtualInode>& result() const {
    return result_;
  }

 private:
  // Any child nodes that we need to load.  We have to use a unique_ptr
  // for this to avoid creating a self-referential type and fail to
  // compile.  This happens to have the nice property of maintaining
  // a stable address for the contents of the VirtualInodeLoader.
  PathMap<std::unique_ptr<VirtualInodeLoader>> children_{
      CaseSensitivity::Sensitive};
  // resolved result, populated by co_loaded
  folly::Try<VirtualInode> result_;

  // Helper for building out the plan during parsing
  VirtualInodeLoader* getOrCreateChild(PathComponentPiece name) {
    auto child = folly::get_ptr(children_, name);
    if (child) {
      return child->get();
    }
    auto ret = children_.emplace(name, std::make_unique<VirtualInodeLoader>());
    return ret.first->second.get();
  }
};

} // namespace detail

// Result type for co_applyToVirtualInode: extracts the inner type from Func's
// co_awaitable return type (SemiFuture<T>, now_task<T>, etc.).
// Uses semi_await_result_t so non-awaitable return types produce a compile
// error.
template <typename Func>
using VirtualInodeResult = folly::coro::semi_await_result_t<
    std::invoke_result_t<Func&, VirtualInode, RelativePath>>;

/**
 * Given a `rootInode` and a list of `paths` relative to that root, load the
 * VirtualInode for each and apply `func` to it.
 *
 * The load builds a tree-shaped plan so the number of inode loads is
 * O(number-of-unique-objects) rather than O(number-of-path-components).
 * Index 0 of the results corresponds to `paths[0]`, and so on; a path that
 * fails to parse or load yields an exception entry in its slot, so the caller
 * gets exactly one record per input path. func may return anything
 * co_awaitable (SemiFuture<T>, now_task<T>, etc.).
 *
 * Both inode resolution and func application are parallelized: resolution
 * via the loader's tree-shaped plan, func via collectAllRange.
 *
 * CONTRACT: The result of invoking func is immediately co_awaited within the
 * same expression via co_awaitTry. This is critical when func returns
 * now_task<T>: now_task requires immediate awaiting to guarantee reference
 * safety for captured references. Do not store, move, or defer the result of
 * func.
 */
template <typename Func>
folly::coro::now_task<std::vector<folly::Try<VirtualInodeResult<Func>>>>
co_applyToVirtualInode(
    InodePtr rootInode,
    const std::vector<std::string>& paths,
    Func func,
    const std::shared_ptr<ObjectStore>& store,
    const ObjectFetchContextPtr& fetchContext) {
  using Result = VirtualInodeResult<Func>;

  detail::VirtualInodeLoader loader;

  // Func may not be copyable, so wrap it in a shared_ptr.
  auto cb = std::make_shared<Func>(std::move(func));

  // Build the load plan. Each entry is either a pointer to a loader node
  // (for valid paths) or an exception (for invalid paths).
  struct PathEntry {
    detail::VirtualInodeLoader* node{nullptr};
    RelativePath path;
    folly::exception_wrapper error;
  };
  std::vector<PathEntry> entries;
  entries.reserve(paths.size());
  for (const auto& path : paths) {
    PathEntry entry;
    try {
      auto relPath = RelativePathPiece{path};
      entry.node = loader.co_load(relPath);
      entry.path = relPath.copy();
    } catch (const std::exception&) {
      entry.error = folly::exception_wrapper(std::current_exception());
    }
    entries.push_back(std::move(entry));
  }

  // Resolve all inodes via the coroutine loader.
  co_await loader.co_loaded(
      folly::Try<VirtualInode>(VirtualInode{std::move(rootInode)}),
      RelativePath(),
      store,
      fetchContext);

  // Apply func to each resolved inode in parallel.
  std::vector<folly::coro::Task<folly::Try<Result>>> tasks;
  tasks.reserve(entries.size());
  for (auto& entry : entries) {
    tasks.push_back(
        folly::coro::co_invoke(
            [cb,
             node = entry.node,
             path = std::move(entry.path),
             error = std::move(entry.error)]() mutable
                -> folly::coro::Task<folly::Try<Result>> {
              if (error) {
                co_return folly::Try<Result>(std::move(error));
              }
              auto& inodeTry = node->result();
              if (inodeTry.hasException()) {
                co_return folly::Try<Result>(inodeTry.exception());
              }
              co_return co_await folly::coro::co_awaitTry(
                  (*cb)(inodeTry.value(), std::move(path)));
            }));
  }

  co_return co_await folly::coro::collectAllRange(std::move(tasks));
}

} // namespace facebook::eden
