/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/journal/Journal.h"

#include <fmt/format.h>
#include <fmt/ranges.h> // NOLINT(facebook-unused-include-check)
#include <folly/test/TestUtils.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <algorithm>
#include <limits>

#include "eden/common/utils/FaultInjector.h"
#include "eden/fs/model/RootId.h"

using namespace facebook::eden;

namespace {

struct IdentityCodec : RootIdCodec {
  RootId parseRootId(folly::StringPiece piece) override {
    return RootId{piece.toString()};
  }
  std::string renderRootId(const RootId& rootId) override {
    return rootId.value();
  }
};

struct JournalTest : ::testing::Test {
  EdenStatsPtr edenStats{makeRefPtr<EdenStats>()};
  Journal journal{edenStats.copy()};
  IdentityCodec codec;
};

TEST_F(JournalTest, containsOnlySaplingChanges) {
  EXPECT_TRUE(journal.containsOnlySaplingChanges(1));

  journal.recordChanged(".sl/wlock"_relpath, dtype_t::Regular);
  journal.recordChanged(".sl/blackbox.log"_relpath, dtype_t::Regular);
  EXPECT_TRUE(journal.containsOnlySaplingChanges(1));

  journal.recordChanged("foo/bar.txt"_relpath, dtype_t::Regular);
  auto afterNonSapling = journal.observeLatest()->sequenceID;
  EXPECT_FALSE(journal.containsOnlySaplingChanges(1));
  journal.recordChanged(".sl/dirstate"_relpath, dtype_t::Regular);
  EXPECT_FALSE(journal.containsOnlySaplingChanges(afterNonSapling));
  EXPECT_TRUE(journal.containsOnlySaplingChanges(afterNonSapling + 1));
}

TEST_F(JournalTest, containsOnlySaplingChanges_legacy_hg_directory) {
  journal.recordChanged(".hg/dirstate"_relpath, dtype_t::Regular);
  EXPECT_TRUE(journal.containsOnlySaplingChanges(1));
}

TEST_F(JournalTest, containsOnlySaplingChanges_rename_out_of_metadata) {
  journal.recordRenamed(
      ".sl/origbackups/a"_relpath, "restored/a"_relpath, dtype_t::Regular);
  EXPECT_FALSE(journal.containsOnlySaplingChanges(1));
}

TEST_F(JournalTest, containsOnlySaplingChanges_root_update) {
  journal.recordChanged(".sl/wlock"_relpath, dtype_t::Regular);
  journal.recordRootUpdate(RootId{"1111111111111111111111111111111111111111"});
  EXPECT_FALSE(journal.containsOnlySaplingChanges(1));
}

TEST_F(JournalTest, containsOnlySaplingChanges_truncation) {
  journal.recordChanged(".sl/wlock"_relpath, dtype_t::Regular);
  journal.flush();
  // The range starting at the flushed-away sequence was truncated: nothing
  // can be said about what it contained.
  EXPECT_FALSE(journal.containsOnlySaplingChanges(1));
}

TEST_F(
    JournalTest,
    containsOnlySaplingChanges_truncated_probe_arms_notification) {
  // Subscriber notifications are edge-triggered: after a notification,
  // further changes do not notify again until someone observes the
  // journal. Like accumulateRange, a containsOnlySaplingChanges probe counts
  // as an observation even when the range was truncated, so the change
  // after the probe must notify.
  unsigned calls = 0;
  auto sub = journal.registerSubscriber([&] { ++calls; });
  (void)sub;

  journal.recordChanged("foo"_relpath, dtype_t::Regular);
  journal.flush();
  auto callsBeforeProbe = calls;
  EXPECT_FALSE(journal.containsOnlySaplingChanges(1));
  journal.recordChanged("bar"_relpath, dtype_t::Regular);
  EXPECT_EQ(callsBeforeProbe + 1, calls);
}

TEST_F(JournalTest, containsOnlySaplingChanges_matches_accumulateRange) {
  auto viaAccumulateRange = [&](JournalDelta::SequenceNumber from) {
    auto range = journal.accumulateRange(from);
    if (!range) {
      return false;
    }
    return !range->isTruncated && range->containsSaplingOnlyChanges &&
        !range->containsRootUpdate;
  };

  journal.recordChanged(".sl/wlock"_relpath, dtype_t::Regular);
  journal.recordChanged(".hg/dirstate"_relpath, dtype_t::Regular);
  EXPECT_EQ(viaAccumulateRange(1), journal.containsOnlySaplingChanges(1));

  journal.recordChanged("outside.txt"_relpath, dtype_t::Regular);
  EXPECT_EQ(viaAccumulateRange(1), journal.containsOnlySaplingChanges(1));

  journal.recordRootUpdate(RootId{"1111111111111111111111111111111111111111"});
  EXPECT_EQ(viaAccumulateRange(1), journal.containsOnlySaplingChanges(1));
}

TEST_F(JournalTest, recordFaultUsesPathBeforeAdvancingSequence) {
  FaultInjector injector{true};
  Journal injectedJournal{edenStats.copy(), &injector};
  injector.injectError(
      "journal-record",
      "file[.]txt",
      std::runtime_error("journal record blocked"),
      1);
  injectedJournal.recordChanged("other.txt"_relpath, dtype_t::Regular);
  EXPECT_EQ(1, injectedJournal.peekLatest()->sequenceID);
  EXPECT_THROW_RE(
      injectedJournal.recordChanged("file.txt"_relpath, dtype_t::Regular),
      std::runtime_error,
      "journal record blocked");
  EXPECT_EQ(1, injectedJournal.peekLatest()->sequenceID);
  injectedJournal.recordChanged("file.txt"_relpath, dtype_t::Regular);
  EXPECT_EQ(2, injectedJournal.peekLatest()->sequenceID);
}

struct JournalDeltaTest : ::testing::Test {
  EdenStatsPtr edenStats{makeRefPtr<EdenStats>()};
  Journal journal{edenStats.copy()};
  IdentityCodec codec;

  RootId root0;
  RootId root1{"1111111111111111111111111111111111111111"};
  RootId root2{"2222222222222222222222222222222222222222"};
  RootId root3{"3333333333333333333333333333333333333333"};

  std::vector<int> expectedFileChangeSequences;
  std::vector<RelativePathPiece> expectedFileChangeNames;
  std::vector<dtype_t> expectedFileChangeDtypes;
  std::vector<int> expectedRootUpdateSequences;
  std::vector<RootId> expectedRootUpdateRoots;

  std::vector<int> fileChangeSequences;
  std::vector<RelativePathPiece> fileChangeNames;
  std::vector<dtype_t> fileChangeDtypes;
  std::vector<int> rootUpdateSequences;
  std::vector<RootId> rootUpdateRoots;

  void addFileChange(
      RelativePathPiece path,
      dtype_t dtype,
      JournalDelta::SequenceNumber after = 0) {
    journal.recordChanged(path, dtype);
    if (journal.observeLatest()->sequenceID >= after) {
      expectedFileChangeNames.push_back(path);
      expectedFileChangeDtypes.push_back(dtype);
      expectedFileChangeSequences.push_back(
          journal.observeLatest()->sequenceID);
    }
  }

