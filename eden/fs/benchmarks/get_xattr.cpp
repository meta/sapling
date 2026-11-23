/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <iostream>
#include <numeric>
#include <stdexcept>
#include <thread>
#include <vector>

#ifndef _WIN32
#include <sys/xattr.h>
#endif

#include <benchmark/benchmark.h>
#include <boost/filesystem.hpp>
#include <fmt/format.h>

#include <folly/File.h>
#include <folly/Utility.h>
#include <folly/container/Array.h>
#include <folly/init/Init.h>
#include <folly/io/async/AsyncSocket.h>
#include <folly/logging/xlog.h>
#include <thrift/lib/cpp2/async/HeaderClientChannel.h>
#include <latch>

#include "eden/common/utils/XAttr.h"
#include "eden/common/utils/benchharness/Bench.h"
#include "eden/fs/service/gen-cpp2/EdenService.h"

// TODO: This benchmark needs major clean-up:
// 1. It should allow the user to specify a glob directly, and resolve the glob
// via Thrift or the filesystem. This avoids shell expansion which could
// possibly pass too many args.
// 2. It should allow all errors to be ignored, such as EISDIR and other special
// cases which are perfectly valid results for certain attributes.
// 3. It should allow you to specify whether you want the xattrs to be fetched
// for files, directories, or both.
// 4. The benchmark doesn't work if you try to query attributes via just 1
// endpoint (i.e. just thrift or just filesystem).

using namespace facebook::eden;
using namespace boost::filesystem;

DEFINE_int64(threads, 1, "The number of concurrent Thrift client threads");
DEFINE_string(repo, "", "Path to Eden repository");
DEFINE_string(
    interface,
    "",
    "Way to get xattrs. Options are: "
    "\"thrift\" (query EdenFS's thrift interface), "
    "\"filesystem\" (getxattr calls through the filesystem), or "
    "\"both\" (try both and display separate results).");
DEFINE_string(
    xattrType,
    kXattrSha1.data(),
    "Type of xattrs to request. Options are: "
    "\"user.sha1\" (sha1 of files), "
    "\"user.blake3\" (blake3 of files), "
    "\"user.digesthash\" (digest hash of file/dirs)");
DEFINE_bool(
    noAttrIsFatal,
    false,
    "Short-circuit the benchmark if any request fails with ENOATTR");

enum XAttrType {
  Sha1,
  Blake3,
  DigestHash,
};

std::string_view xAttrTypeToString(XAttrType xtype) {
  switch (xtype) {
    case XAttrType::Sha1: {
      return kXattrSha1;
    }
    case XAttrType::Blake3: {
      return kXattrBlake3;
    }
    case XAttrType::DigestHash: {
      return kXattrDigestHash;
    }
    default:
      throw std::invalid_argument(
          fmt::format(
              "invalid xattr type selected: {}", fmt::underlying(xtype)));
  }
}

XAttrType stringToXAttrType(std::string_view str) {
  if (str == kXattrSha1) {
    return XAttrType::Sha1;
  } else if (str == kXattrBlake3) {
    return XAttrType::Blake3;
  } else if (str == kXattrDigestHash) {
    return XAttrType::DigestHash;
  } else {
    throw std::invalid_argument(
        fmt::format("cannot convert {} to valid XAttrType", str));
  }
}

size_t getXAttrTypeSize(XAttrType xtype) {
  switch (xtype) {
    case XAttrType::Sha1:
      return 40;
    case XAttrType::Blake3:
    case XAttrType::DigestHash:
      return 64;
    default:
      throw std::invalid_argument(
          fmt::format(
              "invalid xattr type selected: {}", fmt::underlying(xtype)));
  }
}

bool shouldRecordThriftSamples(std::string& interface) {
  return interface == "both" || interface == "thrift";
}

bool shouldRecordFilesystemSamples(std::string& interface) {
  return interface == "both" || interface == "filesystem";
}

template <typename ThriftResultT>
bool isThriftResultFatal(const ThriftResultT& res) {
  // Result is only fatal iff:
  // 1) The result is an error type
  // 2) The error is not ATTRIBUTE_UNAVAILABLE, or the user specified that
  //    ATTRIBUTE_UNAVAILABLE errors should be fatal
  if (res.getType() == ThriftResultT::Type::error) {
    // Always log that an error occurred
    XLOGF(DBG3, "Thrift request failed with: {}", res.error().value().what());

    // The error should only be fatal in some cases
    return res.error()->errorType().value() !=
        EdenErrorType::ATTRIBUTE_UNAVAILABLE ||
        FLAGS_noAttrIsFatal;
  }
  return false;
}

/**
 * Record a sample in `samples` of how long it takes to read a file's xattr
 * from EdenFS's thrift interface.
 */
