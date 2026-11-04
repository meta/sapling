/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <fcntl.h>
#include <folly/init/Init.h>
#include <folly/portability/Unistd.h>
#include <gflags/gflags.h>
#include <inttypes.h>
#include <algorithm>
#include <atomic>
#include <chrono>
#include <fstream>
#include <string>
#include <thread>
#include <vector>

DEFINE_string(
    file_list,
    "",
    "File with one path per line. Each pass opens, reads to the end and closes "
    "every path once.");
DEFINE_uint64(threads, 64, "Number of reader threads");
DEFINE_uint64(passes, 1, "Number of passes over the file list");
DEFINE_uint64(
    wait_between_passes_seconds,
    0,
    "Seconds to sleep before each pass after the first, for example to let "
    "the kernel's FUSE entry and attribute caches expire without dropping "
    "them");

namespace {
constexpr size_t kBatch = 64;
constexpr size_t kBufferSize = 1 << 20;
} // namespace

int main(int argc, char** argv) {
  const folly::Init init(&argc, &argv);

  if (FLAGS_file_list.empty() || FLAGS_threads == 0) {
    fprintf(stderr, "Specify --file_list and a nonzero --threads.\n");
    return 1;
  }
  std::vector<std::string> paths;
  {
    std::ifstream in{FLAGS_file_list};
    for (std::string line; std::getline(in, line);) {
      if (!line.empty()) {
        paths.push_back(std::move(line));
      }
    }
  }
  if (paths.empty()) {
    fprintf(stderr, "No paths in '%s'.\n", FLAGS_file_list.c_str());
    return 1;
  }

  for (uint64_t pass = 0; pass < FLAGS_passes; ++pass) {
    if (pass > 0 && FLAGS_wait_between_passes_seconds > 0) {
      std::this_thread::sleep_for(
          std::chrono::seconds{FLAGS_wait_between_passes_seconds});
    }

    std::atomic<size_t> next{0};
    std::atomic<uint64_t> files{0};
    std::atomic<uint64_t> bytes{0};
    std::atomic<uint64_t> failures{0};
    auto reader = [&] {
      std::vector<char> buffer(kBufferSize);
      uint64_t myFiles = 0;
      uint64_t myBytes = 0;
      uint64_t myFailures = 0;
      for (;;) {
        size_t begin = next.fetch_add(kBatch, std::memory_order_relaxed);
        if (begin >= paths.size()) {
          break;
        }
        size_t end = std::min(begin + kBatch, paths.size());
        for (size_t i = begin; i < end; ++i) {
          int fd = ::open(paths[i].c_str(), O_RDONLY | O_CLOEXEC);
          if (fd < 0) {
            ++myFailures;
            continue;
          }
          ssize_t n;
          while ((n = ::read(fd, buffer.data(), buffer.size())) > 0) {
            myBytes += n;
          }
          if (n < 0) {
            ++myFailures;
          } else {
            ++myFiles;
          }
          ::close(fd);
        }
      }
      files += myFiles;
      bytes += myBytes;
      failures += myFailures;
    };

    auto start = std::chrono::steady_clock::now();
    std::vector<std::thread> threads;
    threads.reserve(FLAGS_threads);
    for (uint64_t t = 0; t < FLAGS_threads; ++t) {
      threads.emplace_back(reader);
    }
    for (auto& thread : threads) {
      thread.join();
    }
    double seconds =
        std::chrono::duration<double>(std::chrono::steady_clock::now() - start)
            .count();

    printf(
        "pass %" PRIu64 ": %" PRIu64
        " files, %.1f MB in %.2f s: %.0f files/s, "
        "%.1f MB/s, %" PRIu64 " failed\n",
        pass + 1,
        files.load(),
        bytes.load() / 1e6,
        seconds,
        files.load() / seconds,
        bytes.load() / 1e6 / seconds,
        failures.load());
  }
  return 0;
}