  void flush() {
    journal.flush();
    expectedFileChangeDtypes.clear();
    expectedFileChangeNames.clear();
    expectedFileChangeSequences.clear();
    expectedRootUpdateRoots.clear();
    expectedRootUpdateSequences.clear();

    expectedRootUpdateSequences.push_back(journal.observeLatest()->sequenceID);
    expectedRootUpdateRoots.push_back(journal.observeLatest()->toRoot);
  }

  void addRootUpdate(RootId to, JournalDelta::SequenceNumber after = 0) {
    addRootUpdate(root0, std::move(to), after);
  }

  void addRootUpdate(
      const RootId& from,
      RootId to,
      JournalDelta::SequenceNumber after = 0) {
    journal.recordRootUpdate(from, std::move(to));
    if (journal.observeLatest()->sequenceID >= after) {
      expectedRootUpdateSequences.push_back(
          journal.observeLatest()->sequenceID);
      expectedRootUpdateRoots.push_back(from);
    }
  }

  void checkExpect() {
    EXPECT_EQ(expectedFileChangeSequences, fileChangeSequences);
    EXPECT_EQ(expectedFileChangeNames, fileChangeNames);
    EXPECT_EQ(expectedFileChangeDtypes, fileChangeDtypes);
    EXPECT_EQ(expectedRootUpdateSequences, rootUpdateSequences);
    EXPECT_EQ(expectedRootUpdateRoots, rootUpdateRoots);
  }

  void reverseResults() {
    std::reverse(fileChangeSequences.begin(), fileChangeSequences.end());
    std::reverse(fileChangeNames.begin(), fileChangeNames.end());
    std::reverse(fileChangeDtypes.begin(), fileChangeDtypes.end());
    std::reverse(rootUpdateSequences.begin(), rootUpdateSequences.end());
    std::reverse(rootUpdateRoots.begin(), rootUpdateRoots.end());
  }

  /*
    This sets the journal state to be in a post-flush state.
    The current root will be set to root1
    The current sequence will be set to 5
  */
  void setupFlushedJournal() {
    journal.recordRootUpdate(root1);
    journal.recordChanged("foo1"_relpath, dtype_t::Regular);
    journal.recordChanged("foo2"_relpath, dtype_t::Symlink);
    flush();
  }

  /*
   * Set up journal state with a mix of fileChanges and rootUpdates
   */
  void setupGeneric(JournalDelta::SequenceNumber after) {
    addRootUpdate(root1, after);
    addFileChange("foo1"_relpath, dtype_t::Regular, after);
    addFileChange("foo2"_relpath, dtype_t::Regular, after);
    addFileChange("foo1"_relpath, dtype_t::Regular, after);
    addFileChange("foo2"_relpath, dtype_t::Regular, after);
    EXPECT_EQ(5u, journal.observeLatest()->sequenceID);
    addFileChange("foo3"_relpath, dtype_t::Regular, after);
    addFileChange("foo4"_relpath, dtype_t::Regular, after);
    EXPECT_EQ(7u, journal.observeLatest()->sequenceID);
    addRootUpdate(root1, root2, after);
    addRootUpdate(root2, root1, after);
    EXPECT_EQ(9u, journal.observeLatest()->sequenceID);
  }
};

} // namespace

TEST_F(JournalTest, accumulate_range_all_changes) {
  // Empty journals have no rang to accumulate over
  EXPECT_FALSE(journal.observeLatest());
  EXPECT_EQ(nullptr, journal.accumulateRange());

  // Make an initial entry.
  journal.recordChanged("foo/bar"_relpath, dtype_t::Dir);

  // Sanity check that the latest information matches.
  auto latest = journal.observeLatest();
  ASSERT_TRUE(latest);
  EXPECT_EQ(1, latest->sequenceID);

  // Add a second entry.
  journal.recordChanged("baz"_relpath, dtype_t::Dir);

  // Sanity check that the latest information matches.
  latest = journal.observeLatest();
  ASSERT_TRUE(latest);
  EXPECT_EQ(2, latest->sequenceID);

  // Check basic sum implementation.
  auto summed = journal.accumulateRange();
  ASSERT_NE(nullptr, summed);
  EXPECT_EQ(1, summed->fromSequence);
  EXPECT_EQ(2, summed->toSequence);
  EXPECT_EQ(2, summed->changedFilesInOverlay.size());

  // First just report the most recent item.
  summed = journal.accumulateRange(2);
  ASSERT_NE(nullptr, summed);
  EXPECT_EQ(2, summed->fromSequence);
  EXPECT_EQ(2, summed->toSequence);
  EXPECT_EQ(1, summed->changedFilesInOverlay.size());

  // Merge the first two entries.
  summed = journal.accumulateRange(1);
  ASSERT_NE(nullptr, summed);
  EXPECT_EQ(1, summed->fromSequence);
  EXPECT_EQ(2, summed->toSequence);
  EXPECT_EQ(2, summed->changedFilesInOverlay.size());
}

TEST_F(JournalTest, accumulateRangeSaplingMetadataChanges) {
  // Empty journals have no rang to accumulate over
  EXPECT_FALSE(journal.observeLatest());
  EXPECT_EQ(nullptr, journal.accumulateRange());

  // Make an initial entry.
  journal.recordChanged("foo/bar"_relpath, dtype_t::Dir);

  // Sanity check that the latest information matches.
  auto latest = journal.observeLatest();

  // get accumulated data for the tip of journal
  auto summed = journal.accumulateRange(latest->sequenceID);
  if (!summed) {
    ADD_FAILURE() << "journal range should contain the recorded change";
    return;
  }
  EXPECT_FALSE(summed->containsSaplingOnlyChanges);

  journal.recordChanged(".sl/foo/bar"_relpath, dtype_t::Dir);
  journal.recordChanged(".hg/foo/baz"_relpath, dtype_t::Dir);

  // get accumulated data for the tip of journal
  latest = journal.observeLatest();
  summed = journal.accumulateRange(latest->sequenceID);
  if (!summed) {
    ADD_FAILURE() << "journal range should contain Sapling metadata changes";
    return;
  }
  EXPECT_TRUE(summed->containsSaplingOnlyChanges);

  // get accumulated data from the beginning.
  summed = journal.accumulateRange();
  if (!summed) {
    ADD_FAILURE() << "journal range should contain all recorded changes";
    return;
  }
  EXPECT_FALSE(summed->containsSaplingOnlyChanges);
}