template <typename ThriftResultT>
void recordThriftSample(
    std::string& file,
    boost::filesystem::path& repo_path,
    std::unique_ptr<EdenServiceAsyncClient>& client,
    void (EdenServiceAsyncClient::*method)(
        std::vector<ThriftResultT>&,
        const PathString&,
        const std::vector<PathString>&,
        const SyncBehavior&),
    uint64_t& sample) {
  auto start = getTime();
  std::vector<ThriftResultT> res;
  // see notes in recordFilesystemSample about these DoNotOptimize protecting
  // ordering here
  benchmark::DoNotOptimize(file);
  auto sync = SyncBehavior{};
  (client.get()->*method)(res, repo_path.native(), {file}, sync);
  benchmark::DoNotOptimize(res);
  auto duration = std::chrono::nanoseconds(getTime() - start);
  sample =
      std::chrono::duration_cast<std::chrono::microseconds>(duration).count();
  if (UNLIKELY(res.empty())) {
    throw std::runtime_error("No results!");
  }
  if (UNLIKELY(res.size() != 1 || isThriftResultFatal(res.at(0)))) {
    throw res.at(0).error().as_const().value();
  }
}

/**
 * Record a sample in `samples` of how long it takes to read a file's xattr
 * using a call through the filesystem (getxattr).
 */
void recordFilesystemSample(
    std::string& file,
    uint64_t& sample,
    std::string& xattr_type,
    size_t xattr_type_size) {
  std::vector<char> res{};
  res.reserve(xattr_type_size);
  auto start = getTime();
  // The DoNotOptimize calls "fence" in the xattr access and make it reliable to
  // time:
  // https://stackoverflow.com/questions/37786547/enforcing-statement-order-in-c
  benchmark::DoNotOptimize(file); // this is just used so that the ordering of
                                  // start time and the xattr access can not be
                                  // reordered by the compiler
  ssize_t success{0};
#ifdef __APPLE__
  success = getxattr(
      file.c_str(), xattr_type.c_str(), res.data(), xattr_type_size, 0, 0);
#elif __linux__
  success =
      getxattr(file.c_str(), xattr_type.c_str(), res.data(), xattr_type_size);
#endif
  benchmark::DoNotOptimize(res); // this is just used so that the ordering of
                                 // recording the end time and the xattr access
                                 // can not be reordered by the compiler
  auto duration = std::chrono::nanoseconds(getTime() - start);
  sample =
      std::chrono::duration_cast<std::chrono::microseconds>(duration).count();

  if (UNLIKELY(success == -1)) {
    XLOGF(
        DBG3,
        "failed to get xattr for file '{}': {} - {}\n",
        file,
        errno,
        folly::errnoStr(errno));
    if (errno != kENOATTR || FLAGS_noAttrIsFatal) {
      throw std::system_error{std::error_code{errno, std::system_category()}};
    }
  }
}

/**
 * Calculate some standard statistics for the given samples and display them.
 */
void calculateStats(
    std::vector<uint64_t>& samples,
    unsigned nthreads,
    unsigned samples_per_thread) {
  if (UNLIKELY(samples.empty())) {
    throw std::runtime_error("No samples to calculate stats for!");
  }
  std::sort(samples.begin(), samples.end());
  double avg =
      std::accumulate(samples.begin(), samples.end(), 0.0) / samples.size();
  fmt::print("avg: {:6f} us\n", avg);
  fmt::print("min: {} us\n", samples.at(0));
  const unsigned nsamples = samples_per_thread * nthreads;
  std::array<unsigned, 3> pcts = folly::make_array<unsigned>(5, 50, 95);
  for (const auto& p : pcts) {
    fmt::print(
        "p{}: {} us\n",
        static_cast<uint64_t>(p),
        samples.at(p * nsamples / 100));
  }
}

