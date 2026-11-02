/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <atomic>
#include <memory>

#include <folly/CancellationToken.h>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_map>
#include <utility>
#include <variant>

#include <folly/Executor.h>
#include <folly/executors/QueuedImmediateExecutor.h>

#include "eden/common/os/ProcessId.h"
#include "eden/common/utils/RefPtr.h"
#include "eden/fs/store/ImportPriority.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/utils/MiniTracer.h"

namespace facebook::eden {

class ObjectId;
class SourceLocation;

class ObjectFetchContext;

using ObjectFetchContextPtr = RefPtr<ObjectFetchContext>;

/**
 * ObjectStore calls methods on this context when fetching objects.
 * It's primarily used to track when and why source control objects are fetched.
 */
class ObjectFetchContext : public RefCounted {
 public:
  /**
   * Which object type was fetched.
   *
   * Suitable for use as an index into an array of size kObjectTypeEnumMax
   */
  enum ObjectType : uint8_t {
    Blob,
    BlobAuxData,
    Tree,
    TreeAuxData,
    RootTree,
    ManifestForRoot,
    PrefetchBlob,
    kObjectTypeEnumMax,
  };

  /**
   * The source of the data that was fetched
   */
  enum class FetchedSource : uint8_t {
    /** The data was fetched from a local source */
    Local,
    /** The data was fetched from a remote source */
    Remote,
    /**
     * The data will be fetched from local or remote source.
     * We don't know the source yet.
     */
    Unknown,
  };

  /**
   * It shows that the fetch was successful or not.
   */
  enum class FetchResult : uint8_t {
    /** The fetch was successful */
    Success,
    /** The fetch failed */
    Failure,
  };

  /**
   * Which cache satisfied a lookup request.
   *
   * Suitable for use as an index into an array of size kOriginEnumMax.
   */
  enum Origin : uint8_t {
    /** The request didn't succeed */
    NotFetched,
    /** The request was serviced from a memory cache */
    FromMemoryCache,
    /** The request was serviced from a disk cache */
    FromDiskCache,
    /** The request was serviced with a network request */
    FromNetworkFetch,
    kOriginEnumMax,
  };

  /**
   * Why did EdenFS fetch these objects?
   * The enum values are based on the priority of the request
   * that can trigger different behavior in the backingstore.
   * If you add a new cause, ensure the priority order is correct.
   */
  enum Cause : uint8_t {
    /** Lowest Priority - unknown origination */
    Unknown = 0,
    /** Inode garbage collection loading the trees it scans */
    InodeGc = 1,
    /** The request originated from a Thrift prefetch endpoint */
    Prefetch = 2,
    /** The request originated from a Thrift glob endpoint */
    Glob = 3,
    /** The request originated from a Thrift endpoint */
    Thrift = 4,
    /** Highest Priority - The request originated from FUSE/NFS/PrjFS */
    Fs = 5,
  };

  /**
   * Whether fetches with this cause are reported to the walk detector. A
   * prefetch or glob is not a walk by a process, and neither is the inode
   * garbage collector revisiting the trees of remembered inodes.
   */
  static constexpr bool shouldTriggerWalkDetection(Cause cause) {
    return cause != Cause::Prefetch && cause != Cause::Glob &&
        cause != Cause::InodeGc;
  }

  /**
   * A string whose backing storage is guaranteed to have static lifetime.
   * Use fromLiteral() for string literals, or fromSourceLocation() for function
   * names supplied by EDEN_CURRENT_SOURCE_LOCATION.
   */
  class StaticCauseDetail {
   public:
    template <size_t Size>
    static consteval StaticCauseDetail fromLiteral(
        const char (&detail)[Size]) noexcept {
      return StaticCauseDetail{detail, Size - 1};
    }

    static StaticCauseDetail fromSourceLocation(
        SourceLocation sourceLocation) noexcept;

    constexpr std::string_view asStringView() const noexcept {
      return detail_;
    }

   private:
    constexpr StaticCauseDetail(const char* detail, size_t size) noexcept
        : detail_{detail, size} {}

    explicit constexpr StaticCauseDetail(std::string_view detail) noexcept
        : detail_{detail} {}

    std::string_view detail_;
  };

  /**
   * Optional detail associated with a fetch cause.
   *
   * Static details are allocation-free. Dynamic details use shared immutable
   * storage, so this type is intentionally not usable in constant expressions.
   */
  class CauseDetail {
   public:
    constexpr CauseDetail() noexcept = default;
    /* implicit */ CauseDetail(std::nullopt_t) noexcept {}
    /* implicit */ CauseDetail(StaticCauseDetail detail) noexcept
        : detail_{detail.asStringView()} {}

    static CauseDetail fromOwnedString(std::string detail) {
      return CauseDetail{
          std::make_shared<const std::string>(std::move(detail))};
    }