TEST_F(JournalTest, accumulateRangeRemoveCreateUpdate) {
  // Remove test.txt
  journal.recordRemoved("test.txt"_relpath, dtype_t::Regular);
  // Create test.txt
  journal.recordCreated("test.txt"_relpath, dtype_t::Regular);
  // Modify test.txt
  journal.recordChanged("test.txt"_relpath, dtype_t::Regular);

  // Sanity check that the latest information matches.
  auto latest = journal.observeLatest();
  ASSERT_TRUE(latest);
  EXPECT_EQ(3, latest->sequenceID);

  // The summed data should report test.txt as changed
  auto summed = journal.accumulateRange();
  ASSERT_NE(nullptr, summed);
  EXPECT_EQ(1, summed->fromSequence);
  EXPECT_EQ(3, summed->toSequence);
  EXPECT_EQ(1, summed->changedFilesInOverlay.size());
  ASSERT_TRUE(summed->changedFilesInOverlay.contains(RelativePath{"test.txt"}));
  EXPECT_EQ(
      true,
      summed->changedFilesInOverlay[RelativePath{"test.txt"}].existedBefore);
  EXPECT_EQ(
      true,
      summed->changedFilesInOverlay[RelativePath{"test.txt"}].existedAfter);

  // Test merging only partway back
  summed = journal.accumulateRange(3);
  ASSERT_NE(nullptr, summed);
  EXPECT_EQ(3, summed->fromSequence);
  EXPECT_EQ(3, summed->toSequence);
  EXPECT_EQ(1, summed->changedFilesInOverlay.size());
  ASSERT_TRUE(summed->changedFilesInOverlay.contains(RelativePath{"test.txt"}));
  EXPECT_EQ(
      true,
      summed->changedFilesInOverlay[RelativePath{"test.txt"}].existedBefore);
  EXPECT_EQ(
      true,
      summed->changedFilesInOverlay[RelativePath{"test.txt"}].existedAfter);

  summed = journal.accumulateRange(2);
  ASSERT_NE(nullptr, summed);
  EXPECT_EQ(2, summed->fromSequence);
  EXPECT_EQ(3, summed->toSequence);
  EXPECT_EQ(1, summed->changedFilesInOverlay.size());
  ASSERT_TRUE(summed->changedFilesInOverlay.contains(RelativePath{"test.txt"}));
  EXPECT_EQ(
      false,
      summed->changedFilesInOverlay[RelativePath{"test.txt"}].existedBefore);
  EXPECT_EQ(
      true,
      summed->changedFilesInOverlay[RelativePath{"test.txt"}].existedAfter);

  summed = journal.accumulateRange(1);
  ASSERT_NE(nullptr, summed);
  EXPECT_EQ(1, summed->fromSequence);
  EXPECT_EQ(3, summed->toSequence);
  EXPECT_EQ(1, summed->changedFilesInOverlay.size());
  ASSERT_TRUE(summed->changedFilesInOverlay.contains(RelativePath{"test.txt"}));
  EXPECT_EQ(
      true,
      summed->changedFilesInOverlay[RelativePath{"test.txt"}].existedBefore);
  EXPECT_EQ(
      true,
      summed->changedFilesInOverlay[RelativePath{"test.txt"}].existedAfter);
}

namespace {

void checkRootMatches(
    const std::vector<RootId>& transitions,
    Journal& journal) {
  auto latest = journal.observeLatest();
  ASSERT_TRUE(latest);
  EXPECT_EQ(transitions.front(), latest->fromRoot);
  EXPECT_EQ(transitions.back(), latest->toRoot);

  auto range = journal.accumulateRange(latest->sequenceID);
  ASSERT_TRUE(range);
  EXPECT_EQ(transitions, range->snapshotTransitions);
  if (transitions.size() > 1) {
    EXPECT_TRUE(range->containsRootUpdate);
  } else {
    EXPECT_FALSE(range->containsRootUpdate);
  }

  range = journal.accumulateRange();
  ASSERT_TRUE(range);
  EXPECT_EQ(RootId{}, range->snapshotTransitions.front());
  EXPECT_EQ(transitions.back(), range->snapshotTransitions.back());
}

} // namespace

TEST_F(JournalTest, accumulate_range_with_hash_updates) {
  RootId root0;
  RootId root1{"1111111111111111111111111111111111111111"};
  RootId root2{"2222222222222222222222222222222222222222"};
  // Empty journals have no range to accumulate over
  EXPECT_FALSE(journal.observeLatest());
  EXPECT_EQ(nullptr, journal.accumulateRange());

  // Make an initial entry.
  journal.recordChanged("foo/bar"_relpath, dtype_t::Dir);
  checkRootMatches({root0}, journal);

  // Update to a new root using 'to' syntax
  journal.recordRootUpdate(root1);
  checkRootMatches({root0, root1}, journal);

  journal.recordChanged("foo/bar"_relpath, dtype_t::Dir);
  checkRootMatches({root1}, journal);

  // Update to a new root using 'from/to' syntax
  journal.recordRootUpdate(root1, root2);
  checkRootMatches({root1, root2}, journal);

  journal.recordChanged("foo/bar"_relpath, dtype_t::Dir);
  checkRootMatches({root2}, journal);

  auto uncleanPaths = std::unordered_set<RelativePath>();
  uncleanPaths.insert(RelativePath("foo/bar"));
  journal.recordUncleanPaths(root2, root1, std::move(uncleanPaths));
  checkRootMatches({root2, root1}, journal);

  journal.recordChanged("foo/bar"_relpath, dtype_t::Dir);
  checkRootMatches({root1}, journal);
}

TEST_F(JournalTest, debugRawJournalInfoRemoveCreateUpdate) {
  // Remove test.txt
  journal.recordRemoved("test.txt"_relpath, dtype_t::Regular);
  // Create test.txt
  journal.recordCreated("test.txt"_relpath, dtype_t::Regular);
  // Modify test.txt
  journal.recordChanged("test.txt"_relpath, dtype_t::Regular);

  long mountGen = 333;

  auto debugDeltas = journal.getDebugRawJournalInfo(0, 3, mountGen, codec);
  ASSERT_EQ(3, debugDeltas.size());

  // Debug Raw Journal Info returns info from newest->latest
  EXPECT_TRUE(*debugDeltas[0].changedPaths()["test.txt"].existedBefore());
  EXPECT_TRUE(*debugDeltas[0].changedPaths()["test.txt"].existedAfter());
  EXPECT_EQ(*debugDeltas[0].fromPosition()->mountGeneration(), mountGen);
  EXPECT_EQ(*debugDeltas[0].fromPosition()->sequenceNumber(), 3);
  EXPECT_FALSE(*debugDeltas[1].changedPaths()["test.txt"].existedBefore());
  EXPECT_TRUE(*debugDeltas[1].changedPaths()["test.txt"].existedAfter());
  EXPECT_EQ(*debugDeltas[1].fromPosition()->mountGeneration(), mountGen);
  EXPECT_EQ(*debugDeltas[1].fromPosition()->sequenceNumber(), 2);
  EXPECT_TRUE(*debugDeltas[2].changedPaths()["test.txt"].existedBefore());
  EXPECT_FALSE(*debugDeltas[2].changedPaths()["test.txt"].existedAfter());
  EXPECT_EQ(*debugDeltas[2].fromPosition()->mountGeneration(), mountGen);
  EXPECT_EQ(*debugDeltas[2].fromPosition()->sequenceNumber(), 1);

  debugDeltas = journal.getDebugRawJournalInfo(0, 1, mountGen, codec);
  ASSERT_EQ(1, debugDeltas.size());
  EXPECT_TRUE(*debugDeltas[0].changedPaths()["test.txt"].existedBefore());
  EXPECT_TRUE(*debugDeltas[0].changedPaths()["test.txt"].existedAfter());
  EXPECT_EQ(*debugDeltas[0].fromPosition()->mountGeneration(), mountGen);
  EXPECT_EQ(*debugDeltas[0].fromPosition()->sequenceNumber(), 3);

  debugDeltas = journal.getDebugRawJournalInfo(0, 0, mountGen, codec);
  ASSERT_EQ(0, debugDeltas.size());
}

