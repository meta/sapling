/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <folly/Benchmark.h>
#include <folly/Range.h>
#include <folly/executors/GlobalExecutor.h>
#include <folly/init/Init.h>
#include <folly/logging/Logger.h>

#include <chrono>
#include <condition_variable>
#include <cstring>
#include <mutex>
#include <optional>

#include "eden/common/utils/ProcessInfoCache.h"
#include "eden/fs/fuse/FuseChannel.h"
#include "eden/fs/fuse/FuseDispatcher.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/testharness/FakeFuse.h"

using namespace std::chrono_literals;

namespace facebook::eden {
namespace {

constexpr auto kTimeout = 1s;
constexpr size_t kTraceBusCapacity = 25000;

const folly::Logger straceLogger{"eden.strace"};

FuseDispatcher::Attr makeAttr() {
  struct stat st = {};
  st.st_ino = FUSE_ROOT_ID;
  st.st_size = 123456;
  st.st_blocks = 248;
  st.st_atim = {1700000001, 123456789};
  st.st_mtim = {1700000002, 234567890};
  st.st_ctim = {1700000003, 345678901};
  st.st_mode = S_IFDIR | 0755;
  st.st_nlink = 2;
  st.st_uid = 1000;
  st.st_gid = 1000;
  st.st_rdev = 42;
  st.st_blksize = 4096;
  return FuseDispatcher::Attr{st, 3600};
}

class GetattrDispatcher final : public FuseDispatcher {
 public:
  GetattrDispatcher(EdenStatsPtr stats, bool suspend)
      : FuseDispatcher{std::move(stats)}, suspend_{suspend} {}

  ImmediateFuture<Attr> getattr(InodeNumber, const ObjectFetchContextPtr&)
      override {
    if (!suspend_) {
      return makeAttr();
    }

    auto future = ImmediateFuture<Attr>::makeEmpty();
    {
      std::lock_guard lock{mutex_};
      XCHECK(!pendingPromise_.has_value());
      pendingPromise_.emplace();
      future = pendingPromise_->getSemiFuture();
      XCHECK(!future.isReady());
    }
    requestReceived_.notify_one();
    return future;
  }

  folly::Promise<Attr> waitForRequest() {
    std::unique_lock lock{mutex_};
    XCHECK(requestReceived_.wait_for(
        lock, kTimeout, [&] { return pendingPromise_.has_value(); }));
    auto promise = std::move(*pendingPromise_);
    pendingPromise_.reset();
    return promise;
  }

  void forget(InodeNumber, unsigned long) override {
    {
      std::lock_guard lock{mutex_};
      ++markerCount_;
    }
    requestReceived_.notify_one();
  }

  void waitForMarker(uint64_t expectedCount) {
    std::unique_lock lock{mutex_};
    XCHECK(requestReceived_.wait_for(
        lock, kTimeout, [&] { return markerCount_ >= expectedCount; }));
    XCHECK_EQ(markerCount_, expectedCount);
  }

 private:
  bool suspend_;
  std::mutex mutex_;
  std::condition_variable requestReceived_;
  std::optional<folly::Promise<Attr>> pendingPromise_;
  uint64_t markerCount_{0};
};

class GetattrBenchmark {
 public:
  explicit GetattrBenchmark(bool suspend) : suspend_{suspend} {
    auto dispatcher =
        std::make_unique<GetattrDispatcher>(stats_.copy(), suspend);
    dispatcher_ = dispatcher.get();
    channel_ = makeFuseChannel(
        nullptr,
        fuse_.start(),
        canonicalPath("/fake/mount/path"),
        folly::getUnsafeMutableGlobalCPUExecutor(),
        1,
        std::move(dispatcher),
        &straceLogger,
        std::make_shared<ProcessInfoCache>(),
        nullptr,
        errorLogger_,
        60s,
        nullptr,
        true,
        12,
        1000,
        10min,
        5min,
        false,
        kTraceBusCapacity);

    auto initFuture = channel_->initialize();
    fuse_.sendInitRequest();
    auto response = fuse_.recvResponse();
    XCHECK_EQ(response.header.error, 0);
    stopFuture_.emplace(std::move(initFuture).get(kTimeout));
  }

  ~GetattrBenchmark() {
    fuse_.close();
    std::move(*stopFuture_).get(kTimeout);
    channel_.reset();
  }

  fuse_attr_out runOnce() {
    auto requestId =
        fuse_.sendRequest(FUSE_GETATTR, FUSE_ROOT_ID, folly::ByteRange{});
    if (suspend_) {
      auto promise = dispatcher_->waitForRequest();
      struct fuse_forget_in forget = {.nlookup = 1};
      fuse_.sendRequest(FUSE_FORGET, FUSE_ROOT_ID, forget);
      dispatcher_->waitForMarker(++expectedMarkerCount_);
      promise.setValue(makeAttr());
    }
    auto response = fuse_.recvResponse();
    XCHECK_EQ(response.header.unique, requestId);
    XCHECK_EQ(response.header.error, 0);
    XCHECK_EQ(
        response.header.len, sizeof(fuse_out_header) + sizeof(fuse_attr_out));
    XCHECK_EQ(response.body.size(), sizeof(fuse_attr_out));
    fuse_attr_out attr{};
    std::memcpy(&attr, response.body.data(), sizeof(attr));
    auto expected = makeAttr().asFuseAttr();
    auto actualBytes =
        folly::ByteRange{reinterpret_cast<const uint8_t*>(&attr), sizeof(attr)};
    auto expectedBytes = folly::ByteRange{
        reinterpret_cast<const uint8_t*>(&expected), sizeof(expected)};
    XCHECK_EQ(actualBytes, expectedBytes);
    return attr;
  }

 private:
  bool suspend_;
  uint64_t expectedMarkerCount_{0};
  FakeFuse fuse_;
  EdenStatsPtr stats_{makeRefPtr<EdenStats>()};
  ErrorLogger errorLogger_{nullptr, {}, nullptr};
  GetattrDispatcher* dispatcher_;
  std::unique_ptr<FuseChannel, FsChannelDeleter> channel_;
  std::optional<FuseChannel::StopFuture> stopFuture_;
};

void runGetattrBenchmark(unsigned int iters, bool suspend) {
  folly::BenchmarkSuspender suspender;
  GetattrBenchmark fixture{suspend};
  auto warmup = fixture.runOnce();
  folly::doNotOptimizeAway(warmup.attr.ino);
  suspender.dismiss();
  for (unsigned int i = 0; i < iters; ++i) {
    auto response = fixture.runOnce();
    folly::doNotOptimizeAway(response.attr.ino);
  }
  suspender.rehire();
}

} // namespace
} // namespace facebook::eden

BENCHMARK(fuse_getattr_immediate_future, iters) {
  facebook::eden::runGetattrBenchmark(iters, false);
}

BENCHMARK(fuse_getattr_suspended_future, iters) {
  facebook::eden::runGetattrBenchmark(iters, true);
}

int main(int argc, char** argv) {
  folly::Init init(&argc, &argv);
  folly::runBenchmarks();
  return 0;
}