    std::optional<std::string_view> asStringView() const& noexcept {
      if (const auto* ownedDetail =
              std::get_if<std::shared_ptr<const std::string>>(&detail_);
          ownedDetail && *ownedDetail) {
        return std::string_view{**ownedDetail};
      }
      if (const auto* staticDetail = std::get_if<std::string_view>(&detail_)) {
        return *staticDetail;
      }
      return std::nullopt;
    }

    std::optional<std::string_view> asStringView() const&& = delete;

   private:
    explicit CauseDetail(std::shared_ptr<const std::string> detail) noexcept
        : detail_{std::move(detail)} {}

    std::variant<
        std::monostate,
        std::string_view,
        std::shared_ptr<const std::string>>
        detail_;
  };

  ObjectFetchContext() = default;

  virtual ~ObjectFetchContext() = default;

  virtual void
  didFetch(ObjectType, const ObjectId&, Origin, uint64_t /* bytes */ = 0) {}

  virtual bool shouldCollectStats() const {
    return false;
  }

  virtual OptionalProcessId getClientPid() const {
    return std::nullopt;
  }

  /**
   * The uid/gid claimed by the client that triggered this request, when
   * known. Currently only populated for NFS requests, from the AUTH_SYS
   * credential; note that AUTH_SYS credentials are client-asserted and
   * spoofable, so these are suitable for telemetry and coarse policy only.
   */
  virtual std::optional<uint32_t> getClientUid() const {
    return std::nullopt;
  }

  virtual std::optional<uint32_t> getClientGid() const {
    return std::nullopt;
  }

  /**
   * If known, returns the reason these objects were fetched.
   */
  virtual Cause getCause() const = 0;

  /**
   * If known, returns additional detail about why these objects were fetched.
   * The returned view remains valid only while this context is alive.
   */
  virtual std::optional<std::string_view> getCauseDetail() const {
    return std::nullopt;
  }

  /**
   * Return an owned copy that remains valid after this context is destroyed.
   * Use getCauseDetail() when the result is consumed immediately.
   */
  CauseDetail copyCauseDetail() const {
    if (const auto detail = getCauseDetail()) {
      return CauseDetail::fromOwnedString(std::string{*detail});
    }
    return {};
  }

  /**
   * Get the cancellation token for this context.
   * Returns an empty cancellation token if no cancellation support.
   */
  virtual folly::CancellationToken getCancellationToken() const {
    return cancellationToken_;
  }

  /**
   * Set the cancellation token for this context.
   */
  void setCancellationToken(folly::CancellationToken token) {
    cancellationToken_ = std::move(token);
  }

  /**
   * Throws an exception if the cancellation token has been canceled.
   *
   * @param throwOnCancel If false, this method returns without checking
   *                      the cancellation token. If true, checks and throws.
   * @throws folly::OperationCancelled if the operation has been canceled
   *         and throwOnCancel is true
   */
  void throwIfCanceled(bool throwOnCancel = true) const {
    if (throwOnCancel && cancellationToken_.isCancellationRequested()) {
      throw folly::OperationCancelled();
    }
  }

  virtual ImportPriority getPriority() const {
    return kDefaultImportPriority;
  }

  void setFetchedSource(
      FetchedSource fetchedSource,
      ObjectType type,
      EdenStatsPtr stats) {
    // There is no stat increment for FetchedSource::Unknown
    const auto counterIt = saplingStatsMap_.find({fetchedSource, type});
    if (counterIt != saplingStatsMap_.end()) {
      stats->increment(counterIt->second);
    }
    fetchedSource_.store(fetchedSource, std::memory_order_relaxed);
  }

  FetchedSource getFetchedSource() const {
    return fetchedSource_.load(std::memory_order_relaxed);
  }

  // RequestInfo keys used by ReCasBackingStore
  inline static const std::string kSessionIdField = "session-id";
  inline static const std::string kCacheSessionIdField = "cache-session-id";
  // RequestInfo keys used by SaplingBackingStore
  inline static const std::string kClientCorrelator = "client-correlator";
  inline static const std::string kClientEntryPoint = "client-entrypoint";

  virtual const std::unordered_map<std::string, std::string>* getRequestInfo()
      const = 0;

  /**
   * Support deprioritizing in sub-classes.
   * Note: Normally, each ObjectFetchContext is designed to be used for only one
   * import (with NullObjectFetchContext being the only exception currently).
   * Therefore, this method should only be called once on each
   * ObjectFetchContext object (when it is related to a process doing too much
   * fetches). However, implementations of this method should write the priority
   * change to log as debug information and watch out for unexpected uses of
   * ObjectFetchContext that cause it to be used for more than one import.
   */
  virtual void deprioritize(uint64_t) {}

  /**
   * Record the expected size of a blob about to be prefetched.
   * Called during tree traversal when file sizes are known from tree entries.
   */
  virtual void addPrefetchedBlobSize(uint64_t /* bytes */) {}