TEST_F(JournalTest, debugRawJournalInfoHashUpdates) {
  auto root0 = RootId{};
  auto root1 = RootId{"1111111111111111111111111111111111111111"};
  auto root2 = RootId{"2222222222222222222222222222222222222222"};

  // Go from root0 to root1
  journal.recordRootUpdate(root0, root1);
  // Create test.txt
  journal.recordCreated("test.txt"_relpath, dtype_t::Regular);
  // Go from root1 to root2
  journal.recordRootUpdate(root1, root2);

  long mountGen = 333;

  auto debugDeltas = journal.getDebugRawJournalInfo(0, 3, mountGen, codec);
  ASSERT_EQ(3, debugDeltas.size());

  // Debug Raw Journal Info returns info from newest->latest
  EXPECT_TRUE(debugDeltas[0].changedPaths()->empty());
  EXPECT_EQ(*debugDeltas[0].fromPosition()->mountGeneration(), mountGen);
  EXPECT_EQ(*debugDeltas[0].fromPosition()->sequenceNumber(), 3);
  EXPECT_EQ(*debugDeltas[0].fromPosition()->snapshotHash(), root1.value());
  EXPECT_EQ(*debugDeltas[0].toPosition()->snapshotHash(), root2.value());
  EXPECT_FALSE(*debugDeltas[1].changedPaths()["test.txt"].existedBefore());
  EXPECT_TRUE(*debugDeltas[1].changedPaths()["test.txt"].existedAfter());
  EXPECT_EQ(*debugDeltas[1].fromPosition()->mountGeneration(), mountGen);
  EXPECT_EQ(*debugDeltas[1].fromPosition()->sequenceNumber(), 2);
  EXPECT_EQ(*debugDeltas[1].fromPosition()->snapshotHash(), root1.value());
  EXPECT_EQ(*debugDeltas[1].toPosition()->snapshotHash(), root1.value());
  EXPECT_TRUE(debugDeltas[2].changedPaths()->empty());
  EXPECT_EQ(*debugDeltas[2].fromPosition()->mountGeneration(), mountGen);
  EXPECT_EQ(*debugDeltas[2].fromPosition()->sequenceNumber(), 1);
  EXPECT_EQ(*debugDeltas[2].fromPosition()->snapshotHash(), root0.value());
  EXPECT_EQ(*debugDeltas[2].toPosition()->snapshotHash(), root1.value());
}

TEST_F(JournalTest, destruction_does_not_overflow_stack_on_long_chain) {
  size_t N =
#ifdef NDEBUG
      200000 // Passes in under 200ms.
#else
      40000 // Passes in under 400ms.
#endif
      ;
  for (size_t i = 0; i < N; ++i) {
    journal.recordChanged("foo/bar"_relpath, dtype_t::Dir);
  }
}

TEST_F(JournalTest, empty_journal_returns_none_for_stats) {
  auto stats = journal.getStats();
  ASSERT_FALSE(stats.has_value());
}

TEST_F(JournalTest, basic_journal_stats) {
  // Journal with 1 entry
  journal.recordRemoved("test.txt"_relpath, dtype_t::Regular);
  ASSERT_TRUE(journal.observeLatest());
  auto from1 = journal.observeLatest()->time;
  auto to1 = journal.observeLatest()->time;
  auto stats = journal.getStats();
  ASSERT_TRUE(stats.has_value());
  ASSERT_EQ(1, stats->entryCount);
  ASSERT_EQ(from1, stats->earliestTimestamp);
  ASSERT_EQ(to1, stats->latestTimestamp);

  // Journal with 2 entries
  journal.recordCreated("test.txt"_relpath, dtype_t::Regular);
  stats = journal.getStats();
  ASSERT_TRUE(journal.observeLatest());
  auto to2 = journal.observeLatest()->time;
  ASSERT_TRUE(stats.has_value());
  ASSERT_EQ(2, stats->entryCount);
  ASSERT_EQ(from1, stats->earliestTimestamp);
  ASSERT_EQ(to2, stats->latestTimestamp);
}

TEST_F(JournalTest, truncated_read_stats) {
  journal.setMemoryLimit(0);
  journal.recordCreated("test1.txt"_relpath, dtype_t::Regular);
  journal.recordRemoved("test1.txt"_relpath, dtype_t::Regular);

  auto data = facebook::fb303::ServiceData::get();
  constexpr folly::StringPiece key = "journal.truncated_reads.sum";
  edenStats->flush();
  auto initialValue = data->getCounter(key);

  // Empty Accumulate range, should be 0 files accumulated
  journal.accumulateRange(3);
  edenStats->flush();
  ASSERT_EQ(0, data->getCounter(key) - initialValue);

  // This is not a truncated read since journal remembers at least one entry
  journal.accumulateRange(2);
  edenStats->flush();
  ASSERT_EQ(0, data->getCounter(key) - initialValue);

  journal.accumulateRange(1);
  edenStats->flush();
  ASSERT_EQ(1, data->getCounter(key) - initialValue);

  journal.accumulateRange(2);
  edenStats->flush();
  ASSERT_EQ(1, data->getCounter(key) - initialValue);

  journal.accumulateRange(1);
  edenStats->flush();
  ASSERT_EQ(2, data->getCounter(key) - initialValue);
}

TEST_F(JournalTest, files_accumulated_stats) {
  journal.recordCreated("test1.txt"_relpath, dtype_t::Regular);
  journal.recordRemoved("test1.txt"_relpath, dtype_t::Regular);

  auto data = facebook::fb303::ServiceData::get();
  constexpr folly::StringPiece key = "journal.files_accumulated.sum";
  edenStats->flush();
  auto initialValue = data->getCounter(key);
  ASSERT_EQ(0, journal.getStats()->maxFilesAccumulated);

  // Empty Accumulate range, should be 0 files accumulated
  journal.accumulateRange(3);
  edenStats->flush();
  ASSERT_EQ(0, data->getCounter(key) - initialValue);
  ASSERT_EQ(0, journal.getStats()->maxFilesAccumulated);

  journal.accumulateRange(2);
  edenStats->flush();
  ASSERT_EQ(1, data->getCounter(key) - initialValue);
  ASSERT_EQ(1, journal.getStats()->maxFilesAccumulated);

  journal.accumulateRange(1);
  edenStats->flush();
  ASSERT_EQ(3, data->getCounter(key) - initialValue);
  ASSERT_EQ(2, journal.getStats()->maxFilesAccumulated);

  journal.accumulateRange(2);
  edenStats->flush();
  ASSERT_EQ(4, data->getCounter(key) - initialValue);
  ASSERT_EQ(2, journal.getStats()->maxFilesAccumulated);
}

TEST_F(JournalTest, memory_usage) {
  auto stats = journal.getStats();
  uint64_t prevMem = journal.estimateMemoryUsage();
  for (int i = 0; i < 10; i++) {
    if (i % 2 == 0) {
      journal.recordCreated("test.txt"_relpath, dtype_t::Regular);
    } else {
      journal.recordRemoved("test.txt"_relpath, dtype_t::Regular);
    }
    stats = journal.getStats();
    uint64_t newMem = journal.estimateMemoryUsage();
    ASSERT_GT(newMem, prevMem);
    prevMem = newMem;
  }
}

TEST_F(JournalTest, set_get_memory_limit) {
  journal.setMemoryLimit(500);
  ASSERT_EQ(500, journal.getMemoryLimit());
  journal.setMemoryLimit(333);
  ASSERT_EQ(333, journal.getMemoryLimit());
  journal.setMemoryLimit(0);
  ASSERT_EQ(0, journal.getMemoryLimit());
}

