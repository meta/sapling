/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

/*
 * CheckoutBenchmark — end-to-end microbenchmarks for `EdenMount::checkout`.
 *
 * Compares the futures path (`coroutines:enable-phase7=false`, default)
 * with the coroutine path (`enableCoroutinesConfig` flips on phase7
 * along with phases 2/3/5 — see `TestMount.h`).
 *
 * Workload: a paired `FakeBackingStore` tree with one file modified
 * between commits. Variants control the materialization rate of leaf
 * files in the working copy (`overwriteFile` materializes a file
 * inode), exercising different paths inside `co_checkoutUpdateEntry`:
 *
 *   - 0% materialized: load-only fast path
 *   - 50% materialized: mixed (rename-lock + invalidate paths
 *                       exercised on materialized half)
 *   - 100% materialized: rename-lock + invalidate on every entry
 *
 * Plus a deep-recursion variant (depth=4, fanout=3) that is the most
 * sensitive to per-recursion coroutine overhead.
 *
 * Use `BENCHMARK_RELATIVE` so the Coroutines run reports its result as
 * a percentage of the corresponding Futures run — the C/F ratio is the
 * headline.
 *
 * Setup work (building the second commit, materializing files) runs
 * inside `BENCHMARK_SUSPEND { ... }` so it's excluded from the timed
 * section. Only the `EdenMount::checkout(...)` call itself is timed.
 */

#include <folly/Benchmark.h>
#include <folly/init/Init.h>

#include <fmt/format.h>

#include "eden/common/utils/PathFuncs.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/model/RootId.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/store/IObjectStore.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/testharness/FakeBackingStore.h"
#include "eden/fs/testharness/FakeTreeBuilder.h"
#include "eden/fs/testharness/TestMount.h"

using namespace facebook::eden;

namespace {

/**
 * Build a balanced tree of `(depth, fanout)` shape with `<depth>` levels
 * of `<fanout>` directories per level, each containing `<filesPerDir>`
 * files. Returns a pair: the FakeTreeBuilder configured (not yet
 * finalized) and a flat list of all leaf-file paths (used to drive
 * materialization).
 *
 * Total file count = filesPerDir * (1 + fanout + fanout^2 + ...).
 */
std::pair<FakeTreeBuilder, std::vector<std::string>> buildTreeShape(
    int depth,
    int fanout,
    int filesPerDir,
    folly::StringPiece marker = "v1") {
  FakeTreeBuilder builder;
  std::vector<std::string> paths;

  std::function<void(std::string, int)> emitDir = [&](std::string prefix,
                                                      int remainingDepth) {
    for (int f = 0; f < filesPerDir; ++f) {
      auto path = fmt::format("{}/file_{}.txt", prefix, f);
      auto contents = fmt::format("{} contents of {}\n", marker, path);
      builder.setFile(path, contents);
      paths.push_back(std::move(path));
    }
    if (remainingDepth > 0) {
      for (int d = 0; d < fanout; ++d) {
        emitDir(fmt::format("{}/dir_{}", prefix, d), remainingDepth - 1);
      }
    }
  };

  emitDir("root", depth);
  return {std::move(builder), std::move(paths)};
}

/**
 * Materialize the requested fraction of leaf files in `mount` by
 * overwriting them via the dispatcher API (which allocates an overlay
 * entry).
 */
void materializeFraction(
    TestMount& mount,
    const std::vector<std::string>& allPaths,
    double fraction) {
  if (fraction <= 0.0) {
    return;
  }
  size_t toMaterialize =
      static_cast<size_t>(static_cast<double>(allPaths.size()) * fraction);
  size_t step =
      std::max<size_t>(1, allPaths.size() / std::max<size_t>(1, toMaterialize));
  size_t materialized = 0;
  for (size_t i = 0; i < allPaths.size() && materialized < toMaterialize;
       i += step) {
    mount.overwriteFile(allPaths[i], "materialized\n");
    ++materialized;
  }
}

/**
 * Run a single checkout from "1" to "2" against the supplied mount.
 * Returns when the checkout completes (drains the executor inline).
 */
void runOneCheckout(TestMount& mount, RootId target) {
  auto executor = mount.getServerExecutor().get();
  auto fut = mount
                 .checkout(
                     target,
                     ObjectFetchContext::getNullContext(),
                     "checkout_benchmark")
                 .waitVia(executor);
  auto result = std::move(fut).get();
  folly::doNotOptimizeAway(result);
}

/**
 * One iteration: build trees, materialize, run checkout. The setup is
 * suspended (excluded from the timing) so the timed portion is just
 * the `checkout(...)` call.
 */
template <bool UseCoroutines>
void runCheckoutBenchmark(
    unsigned iters,
    int depth,
    int fanout,
    int filesPerDir,
    double materializationRate) {
  for (unsigned i = 0; i < iters; ++i) {
    folly::BenchmarkSuspender suspender;

    auto [builder1, paths] = buildTreeShape(depth, fanout, filesPerDir, "v1");
    TestMount mount{builder1};
    if constexpr (UseCoroutines) {
      enableCoroutinesConfig(mount);
    }
    materializeFraction(mount, paths, materializationRate);

    // Build the destination commit: same shape, contents marked "v2".
    auto [builder2, _] = buildTreeShape(depth, fanout, filesPerDir, "v2");
    builder2.finalize(mount.getBackingStore(), /*setReady=*/true);
    auto commit2 = mount.getBackingStore()->putCommit(RootId{"2"}, builder2);
    commit2->setReady();

    suspender.dismiss(); // Begin timing.
    runOneCheckout(mount, RootId{"2"});
  }
}

} // namespace

