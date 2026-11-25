/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use scuba_ext::MononokeScubaSampleBuilder;

macro_rules! define_perf_counters {
    (enum $enum_name:ident {
        $($variant:ident),*,
    }) => {
        #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
        pub enum $enum_name {
            $($variant),*
        }

        impl $enum_name {
            pub fn name(&self) -> &'static str {
                match self {
                    $($enum_name::$variant => stringify!($variant)),*
                }
            }
        }

        pub const PERF_COUNTERS: &[PerfCounterType] = &[
            $($enum_name::$variant),*
        ];

        #[allow(non_snake_case)]
        #[derive(Debug, Default)]
        pub struct PerfCounters {
            $($variant: AtomicI64),*
        }

        impl PerfCounters {
            fn get_counter_atomic(&self, counter: $enum_name) -> &AtomicI64 {
                match counter {
                    $($enum_name::$variant => &self.$variant),*
                }
            }
        }
    };
}

define_perf_counters! {
    enum PerfCounterType {
        CpuTimeNs,
        BlobGets,
        BlobGetsNotFound,
        BlobGetsAccessWait,
        BlobGetsShardAccessWait,
        BlobGetsMaxLatency,
        BlobGetsNotFoundMaxLatency,
        BlobGetsDeduplicated,
        BlobGetsDeduplicatedLarge,
        BlobGetsTotalSize,
        BlobGetsMaxSize,
        BlobPresenceChecks,
        BlobPresenceChecksMaxLatency,
        BlobPuts,
        BlobPutsAccessWait,
        BlobPutsShardAccessWait,
        BlobPutsMaxLatency,
        BlobPutsDeduplicated,
        BlobPutsTotalSize,
        BlobPutsMaxSize,
        BlobUnlinks,
        BlobUnlinksMaxLatency,
        BytesSent,
        CachelibHits,
        CachelibMisses,
        CommitGraphBatchPrefetchScanned,
        EdenapiFiles,
        EdenapiFilesAuxData,
        EdenapiTrees,
        EdenapiTreesAuxData,
        EdenapiAugmentedTrees,
        EdenapiAugmentedTreesFallback,
        EdenapiOriginalRoutedToAugmented,
        EdenapiAugmentedManifestMissUnderRoute,
        EdenapiUploadTreesAugmentedBuildUs,
        EdenapiUploadTreesAugmentedTrees,
        UploadedRootAugmentedStored,
        UploadedRootAugmentedMissing,
        UploadedRootAugmentedUnmappable,
        UploadedRootAugmentedFailed,
        UploadedRootAugmentedMapped,
        UploadParentAugmentedStarted,
        UploadParentAugmentedFailed,
        TreeLookupAugmentedMissing,
        GetbundleNumCommits,
        GetbundleNumDrafts,
        GetpackNumPossibleLFSFiles,
        GetpackPossibleLFSFilesSumSize,
        FilenodesTooBigHistory,
        HgMutationStoreNumAdded,
        HgMutationStoreNumFetched,
        MemcacheHits,
        MemcacheMisses,
        NullLinknode,
        NumKnown,
        NumKnownRequested,
        NumUnknown,
        PhasesDbQueries,
        PhasesLookupCommits,
        PhasesLookupMaxUs,
        PhasesLookupUs,
        PhasesLookups,
        PhasesMarkPublicUs,
        PhasesSectionUs,
        RepoBuildWaitUs,
        SqlReadsMaster,
        SqlReadsReplica,
        SqlWrites,
        SumManifoldPollTime,
        UndesiredTreeFetch,
        ManifoldBlobSumDelay,
        ManifoldBlobRetries,
        ManifoldBlobConflicts,
        S3BlobRetries,
        S3BlobSumDelay,
        S3AccessWait,
    }
}