TEST_F(JournalTest, truncation_by_flush) {
  journal.recordCreated("file1.txt"_relpath, dtype_t::Regular);
  journal.recordCreated("file2.txt"_relpath, dtype_t::Regular);
  journal.recordCreated("file3.txt"_relpath, dtype_t::Regular);
  auto summed = journal.accumulateRange(1);
  ASSERT_TRUE(summed);
  EXPECT_FALSE(summed->isTruncated);
  journal.flush();
  summed = journal.accumulateRange(1);
  ASSERT_TRUE(summed);
  EXPECT_TRUE(summed->isTruncated);
}

TEST_F(JournalTest, limit_of_zero_holds_one_entry) {
  // Even though limit is 0, journal will always remember at least one entry
  journal.setMemoryLimit(0);
  // With 1 file we should be able to accumulate from anywhere without
  // truncation, nullptr returned for sequenceID's > 1 (empty ranges)
  journal.recordCreated("file1.txt"_relpath, dtype_t::Regular);
  auto summed = journal.accumulateRange(1);
  ASSERT_TRUE(summed);
  EXPECT_FALSE(summed->isTruncated);
  summed = journal.accumulateRange(2);
  EXPECT_FALSE(summed);
}

TEST_F(JournalTest, limit_of_zero_truncates_after_one_entry) {
  // Even though limit is 0, journal will always remember at least one entry
  journal.setMemoryLimit(0);
  // With 2 files but only one entry in the journal we can only accumulate from
  // sequenceID 2 and above without truncation, nullptr returned for
  // sequenceID's > 2 (empty ranges)
  journal.recordCreated("file1.txt"_relpath, dtype_t::Regular);
  journal.recordCreated("file2.txt"_relpath, dtype_t::Regular);
  auto summed = journal.accumulateRange(1);
  ASSERT_TRUE(summed);
  EXPECT_TRUE(summed->isTruncated);
  summed = journal.accumulateRange(2);
  ASSERT_TRUE(summed);
  EXPECT_FALSE(summed->isTruncated);
  summed = journal.accumulateRange(3);
  EXPECT_FALSE(summed);
}

TEST_F(JournalTest, truncation_nonzero) {
  // Set the journal to a size such that it can store a few entries
  journal.setMemoryLimit(1500);
  int totalEntries = 0;
  int rememberedEntries;
  // Keep looping until we get a decent amount of truncation
  do {
    if (totalEntries % 2 == 0) {
      journal.recordCreated("file1.txt"_relpath, dtype_t::Regular);
    } else {
      journal.recordRemoved("file1.txt"_relpath, dtype_t::Regular);
    }
    ++totalEntries;
    rememberedEntries = journal.getStats()->entryCount;
    auto firstUntruncatedEntry = totalEntries - rememberedEntries + 1;
    for (int j = 1; j < firstUntruncatedEntry; j++) {
      auto summed = journal.accumulateRange(j);
      ASSERT_TRUE(summed);
      // If the value we are accumulating from is more than rememberedEntries
      // from the current sequenceID then it should be truncated
      EXPECT_TRUE(summed->isTruncated)
          << "Failed when remembering " << rememberedEntries
          << " entries out of " << totalEntries
          << " total entries with j = " << j;
    }
    for (int j = firstUntruncatedEntry; j <= totalEntries; j++) {
      auto summed = journal.accumulateRange(j);
      ASSERT_TRUE(summed);
      // If the value we are accumulating from is less than or equal to
      // rememberedEntries from the current sequenceID then it should not be
      // truncated
      EXPECT_FALSE(summed->isTruncated)
          << "Failed when remembering " << rememberedEntries
          << " entries out of " << totalEntries
          << " total entries with j = " << j;
    }
  } while (rememberedEntries + 5 > totalEntries);
}

namespace {

/**
 * Checks that `restored` answers every position from `from` on the way
 * `original` does.
 */
void expectSameRanges(
    Journal& original,
    Journal& restored,
    Journal::SequenceNumber from) {
  auto next = original.peekLatest()->sequenceID + 1;
  for (auto position = from; position <= next; ++position) {
    SCOPED_TRACE(fmt::format("accumulateRange({})", position));
    auto expected = original.accumulateRange(position);
    auto actual = restored.accumulateRange(position);
    ASSERT_EQ(expected == nullptr, actual == nullptr);
    if (!expected) {
      continue;
    }
    EXPECT_EQ(expected->isTruncated, actual->isTruncated);
    EXPECT_EQ(expected->fromSequence, actual->fromSequence);
    EXPECT_EQ(expected->toSequence, actual->toSequence);
    EXPECT_EQ(expected->fromTime, actual->fromTime);
    EXPECT_EQ(expected->toTime, actual->toTime);
    EXPECT_EQ(expected->snapshotTransitions, actual->snapshotTransitions);
    EXPECT_EQ(expected->changedFilesInOverlay, actual->changedFilesInOverlay);
    EXPECT_EQ(expected->uncleanPaths, actual->uncleanPaths);
  }
}

/**
 * Describes every field of every delta from `from` on, newest first, so that
 * tests can compare deltas that accumulateRange() does not fully expose.
 */
std::vector<std::string> describeDeltas(
    Journal& journal,
    Journal::SequenceNumber from) {
  std::vector<std::string> result;
  journal.forEachDelta(
      from,
      std::nullopt,
      [&](const FileChangeJournalDelta& delta) {
        result.push_back(
            fmt::format(
                "file seq={} earliest={} time={} path1={}:{}:{}:{} "
                "path2={}:{}:{}:{} type={}",
                delta.sequenceID,
                delta.earliestSequenceID,
                delta.time.time_since_epoch().count(),
                delta.isPath1Valid,
                delta.path1.asString(),
                static_cast<bool>(delta.info1.existedBefore),
                static_cast<bool>(delta.info1.existedAfter),
                delta.isPath2Valid,
                delta.path2.asString(),
                static_cast<bool>(delta.info2.existedBefore),
                static_cast<bool>(delta.info2.existedAfter),
                static_cast<int>(delta.type)));
        return true;
      },
      [&](const RootUpdateJournalDelta& delta) {
        std::vector<std::string> uncleanPaths;
        uncleanPaths.reserve(delta.uncleanPaths.size());
        for (const auto& path : delta.uncleanPaths) {
          uncleanPaths.push_back(path.asString());
        }
        std::sort(uncleanPaths.begin(), uncleanPaths.end());
        result.push_back(
            fmt::format(
                "root seq={} earliest={} time={} from={} unclean={}",
                delta.sequenceID,
                delta.earliestSequenceID,
                delta.time.time_since_epoch().count(),
                delta.fromRoot.value(),
                fmt::join(uncleanPaths, ",")));
        return true;
      });
  return result;
}

} // namespace