// =====================================================================
// Variant 1: small tree, 0% materialized (load-only fast path)
// depth=0 → 5 files in root, no subdirs.
// =====================================================================
BENCHMARK(checkout_unmat_5x5_futures, iters) {
  runCheckoutBenchmark</*UseCoroutines=*/false>(
      iters,
      /*depth=*/0,
      /*fanout=*/5,
      /*filesPerDir=*/5,
      /*materializationRate=*/0.0);
}
BENCHMARK_RELATIVE(checkout_unmat_5x5_coro, iters) {
  runCheckoutBenchmark</*UseCoroutines=*/true>(iters, 0, 5, 5, 0.0);
}

// =====================================================================
// Variant 2: small tree, 50% materialized (mixed path)
// =====================================================================
BENCHMARK(checkout_50pct_5x5_futures, iters) {
  runCheckoutBenchmark</*UseCoroutines=*/false>(iters, 0, 5, 5, 0.5);
}
BENCHMARK_RELATIVE(checkout_50pct_5x5_coro, iters) {
  runCheckoutBenchmark</*UseCoroutines=*/true>(iters, 0, 5, 5, 0.5);
}

// =====================================================================
// Variant 3: small tree, 100% materialized (rename-lock + invalidate
// on every entry)
// =====================================================================
BENCHMARK(checkout_full_5x5_futures, iters) {
  runCheckoutBenchmark</*UseCoroutines=*/false>(iters, 0, 5, 5, 1.0);
}
BENCHMARK_RELATIVE(checkout_full_5x5_coro, iters) {
  runCheckoutBenchmark</*UseCoroutines=*/true>(iters, 0, 5, 5, 1.0);
}

// =====================================================================
// Variant 4: deep recursion (depth=4, fanout=3, 3 files/dir).
// Total ~243 directories × 3 files plus interior files = ~1000 entries.
// 50% materialized — sensitive to per-recursion coroutine overhead.
// =====================================================================
BENCHMARK(checkout_deep_d4f3_futures, iters) {
  runCheckoutBenchmark</*UseCoroutines=*/false>(
      iters, /*depth=*/4, /*fanout=*/3, /*filesPerDir=*/3, 0.5);
}
BENCHMARK_RELATIVE(checkout_deep_d4f3_coro, iters) {
  runCheckoutBenchmark</*UseCoroutines=*/true>(iters, 4, 3, 3, 0.5);
}

int main(int argc, char** argv) {
  folly::Init init(&argc, &argv);
  folly::runBenchmarks();
  return 0;
}
