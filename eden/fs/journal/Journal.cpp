/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/journal/Journal.h"
#include <folly/logging/xlog.h>
#include "eden/common/utils/FaultInjector.h"
#include "eden/fs/journal/JournalDelta.h"

namespace facebook::eden {

namespace {

bool isSaplingMetadataPath(const RelativePath& path) {
  if (path.empty()) {
    return true;
  }
  const auto firstComponent = path.paths().begin().piece();
  return firstComponent == ".sl"_relpath || firstComponent == ".hg"_relpath;
}

int64_t toNanos(std::chrono::steady_clock::time_point time) {
  return std::chrono::duration_cast<std::chrono::nanoseconds>(
             time.time_since_epoch())
      .count();
}

std::chrono::steady_clock::time_point fromNanos(int64_t nanos) {
  return std::chrono::steady_clock::time_point{
      std::chrono::duration_cast<std::chrono::steady_clock::duration>(
          std::chrono::nanoseconds{nanos})};
}

} // namespace

JournalDeltaPtr Journal::DeltaState::frontPtr() noexcept {
  bool isFileChangeEmpty = fileChangeDeltas.empty();
  bool isRootUpdateEmpty = rootUpdateDeltas.empty();
  if (!isFileChangeEmpty && !isRootUpdateEmpty) {
    if (fileChangeDeltas.front().sequenceID <
        rootUpdateDeltas.front().sequenceID) {
      return &fileChangeDeltas.front();
    } else {
      return &rootUpdateDeltas.front();
    }
  }
  if (!isFileChangeEmpty) {
    return &fileChangeDeltas.front();
  } else if (!isRootUpdateEmpty) {
    return &rootUpdateDeltas.front();
  } else {
    return nullptr;
  }
}

void Journal::DeltaState::popFront() {
  bool isFileChangeEmpty = fileChangeDeltas.empty();
  bool isRootUpdateEmpty = rootUpdateDeltas.empty();
  if (!isFileChangeEmpty && !isRootUpdateEmpty) {
    if (fileChangeDeltas.front().sequenceID <
        rootUpdateDeltas.front().sequenceID) {
      fileChangeDeltas.pop_front();
    } else {
      rootUpdateDeltas.pop_front();
    }
  } else if (!isFileChangeEmpty) {
    fileChangeDeltas.pop_front();
  } else if (!isRootUpdateEmpty) {
    rootUpdateDeltas.pop_front();
  }
}

JournalDeltaPtr Journal::DeltaState::backPtr() noexcept {
  bool isFileChangeEmpty = fileChangeDeltas.empty();
  bool isRootUpdateEmpty = rootUpdateDeltas.empty();
  if (!isFileChangeEmpty && !isRootUpdateEmpty) {
    if (fileChangeDeltas.back().sequenceID >
        rootUpdateDeltas.back().sequenceID) {
      return &fileChangeDeltas.back();
    } else {
      return &rootUpdateDeltas.back();
    }
  }
  if (!isFileChangeEmpty) {
    return &fileChangeDeltas.back();
  } else if (!isRootUpdateEmpty) {
    return &rootUpdateDeltas.back();
  } else {
    return nullptr;
  }
}

bool Journal::DeltaState::isFileChangeInFront() const {
  bool isFileChangeEmpty = fileChangeDeltas.empty();
  bool isRootUpdateEmpty = rootUpdateDeltas.empty();
  if (!isFileChangeEmpty && !isRootUpdateEmpty) {
    return fileChangeDeltas.front().sequenceID <
        rootUpdateDeltas.front().sequenceID;
  }
  return !isFileChangeEmpty && isRootUpdateEmpty;
}

bool Journal::DeltaState::isFileChangeInBack() const {
  bool isFileChangeEmpty = fileChangeDeltas.empty();
  bool isRootUpdateEmpty = rootUpdateDeltas.empty();
  if (!isFileChangeEmpty && !isRootUpdateEmpty) {
    return fileChangeDeltas.back().sequenceID >
        rootUpdateDeltas.back().sequenceID;
  }
  return !isFileChangeEmpty && isRootUpdateEmpty;
}

void Journal::DeltaState::appendDelta(FileChangeJournalDelta&& delta) {
  fileChangeDeltas.emplace_back(std::move(delta));
}

void Journal::DeltaState::appendDelta(RootUpdateJournalDelta&& delta) {
  rootUpdateDeltas.emplace_back(std::move(delta));
}

Journal::Journal(EdenStatsPtr edenStats, FaultInjector* faultInjector)
    : edenStats_{std::move(edenStats)}, faultInjector_{faultInjector} {
  // Add 0 so that this counter shows up in ODS
  edenStats_->increment(&JournalStats::truncatedReads, 0);
}

void Journal::recordCreated(RelativePathPiece fileName, dtype_t type) {
  addDelta(
      FileChangeJournalDelta(fileName, type, FileChangeJournalDelta::CREATED));
}

void Journal::recordRemoved(RelativePathPiece fileName, dtype_t type) {
  addDelta(
      FileChangeJournalDelta(fileName, type, FileChangeJournalDelta::REMOVED));
}

void Journal::recordChanged(RelativePathPiece fileName, dtype_t type) {
  if (faultInjector_) {
    faultInjector_->check("journal-record", fileName.view());
  }
  addDelta(
      FileChangeJournalDelta(fileName, type, FileChangeJournalDelta::CHANGED));
}

void Journal::recordRenamed(
    RelativePathPiece oldName,
    RelativePathPiece newName,
    dtype_t type) {
  addDelta(FileChangeJournalDelta(
      oldName, newName, type, FileChangeJournalDelta::RENAMED));
}

void Journal::recordReplaced(
    RelativePathPiece oldName,
    RelativePathPiece newName,
    dtype_t type) {
  addDelta(FileChangeJournalDelta(
      oldName, newName, type, FileChangeJournalDelta::REPLACED));
}

void Journal::recordRootUpdate(RootId toRoot) {
  addDelta(RootUpdateJournalDelta{}, std::move(toRoot));
}

void Journal::recordRootUpdate(RootId fromRoot, RootId toRoot) {
  if (fromRoot == toRoot) {
    return;
  }
  RootUpdateJournalDelta delta;
  delta.fromRoot = std::move(fromRoot);
  addDelta(std::move(delta), toRoot);
}

void Journal::recordUncleanPaths(
    RootId fromRoot,
    RootId toRoot,
    std::unordered_set<RelativePath> uncleanPaths) {
  if (fromRoot == toRoot && uncleanPaths.empty()) {
    return;
  }
  RootUpdateJournalDelta delta;
  delta.fromRoot = std::move(fromRoot);
  delta.uncleanPaths = std::move(uncleanPaths);
  addDelta(std::move(delta), std::move(toRoot));
}

void Journal::truncateIfNecessary(DeltaState& deltaState) {
  while (JournalDeltaPtr front = deltaState.frontPtr()) {
    if (estimateMemoryUsage(deltaState) <= deltaState.memoryLimit) {
      break;
    }
    deltaState.stats->entryCount--;

    deltaState.deltaMemoryUsage -= front.estimateMemoryUsage();
    deltaState.popFront();
  }
}

bool Journal::compact(FileChangeJournalDelta& delta, DeltaState& deltaState) {
  auto back = deltaState.backPtr().getAsFileChangeJournalDelta();
  if (back && delta.isModification() && delta.isSameAction(*back)) {
    deltaState.stats->latestTimestamp = delta.time;
    deltaState.deltaMemoryUsage -= back->estimateMemoryUsage();
    deltaState.deltaMemoryUsage += delta.estimateMemoryUsage();
    delta.earliestSequenceID = back->earliestSequenceID;
    *back = std::move(delta);
    return true;
  }
  return false;
}

bool Journal::compact(
    RootUpdateJournalDelta& /* unused */,
    DeltaState& /* unused */) {
  return false;
}

template <typename T>
bool Journal::addDeltaBeforeNotifying(T&& delta, DeltaState& deltaState) {
  delta.sequenceID = deltaState.nextSequence++;
  delta.earliestSequenceID = delta.sequenceID;
  delta.time = std::chrono::steady_clock::now();

  truncateIfNecessary(deltaState);

  // We will compact the delta if possible. We can compact the delta if it is
  // a modification to a single file and matches the last delta added to the
  // Journal. For a consumer the only differences seen due to compaction are
  // that:
  // - getDebugRawJournalInfo will skip entries in its list
  // - The stats should show a different memory usage and number of entries
  // - accumulateRange will return a different fromSequence and fromTime than
  // what would happen if the deltas were not compacted [e.g. JournalDelta 3
  // and 4 are the same modification, accumulateRange(3) would have a
  // fromSequence of 3 without compaction and a fromSequence of 4 with
  // compaction]
  if (!compact(delta, deltaState)) {
    if (deltaState.stats) {
      ++(deltaState.stats->entryCount);
      deltaState.deltaMemoryUsage += delta.estimateMemoryUsage();
    } else {
      deltaState.stats = InternalJournalStats();
      deltaState.stats->entryCount = 1;
      deltaState.deltaMemoryUsage = delta.estimateMemoryUsage();
    }
    deltaState.stats->latestTimestamp = delta.time;
    deltaState.appendDelta(std::forward<T>(delta));
  }

  deltaState.stats->earliestTimestamp = deltaState.frontPtr()->time;

  return deltaState.clearObserved();
}

void Journal::notifySubscribers() const {
  auto subscribers = subscriberState_.rlock()->subscribers;
  for (auto& sub : subscribers) {
    sub.second();
  }
}

void Journal::addDelta(FileChangeJournalDelta&& delta) {
  bool shouldNotify;
  {
    auto deltaState = deltaState_.wlock();
    shouldNotify = addDeltaBeforeNotifying(std::move(delta), *deltaState);
  }
  if (shouldNotify) {
    notifySubscribers();
  }
}

void Journal::addDelta(RootUpdateJournalDelta&& delta, RootId newRootId) {
  bool shouldNotify;
  {
    auto deltaState = deltaState_.wlock();

    // If the roots were not set to anything, default to copying
    // the value from the prior journal entry
    if (delta.fromRoot == RootId{}) {
      delta.fromRoot = deltaState->currentRoot;
    }
    shouldNotify = addDeltaBeforeNotifying(std::move(delta), *deltaState);
    deltaState->currentRoot = std::move(newRootId);
  }
  if (shouldNotify) {
    notifySubscribers();
  }
}

std::optional<JournalDeltaInfo> Journal::getLatestInfo(
    const DeltaState& deltaState) {
  if (deltaState.empty()) {
    return std::nullopt;
  }
  if (deltaState.isFileChangeInBack()) {
    const FileChangeJournalDelta& back = deltaState.fileChangeDeltas.back();
    return JournalDeltaInfo{
        deltaState.currentRoot,
        deltaState.currentRoot,
        back.sequenceID,
        back.time};
  } else {
    const RootUpdateJournalDelta& back = deltaState.rootUpdateDeltas.back();
    return JournalDeltaInfo{
        back.fromRoot, deltaState.currentRoot, back.sequenceID, back.time};
  }
}

std::optional<JournalDeltaInfo> Journal::observeLatest() {
  auto deltaState = deltaState_.rlock();
  deltaState->markObserved();
  return getLatestInfo(*deltaState);
}

std::optional<JournalDeltaInfo> Journal::peekLatest() const {
  return getLatestInfo(*deltaState_.rlock());
}

uint64_t Journal::registerSubscriber(SubscriberCallback&& callback) {
  auto subscriberState = subscriberState_.wlock();
  auto id = subscriberState->nextSubscriberId++;
  subscriberState->subscribers[id] = std::move(callback);
  return id;
}

void Journal::cancelSubscriber(uint64_t id) {
  auto subscriberState = subscriberState_.wlock();
  auto it = subscriberState->subscribers.find(id);
  if (it == subscriberState->subscribers.end()) {
    return;
  }
  // Extend the lifetime of the value we're removing
  auto callback = std::move(it->second);
  subscriberState->subscribers.erase(it);
  // release the lock before we trigger the destructor
  subscriberState.unlock();
  // callback can now run its destructor outside the lock
}

void Journal::cancelAllSubscribers() {
  // Take care: some subscribers will attempt to call cancelSubscriber()
  // as part of their tear down, so we need to make sure that we aren't
  // holding the lock when we trigger that.
  std::unordered_map<SubscriberId, SubscriberCallback> subscribers;
  subscriberState_.wlock()->subscribers.swap(subscribers);
  subscribers.clear();
}

bool Journal::isSubscriberValid(uint64_t id) const {
  auto subscriberState = subscriberState_.rlock();
  auto& subscribers = subscriberState->subscribers;
  return subscribers.find(id) != subscribers.end();
}

std::optional<InternalJournalStats> Journal::getStats() {
  auto deltaState = deltaState_.rlock();
  if (!deltaState->stats) {
    return std::nullopt;
  }
  auto stats = *deltaState->stats;
  stats.maxFilesAccumulated = deltaState->getMaxFilesAccumulated();
  return stats;
}

void Journal::setMemoryLimit(size_t limit) {
  auto deltaState = deltaState_.wlock();
  deltaState->memoryLimit = limit;
}

size_t Journal::getMemoryLimit() const {
  auto deltaState = deltaState_.rlock();
  return deltaState->memoryLimit;
}

size_t Journal::estimateMemoryUsage() const {
  return estimateMemoryUsage(*deltaState_.rlock());
}

template <typename T>
size_t getPaddingAmount(const std::deque<T>& deltaDeque) {
  constexpr size_t numInDequeBuffer = 512 / sizeof(T);
  constexpr size_t maxBufSize = numInDequeBuffer * sizeof(T);
  size_t numBufs =
      (deltaDeque.size() + numInDequeBuffer - 1) / numInDequeBuffer;
  size_t padding = folly::goodMallocSize(maxBufSize) - maxBufSize;
  return padding * numBufs;
}

size_t Journal::estimateMemoryUsage(const DeltaState& deltaState) const {
  size_t memoryUsage = folly::goodMallocSize(sizeof(Journal));
  // Account for overhead of dequeues which have a maximum buffer size of 512.
  memoryUsage += getPaddingAmount(deltaState.fileChangeDeltas);
  memoryUsage += getPaddingAmount(deltaState.rootUpdateDeltas);

  if (deltaState.stats) {
    memoryUsage += deltaState.deltaMemoryUsage;
  }
  return memoryUsage;
}

void Journal::flush() {
  bool shouldNotify;
  {
    auto deltaState = deltaState_.wlock();
    ++deltaState->nextSequence;
    auto lastRoot = deltaState->currentRoot;
    deltaState->fileChangeDeltas.clear();
    deltaState->rootUpdateDeltas.clear();
    deltaState->stats = std::nullopt;
    deltaState->resetMaxFilesAccumulated();
    auto delta = RootUpdateJournalDelta();
    /* Tracking the root correctly when the journal is flushed is important
     * since Watchman uses the root to correctly determine what additional files
     * were changed when a checkout happens, journals have at least one entry
     * unless they are on the null commit with no modifications done. A flush
     * operation should leave us on the same checkout we were on before the
     * flush operation.
     */
    delta.fromRoot = lastRoot;
    shouldNotify = addDeltaBeforeNotifying(std::move(delta), *deltaState);
  }
  if (shouldNotify) {
    notifySubscribers();
  }
}

std::optional<SerializedJournal> Journal::serializeForTakeover(
    uint64_t mountGeneration,
    size_t maxBytes) const {
  auto deltaState = deltaState_.rlock();
  if (deltaState->empty()) {
    return std::nullopt;
  }

  SerializedJournal result;
  result.mountGeneration() = mountGeneration;
  result.nextSequence() = deltaState->nextSequence;
  result.currentRoot() = deltaState->currentRoot.value();
  auto& fileChanges = *result.fileChangeDeltas();
  auto& rootUpdates = *result.rootUpdateDeltas();

  // Walk from the newest delta and stop at the first one over the budget. As
  // with truncateIfNecessary(), the deltas left behind are the oldest ones, so
  // the copy answers every position it does not report as truncated.
  size_t bytes = 0;
  auto fits = [&](size_t deltaBytes) {
    bytes += deltaBytes;
    return bytes <= maxBytes || (fileChanges.empty() && rootUpdates.empty());
  };
  forEachDelta(
      *deltaState,
      1,
      std::nullopt,
      [&](const FileChangeJournalDelta& delta) {
        if (!fits(delta.estimateMemoryUsage())) {
          return false;
        }
        auto& serialized = fileChanges.emplace_back();
        serialized.sequenceID() = delta.sequenceID;
        serialized.earliestSequenceID() = delta.earliestSequenceID;
        serialized.timeNanos() = toNanos(delta.time);
        if (delta.isPath1Valid) {
          serialized.path1() = delta.path1.asString();
        }
        serialized.path1ExistedBefore() = delta.info1.existedBefore;
        serialized.path1ExistedAfter() = delta.info1.existedAfter;
        if (delta.isPath2Valid) {
          serialized.path2() = delta.path2.asString();
        }
        serialized.path2ExistedBefore() = delta.info2.existedBefore;
        serialized.path2ExistedAfter() = delta.info2.existedAfter;
        serialized.dtype() = static_cast<int32_t>(delta.type);
        return true;
      },
      [&](const RootUpdateJournalDelta& delta) {
        if (!fits(delta.estimateMemoryUsage())) {
          return false;
        }
        auto& serialized = rootUpdates.emplace_back();
        serialized.sequenceID() = delta.sequenceID;
        serialized.earliestSequenceID() = delta.earliestSequenceID;
        serialized.timeNanos() = toNanos(delta.time);
        serialized.fromRoot() = delta.fromRoot.value();
        serialized.uncleanPaths()->reserve(delta.uncleanPaths.size());
        for (const auto& path : delta.uncleanPaths) {
          serialized.uncleanPaths()->push_back(path.asString());
        }
        return true;
      });
  std::reverse(fileChanges.begin(), fileChanges.end());
  std::reverse(rootUpdates.begin(), rootUpdates.end());
  return result;
}

void Journal::initializeFromTakeover(SerializedJournal&& journal) {
  auto deltaState = deltaState_.wlock();
  XCHECK(deltaState->empty() && deltaState->nextSequence == 1)
      << "a journal can only be restored before anything is recorded";

  deltaState->nextSequence = *journal.nextSequence();
  deltaState->currentRoot = RootId{std::move(*journal.currentRoot())};

  for (auto& serialized : *journal.fileChangeDeltas()) {
    auto& delta = deltaState->fileChangeDeltas.emplace_back();
    delta.sequenceID = *serialized.sequenceID();
    delta.earliestSequenceID = *serialized.earliestSequenceID();
    delta.time = fromNanos(*serialized.timeNanos());
    if (serialized.path1().has_value()) {
      delta.path1 = RelativePath{std::move(*serialized.path1())};
      delta.isPath1Valid = true;
    }
    delta.info1 = PathChangeInfo{
        *serialized.path1ExistedBefore(), *serialized.path1ExistedAfter()};
    if (serialized.path2().has_value()) {
      delta.path2 = RelativePath{std::move(*serialized.path2())};
      delta.isPath2Valid = true;
    }
    delta.info2 = PathChangeInfo{
        *serialized.path2ExistedBefore(), *serialized.path2ExistedAfter()};
    delta.type = static_cast<dtype_t>(*serialized.dtype());
    deltaState->deltaMemoryUsage += delta.estimateMemoryUsage();
  }

  for (auto& serialized : *journal.rootUpdateDeltas()) {
    auto& delta = deltaState->rootUpdateDeltas.emplace_back();
    delta.sequenceID = *serialized.sequenceID();
    delta.earliestSequenceID = *serialized.earliestSequenceID();
    delta.time = fromNanos(*serialized.timeNanos());
    delta.fromRoot = RootId{std::move(*serialized.fromRoot())};
    for (auto& path : *serialized.uncleanPaths()) {
      delta.uncleanPaths.emplace(std::move(path));
    }
    deltaState->deltaMemoryUsage += delta.estimateMemoryUsage();
  }

  if (!deltaState->empty()) {
    InternalJournalStats stats;
    stats.entryCount = deltaState->fileChangeDeltas.size() +
        deltaState->rootUpdateDeltas.size();
    stats.earliestTimestamp = deltaState->frontPtr()->time;
    stats.latestTimestamp = deltaState->backPtr()->time;
    deltaState->stats = stats;
  }
}

std::unique_ptr<JournalDeltaRange> Journal::accumulateRange(
    SequenceNumber from) {
  XDCHECK(from > 0);
  std::unique_ptr<JournalDeltaRange> result = nullptr;
  folly::stop_watch<std::chrono::milliseconds> watch;

  size_t filesAccumulated = 0;
  auto deltaState = deltaState_.rlock();
  // If this is going to be truncated, handle it before iterating.
  if (!deltaState->empty() && deltaState->getFrontSequenceID() > from) {
    result = std::make_unique<JournalDeltaRange>();
    result->isTruncated = true;
  } else {
    forEachDelta(
        *deltaState,
        from,
        std::nullopt,
        [&](const FileChangeJournalDelta& current) -> bool {
          ++filesAccumulated;
          if (!result) {
            result = std::make_unique<JournalDeltaRange>();
            result->toSequence = current.sequenceID;
            result->toTime = current.time;
            result->snapshotTransitions.push_back(deltaState->currentRoot);
          }
          // Capture the lower bound.
          result->fromSequence = current.sequenceID;
          result->fromTime = current.time;

          for (auto& entry : current.getChangedFilesInOverlay()) {
            auto& name = entry.first;
            if (result->containsSaplingOnlyChanges &&
                !isSaplingMetadataPath(name)) {
              result->containsSaplingOnlyChanges = false;
            }
            auto& currentInfo = entry.second;
            auto* resultInfo =
                folly::get_ptr(result->changedFilesInOverlay, name);
            if (!resultInfo) {
              result->changedFilesInOverlay.emplace(name, currentInfo);
            } else {
              // Directory renames can change a child's existence without
              // recording a delta for that child.
              resultInfo->existedBefore = currentInfo.existedBefore;
            }
          }
          // Return value ignored here
          return true;
        },
        [&](const RootUpdateJournalDelta& current) -> bool {
          if (!result) {
            result = std::make_unique<JournalDeltaRange>();
            result->toSequence = current.sequenceID;
            result->toTime = current.time;
            result->snapshotTransitions.push_back(deltaState->currentRoot);
          }
          // Capture the lower bound.
          result->fromSequence = current.sequenceID;
          result->fromTime = current.time;
          result->snapshotTransitions.push_back(current.fromRoot);

          // Merge the unclean status list
          result->uncleanPaths.insert(
              current.uncleanPaths.begin(), current.uncleanPaths.end());
          // Return value ignored here
          return true;
        });
  }

  if (result) {
    if (edenStats_) {
      if (result->isTruncated) {
        edenStats_->increment(&JournalStats::truncatedReads);
      }
      edenStats_->increment(&JournalStats::filesAccumulated, filesAccumulated);
      edenStats_->addDuration(&JournalStats::accumulateRange, watch.elapsed());
    }
    if (deltaState->stats) {
      deltaState->updateMaxFilesAccumulated(filesAccumulated);
    }

    std::reverse(
        result->snapshotTransitions.begin(), result->snapshotTransitions.end());
    result->containsRootUpdate = result->snapshotTransitions.size() > 1;
  }

  deltaState->markObserved();
  return result;
}

bool Journal::containsOnlySaplingChanges(SequenceNumber limitSequence) {
  XDCHECK(limitSequence > 0);

  auto deltaState = deltaState_.rlock();
  if (!deltaState->empty() &&
      deltaState->getFrontSequenceID() > limitSequence) {
    // The range was truncated: it may have contained anything.
    // Mark the journal observed even on this early return, like
    // accumulateRange does for its truncated result: this call counts as
    // an observation, and skipping it would let the next addDelta swallow
    // the subscriber notification.
    deltaState->markObserved();
    return false;
  }

  bool saplingOnly = true;
  forEachDelta(
      *deltaState,
      limitSequence,
      std::nullopt,
      [&](const FileChangeJournalDelta& current) -> bool {
        if ((current.isPath1Valid && !isSaplingMetadataPath(current.path1)) ||
            (current.isPath2Valid && !isSaplingMetadataPath(current.path2))) {
          saplingOnly = false;
          return false;
        }
        return true;
      },
      [&](const RootUpdateJournalDelta&) -> bool {
        saplingOnly = false;
        return false;
      });

  deltaState->markObserved();
  return saplingOnly;
}

bool Journal::forEachDelta(
    SequenceNumber from,
    std::optional<size_t> lengthLimit,
    FileChangeCallback&& fileChangeCallback,
    RootUpdateCallback&& rootUpdateCallback) {
  XDCHECK(from > 0);
  auto deltaState = deltaState_.rlock();
  // If this is going to be truncated, handle it before iterating.
  if (!deltaState->empty() && deltaState->getFrontSequenceID() > from) {
    return true;
  } else {
    forEachDelta(
        *deltaState,
        from,
        lengthLimit,
        std::forward<FileChangeCallback>(fileChangeCallback),
        std::forward<RootUpdateCallback>(rootUpdateCallback));
  }
  deltaState->markObserved();
  return false;
}

std::vector<DebugJournalDelta> Journal::getDebugRawJournalInfo(
    SequenceNumber from,
    std::optional<size_t> limit,
    long mountGeneration,
    RootIdCodec& rootIdCodec) const {
  auto result = std::vector<DebugJournalDelta>();
  auto deltaState = deltaState_.rlock();
  RootId currentRoot = deltaState->currentRoot;
  forEachDelta(
      *deltaState,
      from,
      limit,
      [&](const FileChangeJournalDelta& current) -> bool {
        DebugJournalDelta delta;
        JournalPosition fromPosition;
        fromPosition.mountGeneration() = mountGeneration;
        fromPosition.sequenceNumber() = current.sequenceID;
        fromPosition.snapshotHash() = rootIdCodec.renderRootId(currentRoot);
        delta.fromPosition() = fromPosition;

        JournalPosition toPosition;
        toPosition.mountGeneration() = mountGeneration;
        toPosition.sequenceNumber() = current.sequenceID;
        toPosition.snapshotHash() = rootIdCodec.renderRootId(currentRoot);
        delta.toPosition() = toPosition;

        for (const auto& entry : current.getChangedFilesInOverlay()) {
          auto& path = entry.first;
          auto& changeInfo = entry.second;

          DebugPathChangeInfo debugChangeInfo;
          debugChangeInfo.existedBefore() = changeInfo.existedBefore;
          debugChangeInfo.existedAfter() = changeInfo.existedAfter;
          delta.changedPaths()->emplace(path.asString(), debugChangeInfo);
        }

        result.push_back(delta);
        // Return value ignored here
        return true;
      },
      [&](const RootUpdateJournalDelta& current) -> bool {
        DebugJournalDelta delta;
        JournalPosition fromPosition;
        fromPosition.mountGeneration() = mountGeneration;
        fromPosition.sequenceNumber() = current.sequenceID;
        fromPosition.snapshotHash() =
            rootIdCodec.renderRootId(current.fromRoot);
        delta.fromPosition() = fromPosition;

        JournalPosition toPosition;
        toPosition.mountGeneration() = mountGeneration;
        toPosition.sequenceNumber() = current.sequenceID;
        toPosition.snapshotHash() = rootIdCodec.renderRootId(currentRoot);
        delta.toPosition() = toPosition;
        currentRoot = current.fromRoot;

        for (auto& path : current.uncleanPaths) {
          delta.uncleanPaths()->emplace(path.asString());
        }

        result.push_back(delta);
        // Return value ignored here
        return true;
      });
  return result;
}

void Journal::forEachDelta(
    const DeltaState& deltaState,
    JournalDelta::SequenceNumber from,
    std::optional<size_t> lengthLimit,
    FileChangeCallback&& fileChangeDeltaCallback,
    RootUpdateCallback&& rootUpdateDeltaCallback) const {
  size_t iters = 0;
  auto fileChangeIt = deltaState.fileChangeDeltas.rbegin();
  auto rootUpdateIt = deltaState.rootUpdateDeltas.rbegin();
  auto fileChangeRend = deltaState.fileChangeDeltas.rend();
  auto rootUpdateRend = deltaState.rootUpdateDeltas.rend();
  while (fileChangeIt != fileChangeRend || rootUpdateIt != rootUpdateRend) {
    bool isFileChange = rootUpdateIt == rootUpdateRend ||
        (fileChangeIt != fileChangeRend &&
         fileChangeIt->sequenceID > rootUpdateIt->sequenceID);
    const Journal::SequenceNumber currentSequenceID =
        isFileChange ? fileChangeIt->sequenceID : rootUpdateIt->sequenceID;
    if (currentSequenceID < from) {
      break;
    }
    if (lengthLimit && iters >= lengthLimit.value()) {
      break;
    }
    if (isFileChange) {
      if (!fileChangeDeltaCallback(*fileChangeIt)) {
        break;
      };
      ++fileChangeIt;
    } else {
      if (!rootUpdateDeltaCallback(*rootUpdateIt)) {
        break;
      }
      ++rootUpdateIt;
    }

    ++iters;
  }
}
} // namespace facebook::eden