TEST_F(JournalTest, takeover_round_trip) {
  auto root1 = RootId{"1111111111111111111111111111111111111111"};
  auto root2 = RootId{"2222222222222222222222222222222222222222"};
  journal.recordRootUpdate(root1);
  journal.recordCreated("created.txt"_relpath, dtype_t::Regular);
  journal.recordChanged("changed.txt"_relpath, dtype_t::Regular);
  journal.recordChanged("changed.txt"_relpath, dtype_t::Regular);
  journal.recordRenamed("old"_relpath, "new"_relpath, dtype_t::Dir);
  journal.recordReplaced("a"_relpath, "b"_relpath, dtype_t::Symlink);
  journal.recordUncleanPaths(root1, root2, {RelativePath{"unclean.txt"}});
  journal.recordRemoved("created.txt"_relpath, dtype_t::Regular);

  auto serialized = journal.serializeForTakeover(
      /*mountGeneration=*/42, std::numeric_limits<size_t>::max());
  ASSERT_TRUE(serialized);
  EXPECT_EQ(42u, *serialized->mountGeneration());

  Journal restored{edenStats.copy()};
  restored.initializeFromTakeover(std::move(*serialized));

  expectSameRanges(journal, restored, 1);
  EXPECT_EQ(describeDeltas(journal, 1), describeDeltas(restored, 1));
  EXPECT_EQ(journal.getStats()->entryCount, restored.getStats()->entryCount);

  auto latest = journal.peekLatest();
  auto restoredLatest = restored.peekLatest();
  ASSERT_TRUE(restoredLatest);
  EXPECT_EQ(latest->sequenceID, restoredLatest->sequenceID);
  EXPECT_EQ(root2, restoredLatest->toRoot);

  // The restored journal continues the sequence numbers.
  restored.recordChanged("after.txt"_relpath, dtype_t::Regular);
  EXPECT_EQ(latest->sequenceID + 1, restored.peekLatest()->sequenceID);
}

TEST_F(JournalTest, takeover_carries_the_newest_entries_up_to_the_limit) {
  for (int i = 0; i < 10; ++i) {
    journal.recordCreated(
        RelativePath{fmt::format("file{}", i)}, dtype_t::Regular);
  }
  auto entryBytes =
      FileChangeJournalDelta{
          "file0"_relpath, dtype_t::Regular, FileChangeJournalDelta::CREATED}
          .estimateMemoryUsage();

  auto serialized = journal.serializeForTakeover(1, 3 * entryBytes);
  ASSERT_TRUE(serialized);
  Journal restored{edenStats.copy()};
  restored.initializeFromTakeover(std::move(*serialized));

  EXPECT_EQ(3u, restored.getStats()->entryCount);
  EXPECT_TRUE(restored.accumulateRange(7)->isTruncated);
  expectSameRanges(journal, restored, 8);
  EXPECT_EQ(describeDeltas(journal, 8), describeDeltas(restored, 8));
}

TEST_F(JournalTest, takeover_carries_at_least_the_newest_entry) {
  journal.recordCreated("file1.txt"_relpath, dtype_t::Regular);
  journal.recordCreated("file2.txt"_relpath, dtype_t::Regular);

  auto serialized = journal.serializeForTakeover(1, 0);
  ASSERT_TRUE(serialized);
  Journal restored{edenStats.copy()};
  restored.initializeFromTakeover(std::move(*serialized));

  EXPECT_TRUE(restored.accumulateRange(1)->isTruncated);
  expectSameRanges(journal, restored, 2);
  EXPECT_EQ(describeDeltas(journal, 2), describeDeltas(restored, 2));
}

TEST_F(JournalTest, takeover_carries_nothing_from_an_empty_journal) {
  EXPECT_FALSE(journal.serializeForTakeover(1, 1000));
}

TEST_F(JournalTest, compaction) {
  journal.recordCreated("file1.txt"_relpath, dtype_t::Regular);
  auto stats = journal.getStats();
  ASSERT_TRUE(stats.has_value());
  ASSERT_EQ(1, stats->entryCount);
  auto latest = journal.observeLatest();
  ASSERT_TRUE(latest);
  ASSERT_EQ(1, latest->sequenceID);

  journal.recordChanged("file1.txt"_relpath, dtype_t::Regular);
  stats = journal.getStats();
  ASSERT_TRUE(stats.has_value());
  ASSERT_EQ(2, stats->entryCount);
  latest = journal.observeLatest();
  ASSERT_TRUE(latest);
  ASSERT_EQ(2, latest->sequenceID);
  auto summed = journal.accumulateRange(2);
  ASSERT_NE(nullptr, summed);
  EXPECT_EQ(2, summed->fromSequence);
  EXPECT_EQ(2, summed->toSequence);
  EXPECT_EQ(1, summed->changedFilesInOverlay.size());

  // Changing file1.txt again should just change the sequenceID of the last
  // delta to be 3
  journal.recordChanged("file1.txt"_relpath, dtype_t::Regular);
  stats = journal.getStats();
  ASSERT_TRUE(stats.has_value());
  ASSERT_EQ(2, stats->entryCount);
  latest = journal.observeLatest();
  ASSERT_TRUE(latest);
  ASSERT_EQ(3, latest->sequenceID);
  summed = journal.accumulateRange(2);
  ASSERT_NE(nullptr, summed);
  // We expect from to be 3 since there is no delta with sequence ID = 2
  EXPECT_EQ(3, summed->fromSequence);
  EXPECT_EQ(3, summed->toSequence);
  EXPECT_EQ(1, summed->changedFilesInOverlay.size());
}

TEST_F(JournalTest, compaction_does_not_truncate_the_absorbed_sequence) {
  journal.recordChanged("file1.txt"_relpath, dtype_t::Regular);
  journal.recordChanged("file1.txt"_relpath, dtype_t::Regular);
  ASSERT_EQ(1, journal.getStats()->entryCount);
  ASSERT_EQ(2, journal.observeLatest()->sequenceID);

  auto summed = journal.accumulateRange(1);
  ASSERT_TRUE(summed);
  EXPECT_FALSE(summed->isTruncated);
  EXPECT_EQ(2, summed->fromSequence);
  EXPECT_EQ(1, summed->changedFilesInOverlay.size());

  summed = journal.accumulateRange(2);
  ASSERT_TRUE(summed);
  EXPECT_FALSE(summed->isTruncated);
}

TEST_F(JournalTest, compaction_after_truncation_keeps_the_truncation_boundary) {
  journal.recordChanged("file1.txt"_relpath, dtype_t::Regular);
  journal.setMemoryLimit(journal.estimateMemoryUsage());

  journal.recordChanged("file2.txt"_relpath, dtype_t::Regular);
  ASSERT_EQ(2, journal.getStats()->entryCount);
  ASSERT_EQ(2, journal.observeLatest()->sequenceID);

  journal.recordChanged("file2.txt"_relpath, dtype_t::Regular);
  ASSERT_EQ(1, journal.getStats()->entryCount);
  ASSERT_EQ(3, journal.observeLatest()->sequenceID);

  auto summed = journal.accumulateRange(1);
  ASSERT_TRUE(summed);
  EXPECT_TRUE(summed->isTruncated);

  summed = journal.accumulateRange(2);
  ASSERT_TRUE(summed);
  EXPECT_FALSE(summed->isTruncated);
  EXPECT_EQ(1, summed->changedFilesInOverlay.size());

  summed = journal.accumulateRange(3);
  ASSERT_TRUE(summed);
  EXPECT_FALSE(summed->isTruncated);
}

TEST_F(JournalTest, update_transitions_are_all_recorded) {
  RootId root1{"0000000000000000000000000000000000000001"};
  RootId root2{"0000000000000000000000000000000000000002"};
  RootId root3{"0000000000000000000000000000000000000003"};
  journal.recordRootUpdate(root1, root2);
  journal.recordRootUpdate(root2, root3);

  auto summed = journal.accumulateRange();
  EXPECT_EQ(3, summed->snapshotTransitions.size());
  EXPECT_EQ(root1, summed->snapshotTransitions[0]);
  EXPECT_EQ(root2, summed->snapshotTransitions[1]);
  EXPECT_EQ(root3, summed->snapshotTransitions[2]);
}