  /**
   * Number of file and directory entries in a directory.
   */
  struct DirEntryCounts {
    uint32_t files;
    uint32_t dirs;
  };

  /**
   * Entry counts of the directory containing the blob being fetched, as
   * recorded when that directory's TreeInode was loaded. A hint for walk
   * detection; nullopt when unknown.
   */
  virtual std::optional<DirEntryCounts> getParentDirEntryCounts() const {
    return std::nullopt;
  }

  virtual void setParentDirEntryCounts(DirEntryCounts /* counts */) {}

  /**
   * Return a no-op fetch context suitable when no tracking is desired.
   */
  static ObjectFetchContextPtr getNullContext();

  /**
   * Return a no-op fetch context which has causeDetail field. This field will
   * be logged which in turn can point out "blind spots" in logging i.e. places
   * where null context should be replaces with a real context.
   *
   * Note that this function allocates and return a pointer to a newly allocated
   * memory. This pointer is intended to be used as static variable i.e. static
   * auto ptr = ObjectFetchContext::getNullContextWithCauseDetail("someval");
   */
  static ObjectFetchContextPtr getNullContextWithCauseDetail(
      CauseDetail causeDetail);

  /**
   * Return a no-op fetch context with Cause::Fs for tests.
   */
  static ObjectFetchContextPtr getNullFsContext();

  /**
   * Return a no-op fetch context with Cause::Prefetch for tests.
   */
  static ObjectFetchContextPtr getNullPrefetchContext();

  /**
   * Return a no-op fetch context with Cause::InodeGc for the inode garbage
   * collector's loads.
   */
  static ObjectFetchContextPtr getNullInodeGcContext();

  /**
   * Set the time tracer.
   */
  void setTimeTracer(std::shared_ptr<MiniTracer> tracer) {
    timeTracer_ = std::move(tracer);
  }

  /**
   * Get the time tracer.
   */
  std::shared_ptr<MiniTracer> getTimeTracer() {
    return timeTracer_;
  }

  /**
   * Create a span for timing instrumentation if a time tracer is set.
   * Returns nullopt if no time tracer is set.
   */
  template <typename... Args>
  std::optional<MiniTracer::Span> createSpan(Args&&... args) const {
    if (timeTracer_) {
      return timeTracer_->createSpan(std::forward<Args>(args)...);
    }
    return std::nullopt;
  }

  /**
   * Set the executor to use for certain detached async operations.
   */
  void setDetachedExecutor(folly::Executor::KeepAlive<> executor) {
    detachedExecutor_ = std::move(executor);
  }

  /**
   * Get the executor for use with certain detached async operations.
   * Defaults to QueuedImmediateExecutor if not explicitly set.
   */
  const folly::Executor::KeepAlive<>& getDetachedExecutor() const {
    return detachedExecutor_;
  }

 private:
  ObjectFetchContext(const ObjectFetchContext&) = delete;
  ObjectFetchContext& operator=(const ObjectFetchContext&) = delete;

  std::atomic<FetchedSource> fetchedSource_{FetchedSource::Unknown};

  // Time tracer for instrumentation (may be nullptr)
  std::shared_ptr<MiniTracer> timeTracer_{nullptr};

  // Executor for certain detached async operations. This should only be used
  // in exceptional cases (such as the detached inode loading futures).
  folly::Executor::KeepAlive<> detachedExecutor_{
      &folly::QueuedImmediateExecutor::instance()};

  // Cancellation token for this fetch context
  folly::CancellationToken cancellationToken_;

  std::unordered_map<
      std::tuple<FetchedSource, ObjectType>,
      StatsGroupBase::Counter SaplingBackingStoreStats::*>
      saplingStatsMap_ = {
          {{FetchedSource::Local, ObjectType::Tree},
           &SaplingBackingStoreStats::fetchTreeLocal},
          {{FetchedSource::Local, ObjectType::TreeAuxData},
           &SaplingBackingStoreStats::fetchTreeAuxDataLocal},
          {{FetchedSource::Local, ObjectType::RootTree},
           &SaplingBackingStoreStats::getRootTreeLocal},
          {{FetchedSource::Local, ObjectType::ManifestForRoot},
           &SaplingBackingStoreStats::importManifestForRootLocal},
          {{FetchedSource::Local, ObjectType::Blob},
           &SaplingBackingStoreStats::fetchBlobLocal},
          {{FetchedSource::Local, ObjectType::BlobAuxData},
           &SaplingBackingStoreStats::fetchBlobAuxDataLocal},
          {{FetchedSource::Local, ObjectType::PrefetchBlob},
           &SaplingBackingStoreStats::prefetchBlobLocal},
      };
};

// For fbcode/eden/scm/lib/backingstore/src/ffi.rs
using FetchCause = ObjectFetchContext::Cause;

} // namespace facebook::eden
