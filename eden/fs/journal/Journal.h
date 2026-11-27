/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/Function.h>
#include <folly/SharedMutex.h>
#include <folly/Synchronized.h>
#include <algorithm>
#include <atomic>
#include <cstdint>
#include <memory>
#include <optional>
#include <unordered_map>
#include "eden/fs/journal/JournalDelta.h"
#include "eden/fs/model/RootId.h"
#include "eden/fs/service/gen-cpp2/streamingeden_types.h"
#include "eden/fs/takeover/gen-cpp2/takeover_types.h"
#include "eden/fs/telemetry/EdenStats.h"

namespace facebook::eden {

class FaultInjector;

/** Contains statistics about the current state of the journal */
struct InternalJournalStats {
  size_t entryCount = 0;
  std::chrono::steady_clock::time_point earliestTimestamp;
  std::chrono::steady_clock::time_point latestTimestamp;
  size_t maxFilesAccumulated = 0;
  uint64_t getDurationInSeconds() {
    return std::chrono::duration_cast<std::chrono::seconds>(
               std::chrono::steady_clock::now() - earliestTimestamp)
        .count();
  }
};

struct JournalDeltaInfo {
  RootId fromRoot;
  RootId toRoot;
  JournalDelta::SequenceNumber sequenceID;
  std::chrono::steady_clock::time_point time;
};

/** The Journal exists to answer questions about how files are changing
 * over time.
 *
 * It contains metadata only; it is not a full snapshot of the state of
 * the filesystem at a particular point in time.
 * The intent is to be able query things like "which set of files changed
 * between time A and time B?".
 *
 * In the initial implementation we are recording file names from the overlay
 * but will expand this to record things like checking out different
 * revisions (the prior and new revision root) from which we can derive
 * the larger list of files.
 *
 * The Journal class is thread-safe.  Subscribers are called on the thread
 * that called addDelta.
 */
class Journal {
 public:
  using SequenceNumber = JournalDelta::SequenceNumber;
  using SubscriberId = uint64_t;
  using SubscriberCallback = std::function<void()>;
  using FileChangeCallback = std::function<bool(const FileChangeJournalDelta&)>;
  using RootUpdateCallback = std::function<bool(const RootUpdateJournalDelta&)>;

  /** The optional fault injector must outlive this journal. */
  explicit Journal(
      EdenStatsPtr edenStats,
      FaultInjector* faultInjector = nullptr);

  Journal(const Journal&) = delete;
  Journal& operator=(const Journal&) = delete;

  // Functions to record writes:

  void recordCreated(RelativePathPiece fileName, dtype_t type);
  void recordRemoved(RelativePathPiece fileName, dtype_t type);
  void recordChanged(RelativePathPiece fileName, dtype_t type);

  /**
   * "Renamed" means that that newName was created as a result of the mv(1).
   */
  void recordRenamed(
      RelativePathPiece oldName,
      RelativePathPiece newName,
      dtype_t type);

  /**
   * "Replaced" means that that newName was overwritten by oldName as a result
   * of the mv(1).
   */
  void recordReplaced(
      RelativePathPiece oldName,
      RelativePathPiece newName,
      dtype_t type);

  /**
   * Creates a journal delta that updates the root to this new root
   */
  void recordRootUpdate(RootId toRoot);

  /**
   * Creates a journal delta that updates the root from fromRoot to toRoot
   */
  void recordRootUpdate(RootId fromRoot, RootId toRoot);

  /**
   * Creates a journal delta that updates the root from fromRoot to toRoot and
   * also sets uncleanPaths
   */
  void recordUncleanPaths(
      RootId fromRoot,
      RootId toRoot,
      std::unordered_set<RelativePath> uncleanPaths);

  // Functions for reading the current state of the journal:

  /**
   * Returns a copy of the tip of the journal and marks the journal as observed.
   * This unblocks subscriber notifications on the next addDelta call.
   * Will return a nullopt if the journal is empty.
   */
  std::optional<JournalDeltaInfo> observeLatest();

  /**
   * Returns a copy of the tip of the journal without marking the journal
   * as observed. Use this when you only need the current position and
   * don't want to unblock subscriber notifications.
   */
  std::optional<JournalDeltaInfo> peekLatest() const;

  /**
   * Returns an accumulation of all deltas with sequence number >= limitSequence
   * merged. If limitSequence is further back than the Journal remembers,
   * isTruncated will be set on the JournalDeltaSum.
   *
   * The default limit value indicates that all deltas should be summed.
   *
   * If the limitSequence means that no deltas will match, returns nullptr.
   */
  std::unique_ptr<JournalDeltaRange> accumulateRange(
      SequenceNumber limitSequence = 1);