TEST_F(JournalTest, update_transitions_are_coalesced) {
  RootId root1{"0000000000000000000000000000000000000001"};
  RootId root2{"0000000000000000000000000000000000000002"};
  RootId root3{"0000000000000000000000000000000000000003"};
  journal.recordRootUpdate(root1, root2);
  journal.recordRootUpdate(root2, root2);
  journal.recordRootUpdate(root2, root3);

  auto summed = journal.accumulateRange();
  EXPECT_EQ(3, summed->snapshotTransitions.size());
  EXPECT_EQ(root1, summed->snapshotTransitions[0]);
  EXPECT_EQ(root2, summed->snapshotTransitions[1]);
  EXPECT_EQ(root3, summed->snapshotTransitions[2]);
}

TEST_F(JournalTest, update_transitions_with_unclean_files_are_not_coalesced) {
  RootId root1{"0000000000000000000000000000000000000001"};
  RootId root2{"0000000000000000000000000000000000000002"};
  RootId root3{"0000000000000000000000000000000000000003"};
  journal.recordRootUpdate(root1, root2);
  journal.recordUncleanPaths(root2, root2, {RelativePath{"foo"}});
  journal.recordRootUpdate(root2, root3);

  auto summed = journal.accumulateRange();
  EXPECT_EQ(4, summed->snapshotTransitions.size());
  EXPECT_EQ(root1, summed->snapshotTransitions[0]);
  EXPECT_EQ(root2, summed->snapshotTransitions[1]);
  EXPECT_EQ(root2, summed->snapshotTransitions[2]);
  EXPECT_EQ(root3, summed->snapshotTransitions[3]);
}

TEST_F(JournalTest, subscribers_are_notified_of_changes) {
  unsigned calls = 0;
  auto sub = journal.registerSubscriber([&] { ++calls; });
  (void)sub;

  EXPECT_EQ(0u, calls);
  journal.recordChanged("foo"_relpath, dtype_t::Dir);
  EXPECT_EQ(1u, calls);
  EXPECT_EQ(1u, journal.observeLatest()->sequenceID);

  journal.recordChanged("foo"_relpath, dtype_t::Dir);
  EXPECT_EQ(2u, calls);
  EXPECT_EQ(2u, journal.observeLatest()->sequenceID);
}

TEST_F(
    JournalTest,
    subscribers_are_not_notified_of_changes_until_they_are_observed) {
  unsigned calls = 0;
  auto sub = journal.registerSubscriber([&] { ++calls; });
  (void)sub;

  EXPECT_EQ(0u, calls);
  journal.recordChanged("foo"_relpath, dtype_t::Regular);
  EXPECT_EQ(1u, calls);
  journal.recordChanged("foo"_relpath, dtype_t::Regular);
  EXPECT_EQ(1u, calls);
  EXPECT_EQ(2u, journal.observeLatest()->sequenceID);
  journal.recordChanged("foo"_relpath, dtype_t::Regular);
  EXPECT_EQ(2u, calls);
  EXPECT_EQ(3u, journal.observeLatest()->sequenceID);
}

TEST_F(JournalTest, all_subscribers_are_notified_after_any_observation) {
  unsigned calls1 = 0;
  unsigned calls2 = 0;
  auto sub1 = journal.registerSubscriber([&] { ++calls1; });
  (void)sub1;
  auto sub2 = journal.registerSubscriber([&] { ++calls2; });
  (void)sub2;

  EXPECT_EQ(0u, calls1);
  EXPECT_EQ(0u, calls2);

  journal.recordChanged("foo"_relpath, dtype_t::Regular);
  journal.recordChanged("foo"_relpath, dtype_t::Regular);

  EXPECT_EQ(1u, calls1);
  EXPECT_EQ(1u, calls2);

  EXPECT_EQ(2u, journal.observeLatest()->sequenceID);
  journal.recordChanged("foo"_relpath, dtype_t::Regular);

  EXPECT_EQ(2u, calls1);
  EXPECT_EQ(2u, calls2);

  journal.recordChanged("foo"_relpath, dtype_t::Regular);

  EXPECT_EQ(2u, calls1);
  EXPECT_EQ(2u, calls2);
}

TEST_F(JournalDeltaTest, for_each_delta) {
  addFileChange("foo1"_relpath, dtype_t::Regular);
  addFileChange("foo2"_relpath, dtype_t::Symlink);
  EXPECT_EQ(2u, journal.observeLatest()->sequenceID);
  addFileChange("foo3"_relpath, dtype_t::Regular);
  addFileChange("foo4"_relpath, dtype_t::Symlink);
  EXPECT_EQ(4u, journal.observeLatest()->sequenceID);
  addRootUpdate(root1, root2);
  EXPECT_EQ(5u, journal.observeLatest()->sequenceID);
  addFileChange("foo6"_relpath, dtype_t::Regular);
  addFileChange("foo7"_relpath, dtype_t::Regular);
  addRootUpdate(root2, root1);

  bool truncated = journal.forEachDelta(
      1u,
      std::nullopt,
      [&](const FileChangeJournalDelta& current) -> bool {
        fileChangeSequences.push_back(current.sequenceID);
        fileChangeNames.push_back(current.path1);
        fileChangeDtypes.push_back(current.type);
        return true;
      },
      [&](const RootUpdateJournalDelta& current) -> bool {
        rootUpdateSequences.push_back(current.sequenceID);
        rootUpdateRoots.push_back(current.fromRoot);
        return true;
      });
  EXPECT_FALSE(truncated);
  reverseResults();
  checkExpect();
}

/*
 * This test covers the case where 'from' is a value below the sequence number
 * of the first delta in fileChanges and there are rootUpdates present between
 * the two. It checks that fileChanges starts from the first entry in the
 * fileChanges vector.
 */
TEST_F(JournalDeltaTest, for_each_delta_file_change_ends_above_from) {
  setupFlushedJournal();
  EXPECT_EQ(5u, journal.observeLatest()->sequenceID);

  // Create rootUpdates after from and before file changes
  addRootUpdate(root1, root2);
  addRootUpdate(root2, root1);
  EXPECT_EQ(7u, journal.observeLatest()->sequenceID);

  // Create file changes
  addFileChange("foo3"_relpath, dtype_t::Regular);
  addFileChange("foo4"_relpath, dtype_t::Symlink);
  EXPECT_EQ(9u, journal.observeLatest()->sequenceID);

  bool truncated = journal.forEachDelta(
      5u,
      std::nullopt,
      [&](const FileChangeJournalDelta& current) -> bool {
        fileChangeSequences.push_back(current.sequenceID);
        fileChangeNames.push_back(current.path1);
        fileChangeDtypes.push_back(current.type);
        return true;
      },
      [&](const RootUpdateJournalDelta& current) -> bool {
        rootUpdateSequences.push_back(current.sequenceID);
        rootUpdateRoots.push_back(current.fromRoot);
        return true;
      });
  EXPECT_FALSE(truncated);
  reverseResults();
  checkExpect();
}

/*
 * This test covers the case where 'from' is a value below the sequence number
 * of the first delta in rootUpdates and there are fileChanges present between
 * the two. It checks that rootUpdates starts from the first entry in the
 * rootUpdates vector.
 */