fn duration_us(duration: Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

enum PerfCounterTypeUpdateFunc {
    Add,
    Max,
}

impl PerfCounterType {
    /// Whether to log the zero value of the counter
    fn always_log(&self) -> bool {
        use PerfCounterType::*;

        match self {
            BlobGets
            | BlobGetsNotFound
            | BlobGetsMaxLatency
            | BlobGetsNotFoundMaxLatency
            | BlobGetsTotalSize
            | BlobGetsMaxSize
            | BlobPresenceChecks
            | BlobPresenceChecksMaxLatency
            | BlobPuts
            | BlobPutsMaxLatency
            | BlobPutsTotalSize
            | BlobPutsMaxSize
            | BlobUnlinks
            | BlobUnlinksMaxLatency
            | CachelibHits
            | CachelibMisses
            | GetpackNumPossibleLFSFiles
            | GetpackPossibleLFSFilesSumSize
            | MemcacheHits
            | MemcacheMisses
            | SqlReadsMaster
            | SqlReadsReplica
            | SqlWrites => true,
            _ => false,
        }
    }

    fn expected_update_func(&self) -> PerfCounterTypeUpdateFunc {
        use PerfCounterType::*;

        match self {
            CpuTimeNs
            | BlobGets
            | BlobGetsNotFound
            | BlobGetsAccessWait
            | BlobGetsShardAccessWait
            | BlobGetsDeduplicated
            | BlobGetsDeduplicatedLarge
            | BlobGetsTotalSize
            | BlobPresenceChecks
            | BlobPuts
            | BlobPutsAccessWait
            | BlobPutsShardAccessWait
            | BlobPutsDeduplicated
            | BlobPutsTotalSize
            | BlobUnlinks
            | BytesSent
            | CachelibHits
            | CachelibMisses
            | CommitGraphBatchPrefetchScanned
            | EdenapiFiles
            | EdenapiFilesAuxData
            | EdenapiTrees
            | EdenapiTreesAuxData
            | EdenapiAugmentedTrees
            | EdenapiAugmentedTreesFallback
            | EdenapiOriginalRoutedToAugmented
            | EdenapiAugmentedManifestMissUnderRoute
            | EdenapiUploadTreesAugmentedBuildUs
            | EdenapiUploadTreesAugmentedTrees
            | UploadedRootAugmentedStored
            | UploadedRootAugmentedMissing
            | UploadedRootAugmentedUnmappable
            | UploadedRootAugmentedFailed
            | UploadedRootAugmentedMapped
            | UploadParentAugmentedStarted
            | UploadParentAugmentedFailed
            | TreeLookupAugmentedMissing
            | GetbundleNumCommits
            | GetbundleNumDrafts
            | GetpackNumPossibleLFSFiles
            | GetpackPossibleLFSFilesSumSize
            | FilenodesTooBigHistory
            | HgMutationStoreNumAdded
            | HgMutationStoreNumFetched
            | MemcacheHits
            | MemcacheMisses
            | NullLinknode
            | NumKnown
            | NumKnownRequested
            | NumUnknown
            | PhasesDbQueries
            | PhasesLookupCommits
            | PhasesLookupUs
            | PhasesLookups
            | PhasesMarkPublicUs
            | PhasesSectionUs
            | RepoBuildWaitUs
            | SqlReadsMaster
            | SqlReadsReplica
            | SqlWrites
            | SumManifoldPollTime
            | UndesiredTreeFetch
            | ManifoldBlobSumDelay
            | ManifoldBlobRetries
            | ManifoldBlobConflicts
            | S3BlobRetries
            | S3BlobSumDelay
            | S3AccessWait => PerfCounterTypeUpdateFunc::Add,
            BlobGetsMaxLatency
            | BlobGetsNotFoundMaxLatency
            | BlobPresenceChecksMaxLatency
            | BlobPutsMaxLatency
            | BlobUnlinksMaxLatency
            | BlobGetsMaxSize
            | BlobPutsMaxSize
            | PhasesLookupMaxUs => PerfCounterTypeUpdateFunc::Max,
        }
    }
}

impl PerfCounters {
    pub fn set_counter(&self, counter: PerfCounterType, val: i64) {
        self.get_counter_atomic(counter)
            .store(val, Ordering::Relaxed);
    }

    pub fn increment_counter(&self, counter: PerfCounterType) {
        self.get_counter_atomic(counter)
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn decrement_counter(&self, counter: PerfCounterType) {
        self.get_counter_atomic(counter)
            .fetch_sub(1, Ordering::Relaxed);
    }

    pub fn add_to_counter(&self, counter: PerfCounterType, val: i64) {
        self.get_counter_atomic(counter)
            .fetch_add(val, Ordering::Relaxed);
    }

    pub fn set_max_counter(&self, counter: PerfCounterType, val: i64) {
        self.get_counter_atomic(counter)
            .fetch_max(val, Ordering::Relaxed);
    }

    /// Add a duration to a counter that measures microseconds.
    pub fn add_duration_us(&self, counter: PerfCounterType, duration: Duration) {
        self.add_to_counter(counter, duration_us(duration));
    }

    /// Raise a counter that measures microseconds to at least this duration.
    pub fn set_max_duration_us(&self, counter: PerfCounterType, duration: Duration) {
        self.set_max_counter(counter, duration_us(duration));
    }

    pub fn get_counter(&self, counter: PerfCounterType) -> i64 {
        self.get_counter_atomic(counter).load(Ordering::Relaxed)
    }

    pub fn update_with_counters(&self, counters: &PerfCounters) {
        for key in PERF_COUNTERS.iter() {
            match key.expected_update_func() {
                PerfCounterTypeUpdateFunc::Add => {
                    self.add_to_counter(*key, counters.get_counter(*key))
                }
                PerfCounterTypeUpdateFunc::Max => {
                    self.set_max_counter(*key, counters.get_counter(*key))
                }
            };
        }
    }

    pub fn insert_nonzero_perf_counters(&self, builder: &mut MononokeScubaSampleBuilder) {
        for key in PERF_COUNTERS.iter() {
            let value = self.get_counter(*key);
            if value != 0 {
                builder.add(key.name(), value);
            }
        }
    }

    pub fn insert_perf_counters(&self, builder: &mut MononokeScubaSampleBuilder) {
        // NOTE: we log 0 mainly so that we can distinguish
        // nulls i.e. "not logged" and 0 as in "zero calls to blobstore".
        // Logging 0 allows counting avg, p50 etc statistic.
        for key in PERF_COUNTERS.iter() {
            let value = self.get_counter(*key);
            if value != 0 || key.always_log() {
                builder.add(key.name(), value);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn test_perf_counter() {
        // NOTE: This test doesn't try to do anything fancy or test concurrency. It does however
        // check that we pass valid values for Ordering (invalid values panic on atomics).
        let ctrs = PerfCounters::default();
        let k = PerfCounterType::BlobGets;

        ctrs.set_counter(k, 1);
        assert_eq!(ctrs.get_counter(k), 1);

        ctrs.increment_counter(k);
        assert_eq!(ctrs.get_counter(k), 2);

        ctrs.decrement_counter(k);
        assert_eq!(ctrs.get_counter(k), 1);

        ctrs.add_to_counter(k, 1);
        assert_eq!(ctrs.get_counter(k), 2);

        ctrs.set_max_counter(k, 3);
        assert_eq!(ctrs.get_counter(k), 3);

        ctrs.set_max_counter(k, 2);
        assert_eq!(ctrs.get_counter(k), 3);
    }

    #[mononoke::test]
    fn test_duration_counters() {
        let ctrs = PerfCounters::default();
        let sum = PerfCounterType::PhasesLookupUs;
        let max = PerfCounterType::PhasesLookupMaxUs;

        ctrs.add_duration_us(sum, Duration::from_millis(3));
        ctrs.add_duration_us(sum, Duration::from_micros(250));
        assert_eq!(
            ctrs.get_counter(sum),
            3_250,
            "durations add up in microseconds"
        );

        ctrs.set_max_duration_us(max, Duration::from_millis(2));
        ctrs.set_max_duration_us(max, Duration::from_millis(1));
        assert_eq!(
            ctrs.get_counter(max),
            2_000,
            "the max keeps the longest duration"
        );
    }
}