  /**
   * Returns true if every change with sequence number >= limitSequence
   * touches only paths under .sl or legacy .hg, the range contains no root
   * transitions, and the range has not been truncated from the journal.
   *
   * This answers the same question as reading isTruncated,
   * containsSaplingOnlyChanges, and containsRootUpdate from
   * accumulateRange(limitSequence), but does not materialize the set of
   * changed files and returns as soon as a disqualifying delta is seen.
   */
  bool containsOnlySaplingChanges(SequenceNumber limitSequence);

  /**
   * Runs from the latest delta to the delta with sequence ID (if 'lengthLimit'
   * is not nullopt then checks at most 'lengthLimit' entries) and runs
   * appropriate callback on each entry encountered.
   *
   * Return bool indicating whether the journal is truncated
   */
  bool forEachDelta(
      JournalDelta::SequenceNumber from,
      std::optional<size_t> lengthLimit,
      FileChangeCallback&& fileChangeCallback,
      RootUpdateCallback&& rootUpdateCallback);

  // Subscription functionality:

  /**
   * Registers a callback to be invoked when the journal has changed.
   *
   * The subscriber is called while the subscriber lock is held, so it
   * is recommended the subscriber callback do the minimal amount of work needed
   * to schedule the real work to happen in some other context, because journal
   * updates are likely to happen in awkward contexts or in the middle of some
   * batch of mutations where it is not appropriate to do any heavy lifting.
   *
   * To minimize notification traffic, the Journal may coalesce redundant
   * modifications between subscriber notifications and calls to observeLatest
   * or accumulateRange.
   *
   * The return value of registerSubscriber is an identifier than can be passed
   * to cancelSubscriber to later remove the registration.
   */
  SubscriberId registerSubscriber(SubscriberCallback&& callback);
  void cancelSubscriber(SubscriberId id);

  void cancelAllSubscribers();
  bool isSubscriberValid(SubscriberId id) const;

  // Statistics and debugging:

  /**
   * Returns an option that is nullopt if the Journal is empty or an option
   * that contains valid InternalJournalStats if the Journal is non-empty.
   */
  std::optional<InternalJournalStats> getStats();

  /** Gets a vector of the modifications (newer deltas having lower indices)
   * done by the latest 'limit' deltas, if the
   * beginning of the journal is reached before 'limit' number of deltas are
   * reached then it will just return what had been currently found.
   * */
  std::vector<DebugJournalDelta> getDebugRawJournalInfo(
      SequenceNumber from,
      std::optional<size_t> limit,
      long mountGeneration,
      RootIdCodec& rootIdCodec) const;

  /** Removes all prior contents from the journal and sets up the journal in a
   * way such that when subscribers are notified they all get truncated results
   * */
  void flush();

  void setMemoryLimit(size_t limit);

  size_t getMemoryLimit() const;

  size_t estimateMemoryUsage() const;

  // Graceful restart:

  /**
   * Copies the newest deltas, up to maxBytes of their estimated memory usage,
   * for the next EdenFS process to restore with initializeFromTakeover(). The
   * newest delta is always copied: a restored journal with no deltas would
   * report no changes for every earlier position instead of truncation.
   * Returns nullopt if the journal is empty.
   */
  std::optional<SerializedJournal> serializeForTakeover(
      uint64_t mountGeneration,
      size_t maxBytes) const;

  /**
   * Restores the deltas the previous EdenFS process copied with
   * serializeForTakeover(). Positions older than the oldest restored delta
   * read as truncated. Must be called before anything is recorded.
   */
  void initializeFromTakeover(SerializedJournal&& journal);

 private:
  /** Add a delta to the journal and notify subscribers.
   * The delta will have a new sequence number and timestamp
   * applied.
   */
  void addDelta(FileChangeJournalDelta&& delta);
  void addDelta(RootUpdateJournalDelta&& delta, RootId newRootId);

  static constexpr size_t kDefaultJournalMemoryLimit = 1000000000;

  struct DeltaState {
    /**
     * The sequence number that we'll use for the next entry that we link into
     * the chain.
     */
    SequenceNumber nextSequence{1};
    /**
     * All recorded entries. Newer (more recent) deltas are added to the back of
     * the appropriate deque.
     */
    std::deque<FileChangeJournalDelta> fileChangeDeltas;
    std::deque<RootUpdateJournalDelta> rootUpdateDeltas;
    RootId currentRoot;
    /// The stats about this Journal up to the latest delta.
    std::optional<InternalJournalStats> stats;
    size_t memoryLimit = kDefaultJournalMemoryLimit;
    size_t deltaMemoryUsage = 0;

    JournalDeltaPtr frontPtr() noexcept;
    void popFront();
    JournalDeltaPtr backPtr() noexcept;

    bool empty() const {
      return fileChangeDeltas.empty() && rootUpdateDeltas.empty();
    }