TEST_F(JournalDeltaTest, for_each_delta_hash_update_ends_above_from) {
  setupFlushedJournal();
  EXPECT_EQ(5u, journal.observeLatest()->sequenceID);

  // Create file changes after from and before rootUpdates
  addFileChange("foo3"_relpath, dtype_t::Regular);
  addFileChange("foo4"_relpath, dtype_t::Symlink);
  EXPECT_EQ(7u, journal.observeLatest()->sequenceID);

  // Create rootUpdates
  addRootUpdate(root1, root2);
  addRootUpdate(root2, root1);
  EXPECT_EQ(9u, journal.observeLatest()->sequenceID);

  bool truncated = journal.forEachDelta(
      5u,
      std::nullopt,
      [&](const FileChangeJournalDelta& current) -> bool {
        fileChangeSequences.push_back(current.sequenceID);
        fileChangeNames.push_back(current.path1);
        fileChangeDtypes.push_back(current.type);
        return true;
      },
      [&](const RootUpdateJournalDelta& current) -> bool {
        rootUpdateSequences.push_back(current.sequenceID);
        rootUpdateRoots.push_back(current.fromRoot);
        return true;
      });
  EXPECT_FALSE(truncated);
  reverseResults();
  checkExpect();
}

/*
 * Tests that when 'from' is in the middle of the result set,
 * returns all results starting from that value
 */
TEST_F(JournalDeltaTest, for_each_delta_partial_results) {
  setupGeneric(6u);
  bool truncated = journal.forEachDelta(
      6u,
      std::nullopt,
      [&](const FileChangeJournalDelta& current) -> bool {
        fileChangeSequences.push_back(current.sequenceID);
        fileChangeNames.push_back(current.path1);
        fileChangeDtypes.push_back(current.type);
        return true;
      },
      [&](const RootUpdateJournalDelta& current) -> bool {
        rootUpdateSequences.push_back(current.sequenceID);
        rootUpdateRoots.push_back(current.fromRoot);
        return true;
      });
  EXPECT_FALSE(truncated);
  reverseResults();
  checkExpect();
}

/*
 * Tests that when 'from' is higher than the current sequence root,
 * returns no values.
 */
TEST_F(JournalDeltaTest, for_each_delta_no_results) {
  setupGeneric(10u);
  bool truncated = journal.forEachDelta(
      10u,
      std::nullopt,
      [&](const FileChangeJournalDelta& current) -> bool {
        fileChangeSequences.push_back(current.sequenceID);
        fileChangeNames.push_back(current.path1);
        fileChangeDtypes.push_back(current.type);
        return true;
      },
      [&](const RootUpdateJournalDelta& current) -> bool {
        rootUpdateSequences.push_back(current.sequenceID);
        rootUpdateRoots.push_back(current.fromRoot);
        return true;
      });
  EXPECT_FALSE(truncated);
  reverseResults();
  checkExpect();
}

/*
 * Tests that when the fileChange callback returns false, iteration stops
 * Since iteration is backwards, the contents will be from most recent to
 * stopping point
 */
TEST_F(JournalDeltaTest, for_each_delta_early_exit_file) {
  // We're using a custom expect values so the input to setupGeneric doesn't
  // matter
  setupGeneric(0u);

  // We expect to stop when sequenceID == 7, so only the first entry is
  // populated
  expectedFileChangeSequences = {};
  expectedFileChangeNames = {};
  expectedFileChangeDtypes = {};
  expectedRootUpdateSequences = {8, 9};
  expectedRootUpdateRoots = {root1, root2};

  bool truncated = journal.forEachDelta(
      6u,
      std::nullopt,
      [&](const FileChangeJournalDelta& current) -> bool {
        if (current.sequenceID == 7) {
          return false;
        }
        fileChangeSequences.push_back(current.sequenceID);
        fileChangeNames.push_back(current.path1);
        fileChangeDtypes.push_back(current.type);
        return true;
      },
      [&](const RootUpdateJournalDelta& current) -> bool {
        rootUpdateSequences.push_back(current.sequenceID);
        rootUpdateRoots.push_back(current.fromRoot);
        return true;
      });
  EXPECT_FALSE(truncated);
  reverseResults();
  checkExpect();
}

/*
 * Tests that when the rootUpdate callback returns false, iteration stops
 * Since iteration is backwards, the contents will be from most recent to
 * stopping point
 */
TEST_F(JournalDeltaTest, for_each_delta_early_exit_hash) {
  // We're using a custom expect values so the input to setupGeneric doesn't
  // matter
  setupGeneric(0u);

  // We expect to stop when sequenceID == 9, so only the first entry is
  // populated in rootUpdate
  expectedFileChangeSequences = {};
  expectedFileChangeNames = {};
  expectedFileChangeDtypes = {};
  expectedRootUpdateSequences = {9};
  expectedRootUpdateRoots = {root2};

  bool truncated = journal.forEachDelta(
      6u,
      std::nullopt,
      [&](const FileChangeJournalDelta& current) -> bool {
        fileChangeSequences.push_back(current.sequenceID);
        fileChangeNames.push_back(current.path1);
        fileChangeDtypes.push_back(current.type);
        return true;
      },
      [&](const RootUpdateJournalDelta& current) -> bool {
        if (current.sequenceID == 8) {
          return false;
        }
        rootUpdateSequences.push_back(current.sequenceID);
        rootUpdateRoots.push_back(current.fromRoot);
        return true;
      });
  EXPECT_FALSE(truncated);
  reverseResults();
  checkExpect();
}

/*
 * Tests all file change dtypes and empty root update
 */
TEST_F(JournalDeltaTest, for_each_delta_file_changes_only) {
  addFileChange("foo1"_relpath, dtype_t::Unknown);
  addFileChange("foo2"_relpath, dtype_t::Fifo);
  addFileChange("foo3"_relpath, dtype_t::Char);
  addFileChange("foo4"_relpath, dtype_t::Dir);
  addFileChange("foo5"_relpath, dtype_t::Regular);
  addFileChange("foo6"_relpath, dtype_t::Symlink);
  addFileChange("foo7"_relpath, dtype_t::Socket);
  EXPECT_EQ(7u, journal.observeLatest()->sequenceID);

  bool truncated = journal.forEachDelta(
      1u,
      std::nullopt,
      [&](const FileChangeJournalDelta& current) -> bool {
        fileChangeSequences.push_back(current.sequenceID);
        fileChangeNames.push_back(current.path1);
        fileChangeDtypes.push_back(current.type);
        return true;
      },
      [&](const RootUpdateJournalDelta& /*current*/) -> bool { return true; });
  EXPECT_FALSE(truncated);
  reverseResults();
  checkExpect();
}

/*
 * Tests rootUpdate with empty fileChange
 */
TEST_F(JournalDeltaTest, for_each_delta_hash_update_only) {
  addRootUpdate(root1);
  addRootUpdate(root1, root2);
  addRootUpdate(root2, root1);
  addRootUpdate(root1, root3);
  EXPECT_EQ(4u, journal.observeLatest()->sequenceID);

  bool truncated = journal.forEachDelta(
      1u,
      std::nullopt,
      [&](const FileChangeJournalDelta& /*current*/) -> bool { return true; },
      [&](const RootUpdateJournalDelta& current) -> bool {
        rootUpdateSequences.push_back(current.sequenceID);
        rootUpdateRoots.push_back(current.fromRoot);
        return true;
      });
  reverseResults();
  EXPECT_FALSE(truncated);
  checkExpect();
}