int main(int argc, char** argv) {
  const folly::Init init(&argc, &argv);

  if (!FLAGS_threads) {
    std::cerr << "Must specify nonzero number of threads" << std::endl;
    gflags::ShowUsageWithFlags(argv[0]);
    return 1;
  }
  if (FLAGS_repo.empty()) {
    std::cerr << "Must specify a repository root" << std::endl;
    gflags::ShowUsageWithFlagsRestrict(argv[0], __FILE__);
    return 1;
  }

  auto xAttrType = XAttrType::Sha1;
  try {
    xAttrType = stringToXAttrType(FLAGS_xattrType);
  } catch (std::invalid_argument& e) {
    std::cerr << "Must specify a valid xattr type: " << e.what() << std::endl;
    gflags::ShowUsageWithFlagsRestrict(argv[0], __FILE__);
    return 1;
  }

  auto real_path = realpath(FLAGS_repo.c_str(), nullptr);
  if (!real_path) {
    perror("realpath on given repo failed");
    return 1;
  }
  SCOPE_EXIT {
    free(real_path);
  };
  path repo_path = real_path;

  if ((argc - 1) < FLAGS_threads) {
    std::cerr
        << "Must specify a set of files to query, at least one per thread."
        << " files to query: " << argc - 1
        << " threads to run: " << FLAGS_threads << std::endl;
    return 1;
  }

  if ((argc - 1) % FLAGS_threads != 0) {
    std::cerr << "Each thread needs the same number of files to sample."
              << " files to query: " << argc - 1
              << " threads to run: " << FLAGS_threads << std::endl;
    return 1;
  }

#ifdef _WIN32
  if (shouldRecordFilesystemSamples(FLAGS_interface)) {
    std::cerr << "Filesystem xattr not currently supported" << std::endl;
    return 1;
  }
#endif

  std::vector<std::string> thrift_files;
  std::vector<std::string> filesystem_files;
  for (int i = 1; i < argc; ++i) {
    if (shouldRecordThriftSamples(FLAGS_interface)) {
      thrift_files.emplace_back(argv[i]);
    }
    if (shouldRecordFilesystemSamples(FLAGS_interface)) {
      filesystem_files.emplace_back((repo_path / argv[i]).native());
    }
  }

  const auto socket_path = repo_path / ".eden" / "socket";
  const size_t nthreads = FLAGS_threads;
  // This number should stay a power of two, to avoid cache line ping-ponging
  // on the boundaries.
  const unsigned samples_per_thread = 8192;

  std::vector<std::thread> threads;
  std::latch gate{folly::to_signed(nthreads)};
  std::vector<uint64_t> thrift_samples(nthreads * samples_per_thread);
  std::vector<uint64_t> filesystem_samples(nthreads * samples_per_thread);
  // we are just going to do calculations based on the thrift_files size, so
  // let's assert the assumption that they have the same size
  XCHECK_EQ(
      thrift_files.size(),
      filesystem_files.size(),
      "thrift and filesystem number of files must be the same");
  for (unsigned thread_number = 0; thread_number < nthreads; ++thread_number) {
    threads.emplace_back([thread_number,
                          &gate,
                          &socket_path,
                          &repo_path,
                          &thrift_samples,
                          &filesystem_samples,
                          &thrift_files,
                          &filesystem_files,
                          &interface = FLAGS_interface,
                          &xAttrType] {
      // The order of these variables matters, the client MUST be
      // destroyed before the event base because the client
      // destructor is gonna touch the eventbase.
      folly::EventBase eventBase;
      std::unique_ptr<EdenServiceAsyncClient> client;
      if (shouldRecordThriftSamples(interface)) {
        auto socket = folly::AsyncSocket::newSocket(
            &eventBase,
            folly::SocketAddress::makeFromPath(
                socket_path.string<std::string>()));
        auto channel =
            apache::thrift::HeaderClientChannel::newChannel(std::move(socket));
        client = std::make_unique<EdenServiceAsyncClient>(std::move(channel));
      }

      gate.arrive_and_wait();
      auto xattr_type_size = getXAttrTypeSize(xAttrType);
      for (unsigned j = 0; j < samples_per_thread; ++j) {
        auto samples_index = thread_number * samples_per_thread + j;
        auto files_index = samples_index % thrift_files.size();
        if (shouldRecordThriftSamples(interface)) {
          switch (xAttrType) {
            case XAttrType::Sha1: {
              recordThriftSample<SHA1Result>(
                  thrift_files[files_index],
                  repo_path,
                  client,
                  &EdenServiceAsyncClient::sync_getSHA1,
                  thrift_samples[samples_index]);
              break;
            }
            case XAttrType::Blake3: {
              recordThriftSample<Blake3Result>(
                  thrift_files[files_index],
                  repo_path,
                  client,
                  &EdenServiceAsyncClient::sync_getBlake3,
                  thrift_samples[samples_index]);
              break;
            }
            case XAttrType::DigestHash: {
              recordThriftSample<DigestHashResult>(
                  thrift_files[files_index],
                  repo_path,
                  client,
                  &EdenServiceAsyncClient::sync_getDigestHash,
                  thrift_samples[samples_index]);
              break;
            }
            default:
              throw std::invalid_argument(
                  fmt::format(
                      "cannot fetch unknown attr via thrift: {}",
                      fmt::underlying(xAttrType)));
          }
        }

        if (shouldRecordFilesystemSamples(interface)) {
          recordFilesystemSample(
              filesystem_files[files_index],
              filesystem_samples[samples_index],
              FLAGS_xattrType,
              xattr_type_size);
        }
      }
    });
  }

  for (auto& thread : threads) {
    thread.join();
  }

  if (shouldRecordThriftSamples(FLAGS_interface)) {
    std::cout << "Thrift Statistics: " << std::endl;
    calculateStats(thrift_samples, nthreads, samples_per_thread);
    std::cout << std::endl;
  }

  if (shouldRecordFilesystemSamples(FLAGS_interface)) {
    std::cout << "Filesystem Statistics: " << std::endl;
    calculateStats(filesystem_samples, nthreads, samples_per_thread);
    std::cout << std::endl;
  }

  return 0;
}
