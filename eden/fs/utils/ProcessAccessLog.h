/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/Synchronized.h>
#include <folly/ThreadLocal.h>
#include <type_traits>

#include "eden/common/utils/BucketedLog.h"
#include "eden/common/utils/EnumValue.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"

namespace facebook::eden {

class ProcessInfoCache;
struct ThreadLocalBucket;
struct ThreadLocalBucketTag;
// Strict mode makes destroying a log wait for a thread that is exiting at the
// same time: in the default mode the exiting thread unlinks its bucket before
// running the bucket's destructor, so that destructor could merge into a log
// that is already gone.
using ThreadLocalBucketAccessMode = folly::AccessModeStrict;

/**
 * An inexpensive mechanism for counting accesses by pids. Intended for counting
 * channel and Thrift calls from external processes.
 *
 * Each thread keeps one bucket per ProcessAccessLog it records into, so a
 * thread pool may serve several ProcessAccessLog instances. A caller must keep
 * the ProcessAccessLog alive while it records into it.
 */
class ProcessAccessLog {
 public:
  enum class AccessType : unsigned char {
    FsChannelRead,
    FsChannelWrite,
    FsChannelOther,
    FsChannelMemoryCacheImport,
    FsChannelDiskCacheImport,
    FsChannelBackingStoreImport,
    Last,
  };

  explicit ProcessAccessLog(std::shared_ptr<ProcessInfoCache> processInfoCache);
  ~ProcessAccessLog();

  /**
   * Records an access by a process ID.
   *
   * Process IDs passed to recordAccess are also inserted into the
   * ProcessInfoCache.
   */
  void recordAccess(pid_t pid, AccessType type);
  void recordDuration(pid_t pid, std::chrono::nanoseconds duration);

  /**
   * Returns the number of times each pid was passed to recordAccess() in
   * `lastNSeconds`.
   *
   * Note: ProcessAccessLog buckets by whole seconds, so this number should be
   * considered an approximation.
   */
  std::unordered_map<pid_t, AccessCounts> getAccessCounts(
      std::chrono::seconds lastNSeconds);

 private:
  struct PerBucketAccessCounts {
    size_t counts[enumValue(AccessType::Last)];
    std::chrono::nanoseconds duration;

    size_t& operator[](AccessType type) {
      static_assert(std::is_unsigned_v<std::underlying_type_t<AccessType>>);
      auto idx = enumValue(type);
      XCHECK_LT(idx, enumValue(AccessType::Last));
      return counts[idx];
    }
  };

  // Data for one second.
  struct Bucket {
    void clear();
    void add(pid_t pid, bool& isNew, AccessType type);
    void add(pid_t pid, bool& isNew, std::chrono::nanoseconds duration);
    void merge(const Bucket& other);

    std::unordered_map<pid_t, PerBucketAccessCounts> accessCountsByPid;
  };

  // Keep up to ten seconds of data, but use a power of two so BucketedLog
  // generates smaller, faster code.
  static constexpr uint64_t kBucketCount = 16;
  using Buckets = BucketedLog<Bucket, kBucketCount>;

  struct State {
    Buckets buckets;
  };

  const std::shared_ptr<ProcessInfoCache> processInfoCache_;
  folly::Synchronized<State> state_;
  // Declared after state_: destroying it merges every thread's bucket into
  // state_.
  folly::ThreadLocalPtr<
      ThreadLocalBucket,
      ThreadLocalBucketTag,
      ThreadLocalBucketAccessMode>
      threadLocalBucket_;

  uint64_t getSecondsSinceEpoch();
  ThreadLocalBucket* getTlb();

  friend struct ThreadLocalBucket;
};

} // namespace facebook::eden
