/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/utils/ProcessAccessLog.h"

#include <benchmark/benchmark.h>
#include <memory>
#include <vector>

#include "eden/common/utils/ProcessInfoCache.h"

using namespace facebook::eden;

struct ProcessAccessLogFixture : benchmark::Fixture {
  std::shared_ptr<ProcessInfoCache> processInfoCache{
      std::make_shared<ProcessInfoCache>()};
  ProcessAccessLog processAccessLog{processInfoCache};
};

/**
 * A high but realistic amount of contention.
 */
constexpr size_t kThreadCount = 4;

BENCHMARK_DEFINE_F(ProcessAccessLogFixture, add_self)(benchmark::State& state) {
  auto myPid = getpid();
  for (auto _ : state) {
    processAccessLog.recordAccess(
        myPid, ProcessAccessLog::AccessType::FsChannelOther);
  }
}

BENCHMARK_REGISTER_F(ProcessAccessLogFixture, add_self)->Threads(kThreadCount);

/**
 * The FS channel thread pool is shared by all mounts, so one thread records
 * into one log per mount.
 */
constexpr size_t kLogCount = 8;

struct ManyProcessAccessLogsFixture : benchmark::Fixture {
  std::shared_ptr<ProcessInfoCache> processInfoCache{
      std::make_shared<ProcessInfoCache>()};
  std::vector<std::unique_ptr<ProcessAccessLog>> logs = [&] {
    std::vector<std::unique_ptr<ProcessAccessLog>> result;
    result.reserve(kLogCount);
    for (size_t i = 0; i < kLogCount; ++i) {
      result.push_back(std::make_unique<ProcessAccessLog>(processInfoCache));
    }
    return result;
  }();
};

BENCHMARK_DEFINE_F(ManyProcessAccessLogsFixture, add_self_round_robin)
(benchmark::State& state) {
  auto myPid = getpid();
  size_t i = 0;
  for (auto _ : state) {
    logs[i++ % kLogCount]->recordAccess(
        myPid, ProcessAccessLog::AccessType::FsChannelOther);
  }
}

BENCHMARK_REGISTER_F(ManyProcessAccessLogsFixture, add_self_round_robin)
    ->Threads(kThreadCount);