    bool isFileChangeInFront() const;
    bool isFileChangeInBack() const;

    void appendDelta(FileChangeJournalDelta&& delta);
    void appendDelta(RootUpdateJournalDelta&& delta);

    JournalDelta::SequenceNumber getFrontSequenceID() const {
      if (isFileChangeInFront()) {
        return fileChangeDeltas.front().earliestSequenceID;
      } else {
        return rootUpdateDeltas.front().earliestSequenceID;
      }
    }

    /**
     * Mark the journal as observed. Callable with at least a read lock
     * (const method). Unblocks subscriber notifications on the next addDelta.
     */
    void markObserved() const {
      lastModificationHasBeenObserved_.store(true);
    }

    /**
     * Atomically clear the observed flag and return its previous value.
     * Requires write lock (non-const method).
     * Returns true if subscribers should be notified.
     */
    [[nodiscard]] bool clearObserved() {
      return lastModificationHasBeenObserved_.exchange(false);
    }

    /**
     * Update the high-water mark for files accumulated in a single
     * accumulateRange call. Callable with at least a read lock (const method).
     */
    void updateMaxFilesAccumulated(size_t filesAccumulated) const {
      auto current = maxFilesAccumulated_.load();
      while (filesAccumulated > current &&
             !maxFilesAccumulated_.compare_exchange_weak(
                 current, filesAccumulated)) {
      }
    }

    /**
     * Returns the high-water mark for files accumulated.
     */
    size_t getMaxFilesAccumulated() const {
      return maxFilesAccumulated_.load();
    }

    /**
     * Resets the high-water mark. Requires write lock (non-const method).
     */
    void resetMaxFilesAccumulated() {
      maxFilesAccumulated_.store(0);
    }

   private:
    // Atomic flag for notification coalescing.
    //
    // Locking safety: this is std::atomic because markObserved() (which
    // stores true) is called under rlock, allowing concurrent readers.
    // The interesting cases:
    //
    //  - Multiple concurrent rlock holders call markObserved(): all
    //    store true idempotently — no lost updates.
    //  - markObserved() (rlock) vs clearObserved() (wlock): cannot run
    //    concurrently — SharedMutex excludes readers while a writer
    //    holds the lock.
    //  - Two concurrent addDelta calls racing on clearObserved(): cannot
    //    happen — clearObserved() runs under wlock, so only one writer
    //    can exchange the flag at a time.
    //
    // Net result: the only concurrent access pattern is multiple
    // store(true) calls, which is safe and idempotent.
    mutable std::atomic<bool> lastModificationHasBeenObserved_{true};

    // High-water mark for files accumulated. Updated via CAS under rlock.
    mutable std::atomic<size_t> maxFilesAccumulated_{0};
  };
  folly::Synchronized<DeltaState, folly::SharedMutex> deltaState_;

  /**
   * Removes the oldest deltas until the memory usage of the journal is below
   * the journal's memory limit.
   */
  void truncateIfNecessary(DeltaState& deltaState);

  /**
   * Tries to compact a new Journal Delta with an old one if possible,
   * returning true if it did compact it and false if not
   */
  bool compact(FileChangeJournalDelta& delta, DeltaState& deltaState);
  bool compact(RootUpdateJournalDelta& delta, DeltaState& deltaState);

  struct SubscriberState {
    SubscriberId nextSubscriberId{1};
    std::unordered_map<SubscriberId, SubscriberCallback> subscribers;
  };

  /**
   * Add a delta to the journal without notifying subscribers.
   * The delta will have a new sequence number and timestamp
   * applied. A lock to the deltaState must be held and passed to this
   * function.
   *
   * Returns true if subscribers should be notified.
   */
  template <typename T>
  [[nodiscard]] bool addDeltaBeforeNotifying(T&& delta, DeltaState& deltaState);

  /**
   * Notify subscribers that a change has happened. Must not be called while
   * Journal locks are held.
   */
  void notifySubscribers() const;

  size_t estimateMemoryUsage(const DeltaState& deltaState) const;

  static std::optional<JournalDeltaInfo> getLatestInfo(
      const DeltaState& deltaState);

  /**
   * Runs from the latest delta to the delta with sequence ID (if 'lengthLimit'
   * is not nullopt then checks at most 'lengthLimit' entries) and runs
   * deltaActor on each entry encountered.
   * */
  void forEachDelta(
      const DeltaState& deltaState,
      JournalDelta::SequenceNumber from,
      std::optional<size_t> lengthLimit,
      FileChangeCallback&& fileChangeDeltaCallback,
      RootUpdateCallback&& rootUpdateDeltaCallback) const;

  folly::Synchronized<SubscriberState> subscriberState_;

  EdenStatsPtr edenStats_;
  FaultInjector* const faultInjector_;
};
} // namespace facebook::eden
