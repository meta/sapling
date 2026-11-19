/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::env;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use fbinit::FacebookInit;
use scribe_ext::Scribe;
use scuba_ext::MononokeScubaSampleBuilder;

use crate::perf_counters::PerfCounters;
use crate::perf_counters_stack::PerfCountersStack;

/// Used to correlation a high level action on a CoreContext
/// e.g. walk of a repo,  with low level actions using that context
/// e.g. which blobs are read
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct SamplingKey {
    inner_key: u32,
}

static NEXT_SAMPLING_KEY: AtomicU32 = AtomicU32::new(0);

impl SamplingKey {
    pub fn new() -> Self {
        let v = NEXT_SAMPLING_KEY.fetch_add(1, Ordering::Relaxed);
        Self { inner_key: v }
    }

    pub fn inner(&self) -> u32 {
        self.inner_key
    }
}

#[derive(Debug, Clone)]
pub struct LoggingContainer {
    scuba: Arc<MononokeScubaSampleBuilder>,
    perf_counters: PerfCountersStack,
    sampling_key: Option<SamplingKey>,
    scribe: Scribe,
    override_sampling: Arc<AtomicBool>,
    nocache_thriftcache: Arc<AtomicBool>,
    partial_response: Arc<AtomicBool>,
    potential_partial_response: Arc<AtomicBool>,
    response_depends_on_caller: Arc<AtomicBool>,
}

impl LoggingContainer {
    pub fn new(fb: FacebookInit, scuba: MononokeScubaSampleBuilder) -> Self {
        let scribe_logging_directory = env::var("MONONOKE_TEST_SCRIBE_LOGGING_DIRECTORY").ok();
        let scribe = if let Some(dir_path) = scribe_logging_directory {
            Scribe::new_to_file(dir_path.into())
        } else {
            Scribe::new(fb)
        };
        Self {
            scuba: Arc::new(scuba),
            perf_counters: Default::default(),
            sampling_key: None,
            scribe,
            override_sampling: Arc::new(AtomicBool::new(false)),
            nocache_thriftcache: Arc::new(AtomicBool::new(false)),
            partial_response: Arc::new(AtomicBool::new(false)),
            potential_partial_response: Arc::new(AtomicBool::new(false)),
            response_depends_on_caller: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn fork_perf_counters(&mut self) -> Arc<PerfCounters> {
        let (perf_counters, ret) = self.perf_counters.fork();
        self.perf_counters = perf_counters;
        ret
    }

    pub fn clone_and_sample(&self, sampling_key: SamplingKey) -> Self {
        Self {
            scuba: self.scuba.clone(),
            perf_counters: self.perf_counters.clone(),
            sampling_key: Some(sampling_key),
            scribe: self.scribe.clone(),
            override_sampling: self.override_sampling.clone(),
            nocache_thriftcache: self.nocache_thriftcache.clone(),
            partial_response: self.partial_response.clone(),
            potential_partial_response: self.potential_partial_response.clone(),
            response_depends_on_caller: self.response_depends_on_caller.clone(),
        }
    }

    pub fn with_scribe(&mut self, scribe: Scribe) -> &mut Self {
        self.scribe = scribe;
        self
    }

    pub fn scuba(&self) -> &MononokeScubaSampleBuilder {
        &self.scuba
    }

    pub fn perf_counters(&self) -> &PerfCountersStack {
        &self.perf_counters
    }

    pub fn sampling_key(&self) -> Option<&SamplingKey> {
        self.sampling_key.as_ref()
    }

    pub fn scribe(&self) -> &Scribe {
        &self.scribe
    }

    pub fn with_mutated_scuba(
        &self,
        mutator: impl FnOnce(MononokeScubaSampleBuilder) -> MononokeScubaSampleBuilder,
    ) -> Self {
        Self {
            scuba: Arc::new(mutator(self.scuba().clone())),
            perf_counters: self.perf_counters.clone(),
            sampling_key: self.sampling_key.clone(),
            scribe: self.scribe.clone(),
            override_sampling: self.override_sampling.clone(),
            nocache_thriftcache: self.nocache_thriftcache.clone(),
            partial_response: self.partial_response.clone(),
            potential_partial_response: self.potential_partial_response.clone(),
            response_depends_on_caller: self.response_depends_on_caller.clone(),
        }
    }

    pub fn override_sampling(&self) -> bool {
        self.override_sampling.load(Ordering::Relaxed)
    }

    pub fn set_override_sampling(&self) {
        self.override_sampling.store(true, Ordering::Relaxed);
    }

    pub fn nocache_thriftcache(&self) -> bool {
        self.nocache_thriftcache.load(Ordering::Relaxed)
    }

    pub fn set_nocache_thriftcache(&self) {
        self.nocache_thriftcache.store(true, Ordering::Relaxed);
    }

    pub fn partial_response(&self) -> bool {
        self.partial_response.load(Ordering::Relaxed)
    }

    pub fn set_partial_response(&self) {
        self.partial_response.store(true, Ordering::Relaxed);
    }

    pub fn potential_partial_response(&self) -> bool {
        self.potential_partial_response.load(Ordering::Relaxed)
    }

    pub fn set_potential_partial_response(&self) {
        self.potential_partial_response
            .store(true, Ordering::Relaxed);
    }

    pub fn response_depends_on_caller(&self) -> bool {
        self.response_depends_on_caller.load(Ordering::Relaxed)
    }

    pub fn set_response_depends_on_caller(&self) {
        self.response_depends_on_caller
            .store(true, Ordering::Relaxed);
    }
}
