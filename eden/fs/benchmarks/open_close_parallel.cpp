/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <fcntl.h>
#include <folly/Exception.h>
#include <folly/Likely.h>
#include <folly/Utility.h>
#include <folly/init/Init.h>
#include <folly/portability/Unistd.h>
#include <gflags/gflags.h>
#include <inttypes.h>
#include <string.h>
#include <latch>
#include <limits>
#include <mutex>
#include <system_error>
#include <thread>
#include "eden/common/utils/benchharness/Bench.h"

DEFINE_uint64(threads, 1, "The number of concurrent open/close threads");
DEFINE_uint64(iterations, 100000, "Number of open/close iterations per thread");

using namespace facebook::eden;

int main(int argc, char** argv) {
  const folly::Init init(&argc, &argv);

  if (argc <= 1) {
    fprintf(
        stderr,
        "Specify a list of filenames on the command line. They will be opened "
        "in sequence.\n");
    return 1;
  }

  auto clock_overhead = measureClockOverhead();
  printf(
      "Clock overhead measured at %" PRIu64 " ns minimum, %" PRIu64
      " ns average\n",
      clock_overhead.getMinimum(),
      clock_overhead.getAverage());

  // Prefetch every specified file.
  for (int i = 1; i < argc; ++i) {
    auto filename = argv[i];
    int fd = ::open(filename, O_RDONLY);
    if (UNLIKELY(-1 == fd)) {
      perror(folly::to<std::string>("Failed to open '", filename, "'").c_str());
      return 1;
    }
    ::close(fd);
  }

  std::latch gate{folly::to_signed(FLAGS_threads)};

  std::mutex result_mutex;
  StatAccumulator combined_open;
  StatAccumulator combined_close;

  auto thread = [&] {
    StatAccumulator open_accum;
    StatAccumulator close_accum;
    int file_index = 1;

    gate.arrive_and_wait();

    for (uint64_t i = 0; i < FLAGS_iterations; ++i) {
      const char* filename = argv[file_index];

      uint64_t start_time = getTime();
      int fd = ::open(filename, O_RDONLY);
      uint64_t after_open = getTime();
      if (UNLIKELY(-1 == fd)) {
        folly::throwSystemError("Failed to open '", filename, "'");
      }
      ::close(fd);
      uint64_t after_close = getTime();

      if (++file_index >= argc) {
        file_index = 1;
      }

      open_accum.add(after_open - start_time);
      close_accum.add(after_close - after_open);
    }

    std::lock_guard guard{result_mutex};
    combined_open.combine(open_accum);
    combined_close.combine(close_accum);
  };

  std::vector<std::thread> threads;
  threads.reserve(FLAGS_threads);
  for (uint64_t t = 0; t < FLAGS_threads; ++t) {
    threads.emplace_back(thread);
  }

  for (auto& thrd : threads) {
    thrd.join();
  }

  printf(
      "open()\n  minimum: %" PRIu64 " ns\n  average: %" PRIu64 " ns\n",
      combined_open.getMinimum(),
      combined_open.getAverage());
  printf(
      "close()\n  minimum: %" PRIu64 " ns\n  average: %" PRIu64 " ns\n",
      combined_close.getMinimum(),
      combined_close.getAverage());
}
