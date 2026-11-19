/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include <fmt/format.h>
#include <fmt/ranges.h>
#include <folly/String.h>
#include <iomanip>
#include <map>
#include <sstream>
#include <string>
#include <vector>

#include "eden/common/utils/benchharness/Bench.h"

namespace {

struct TestData {
  int number = 42;
  std::string str = "hello world";
  double dbl = 3.14;
  bool flag = false;
};

struct ExpensiveTestData {
  std::vector<int> numbers = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10};
  std::map<std::string, std::string> metadata = {
      {"file_path", "/very/long/path/to/some/file/in/the/filesystem.txt"},
      {"timestamp", "2024-01-15T10:30:45.123456Z"},
      {"user_id", "user_12345_with_very_long_identifier"},
      {"session_id", "session_abcdef123456789_extended_identifier"}};
  std::vector<std::string> tags =
      {"performance", "critical", "high-priority", "user-facing", "backend"};
  double performance_metrics[5] = {99.95, 87.23, 156.78, 42.11, 73.89};
  int count = 0;
  bool is_active = true;
  std::string description =
      "This is a complex data structure used for benchmarking expensive formatting operations that involve multiple data types and nested structures";
};

void iostream_benchmark(benchmark::State& state) {
  TestData data;
  for (auto _ : state) {
    std::ostringstream ss;
    ss << "Data{number: " << data.number << ", str: " << data.str
       << ", dbl: " << data.dbl << ", flag: " << (data.flag ? "true" : "false")
       << "}";
    std::string result = ss.str();
    benchmark::DoNotOptimize(result);
  }
}

void fmt_benchmark(benchmark::State& state) {
  TestData data;
  for (auto _ : state) {
    std::string result = fmt::format(
        "Data{{number: {}, str: {}, dbl: {}, flag: {}}}",
        data.number,
        data.str,
        data.dbl,
        data.flag);
    benchmark::DoNotOptimize(result);
  }
}

void expensive_iostream_benchmark(benchmark::State& state) {
  ExpensiveTestData data;
  for (auto _ : state) {
    std::ostringstream ss;
    ss << "ExpensiveData{numbers: [";
    for (size_t i = 0; i < data.numbers.size(); ++i) {
      if (i > 0) {
        ss << ", ";
      }
      ss << data.numbers[i];
    }
    ss << "], metadata: {";
    bool first = true;
    for (const auto& [key, value] : data.metadata) {
      if (!first) {
        ss << ", ";
      }
      ss << key << ": " << value;
      first = false;
    }
    ss << "}, tags: [";
    for (size_t i = 0; i < data.tags.size(); ++i) {
      if (i > 0) {
        ss << ", ";
      }
      ss << data.tags[i];
    }
    ss << "], metrics: [";
    for (size_t i = 0; i < 5; ++i) {
      if (i > 0) {
        ss << ", ";
      }
      ss << std::fixed << std::setprecision(2) << data.performance_metrics[i];
    }
    ss << "], errors: " << data.count
       << ", active: " << (data.is_active ? "true" : "false")
       << ", desc: " << data.description << "}";
    std::string result = ss.str();
    benchmark::DoNotOptimize(result);
  }
}

void expensive_fmt_benchmark(benchmark::State& state) {
  ExpensiveTestData data;
  for (auto _ : state) {
    std::string numbers_str = fmt::format("{}", fmt::join(data.numbers, ", "));

    std::string metadata_str;
    bool first = true;
    for (const auto& [key, value] : data.metadata) {
      if (!first) {
        metadata_str += fmt::format(",{}: {}", key, value);
      } else {
        metadata_str += fmt::format("{}: {}", key, value);
        first = false;
      }
    }

    std::string tags_str = fmt::format("{}", fmt::join(data.tags, ", "));

    std::string metrics_str = fmt::format(
        "{:.2f}, {:.2f}, {:.2f}, {:.2f}, {:.2f}",
        data.performance_metrics[0],
        data.performance_metrics[1],
        data.performance_metrics[2],
        data.performance_metrics[3],
        data.performance_metrics[4]);

    std::string result = fmt::format(
        "ExpensiveData{{numbers: [{}], metadata: {{{}}}, tags: [{}], "
        "metrics: [{}], errors: {}, active: {}, desc: {}}}",
        numbers_str,
        metadata_str,
        tags_str,
        metrics_str,
        data.count,
        data.is_active,
        data.description);
    benchmark::DoNotOptimize(result);
  }
}

// TODO std::format benchmark

BENCHMARK(iostream_benchmark);
BENCHMARK(fmt_benchmark);
BENCHMARK(expensive_iostream_benchmark);
BENCHMARK(expensive_fmt_benchmark);

} // namespace

EDEN_BENCHMARK_MAIN();
