/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use clientinfo::ClientInfo;
use clientinfo::ClientRequestInfo;
use fbinit::FacebookInit;
use metadata::Metadata;
use scribe_ext::Scribe;
use scuba_ext::MononokeScubaSampleBuilder;
use sql_query_telemetry::SqlQueryTelemetry;

use crate::logging::LoggingContainer;
use crate::logging::SamplingKey;
use crate::perf_counters::PerfCounters;
use crate::perf_counters_stack::PerfCountersStack;
use crate::session::SessionClass;
use crate::session::SessionContainer;

#[derive(Clone)]
pub struct CoreContext {
    pub fb: FacebookInit,
    session: SessionContainer,
    logging: LoggingContainer,
    access_label: Option<&'static str>,
}

impl CoreContext {
    pub fn from_parts(
        fb: FacebookInit,
        logging: LoggingContainer,
        session: SessionContainer,
    ) -> Self {
        Self {
            fb,
            logging,
            session,
            access_label: None,
        }
    }

    pub fn new(fb: FacebookInit) -> Self {
        let session = SessionContainer::new_with_defaults(fb);
        session.new_context(MononokeScubaSampleBuilder::with_discard())
    }

    pub fn new_with_client_info(fb: FacebookInit, client_info: ClientInfo) -> Self {
        let mut metadata = Metadata::default();
        metadata.add_client_info(client_info);
        let session = SessionContainer::builder(fb)
            .metadata(Arc::new(metadata))
            .build();
        session.new_context(MononokeScubaSampleBuilder::with_discard())
    }

    pub fn new_with_scuba(fb: FacebookInit, scuba: MononokeScubaSampleBuilder) -> Self {
        let session = SessionContainer::new_with_defaults(fb);
        session.new_context(scuba)
    }

    // Context for bulk processing like scrubbing or bulk backfilling
    pub fn new_for_bulk_processing(fb: FacebookInit) -> Self {
        let session = SessionContainer::builder(fb)
            .session_class(SessionClass::Background)
            .build();
        session.new_context(MononokeScubaSampleBuilder::with_discard())
    }

    pub fn test_mock(fb: FacebookInit) -> Self {
        let session = SessionContainer::new_with_defaults(fb);

        Self::test_mock_session(session)
    }

    pub fn test_mock_session(session: SessionContainer) -> Self {
        session.new_context(MononokeScubaSampleBuilder::with_discard())
    }

    /// Create a new CoreContext, with a reset LoggingContainer. This is useful to reset perf
    /// counters. The existing CoreContext is unaffected.
    /// The access label is kept.
    pub fn clone_and_reset(&self) -> Self {
        Self {
            access_label: self.access_label,
            ..self.session.new_context(self.scuba().clone())
        }
    }

    pub fn clone_and_sample(&self, sampling_key: SamplingKey) -> Self {
        Self {
            fb: self.fb,
            session: self.session.clone(),
            logging: self.logging.clone_and_sample(sampling_key),
            access_label: self.access_label,
        }
    }

    pub fn with_mutated_scuba(
        &self,
        mutator: impl FnOnce(MononokeScubaSampleBuilder) -> MononokeScubaSampleBuilder,
    ) -> Self {
        Self {
            fb: self.fb,
            session: self.session.clone(),
            logging: self.logging.with_mutated_scuba(mutator),
            access_label: self.access_label,
        }
    }

    /// Return a copy of this context labelled with `label`, so storage layers
    /// can tell which component is reading through it.
    ///
    /// The label belongs to the returned copy and to clones made from it. The
    /// context it was made from, and its other clones, are unaffected, unlike
    /// the flags such as `set_override_sampling` that are shared by every
    /// clone of a request's context. Labelling again replaces the label.
    pub fn with_access_label(&self, label: &'static str) -> Self {
        Self {
            access_label: Some(label),
            ..self.clone()
        }
    }

    /// The label set by `with_access_label`, if any.
    pub fn access_label(&self) -> Option<&'static str> {
        self.access_label
    }

    /// Return a clone of this context with the session's metadata overridden,
    /// preserving the logging container (scuba, perf counters, scribe, sampling
    /// key, etc.). Unlike `SessionContainer::new_context`, which starts a fresh
    /// `LoggingContainer`, this keeps the caller's logging state intact, so work
    /// run under the returned context (e.g. hooks evaluated with `run_as`)
    /// observes only a different identity, not a reset logging environment.
    pub fn with_overridden_metadata(&self, metadata: Arc<Metadata>) -> Self {
        Self {
            fb: self.fb,
            session: self.session.override_metadata(metadata),
            logging: self.logging.clone(),
            access_label: self.access_label,
        }
    }

    pub fn sampling_key(&self) -> Option<&SamplingKey> {
        self.logging.sampling_key()
    }

    pub fn scuba(&self) -> &MononokeScubaSampleBuilder {
        self.logging.scuba()
    }

    pub fn perf_counters(&self) -> &PerfCountersStack {
        self.logging.perf_counters()
    }

    /// Charge CPU executed in the current Folly request context, including
    /// inherited task contexts, to this request's perf counters.
    pub fn track_cpu_time(&self) {
        #[cfg(fbcode_build)]
        crate::cpu_time::track_cpu_time(self.perf_counters().clone());
    }

    pub fn metadata(&self) -> &Metadata {
        self.session.metadata()
    }

    pub fn client_request_info(&self) -> Option<&ClientRequestInfo> {
        self.metadata().client_request_info()
    }

    pub fn session(&self) -> &SessionContainer {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut SessionContainer {
        &mut self.session
    }

    pub fn scribe(&self) -> &Scribe {
        self.logging.scribe()
    }

    pub fn fork_perf_counters(&mut self) -> Arc<PerfCounters> {
        self.logging.fork_perf_counters()
    }

    pub fn override_sampling(&self) -> bool {
        self.logging.override_sampling()
    }

    pub fn set_override_sampling(&self) {
        self.logging.set_override_sampling();
    }

    pub fn nocache_thriftcache(&self) -> bool {
        self.logging.nocache_thriftcache()
    }

    pub fn set_nocache_thriftcache(&self) {
        self.logging.set_nocache_thriftcache();
    }

    pub fn partial_response(&self) -> bool {
        self.logging.partial_response()
    }

    pub fn set_partial_response(&self) {
        self.logging.set_partial_response();
    }

    pub fn potential_partial_response(&self) -> bool {
        self.logging.potential_partial_response()
    }

    pub fn set_potential_partial_response(&self) {
        self.logging.set_potential_partial_response();
    }

    /// Whether the response to this request depends on the caller's
    /// identities beyond repo read access, so it must not be served to a
    /// caller it was not computed for.
    pub fn response_depends_on_caller(&self) -> bool {
        self.logging.response_depends_on_caller()
    }

    pub fn set_response_depends_on_caller(&self) {
        self.logging.set_response_depends_on_caller();
    }

    pub fn sql_query_telemetry(&self) -> SqlQueryTelemetry {
        let fb = self.fb.clone();
        let metadata = self.metadata();
        SqlQueryTelemetry::new(
            fb,
            metadata.clone(),
            self.scuba().observability_context().cloned(),
        )
    }

    pub fn client_correlator(&self) -> Option<&str> {
        self.metadata()
            .client_info()
            .and_then(|ci| ci.request_info.as_ref().map(|cri| cri.correlator.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::fbinit_test]
    fn test_label_does_not_leak_to_original_or_sibling_clones(fb: FacebookInit) {
        // Given: a context and a clone of it made before labelling.
        let ctx = CoreContext::test_mock(fb);
        let sibling = ctx.clone();

        // When: a labelled copy is made from the context.
        let labelled = ctx.with_access_label("reader");

        // Then: the copy is labelled, but neither the context nor its earlier
        // clone is, nor a clone made from the context afterwards.
        assert_eq!(labelled.access_label(), Some("reader"));
        assert_eq!(
            ctx.access_label(),
            None,
            "the original must stay unlabelled"
        );
        assert_eq!(
            sibling.access_label(),
            None,
            "earlier clones must stay unlabelled"
        );
        assert_eq!(
            ctx.clone().access_label(),
            None,
            "later clones of the original must stay unlabelled",
        );
    }

    #[mononoke::fbinit_test]
    fn test_label_survives_derived_copies(fb: FacebookInit) {
        // Given: a labelled context.
        let labelled = CoreContext::test_mock(fb).with_access_label("reader");

        // When/Then: every way of copying it keeps the label.
        assert_eq!(labelled.clone().access_label(), Some("reader"));
        assert_eq!(labelled.clone_and_reset().access_label(), Some("reader"));
        assert_eq!(
            labelled.clone_and_sample(SamplingKey::new()).access_label(),
            Some("reader"),
        );
        assert_eq!(
            labelled.with_mutated_scuba(|scuba| scuba).access_label(),
            Some("reader"),
        );
        assert_eq!(
            labelled
                .with_overridden_metadata(Arc::new(Metadata::default()))
                .access_label(),
            Some("reader"),
        );
    }

    #[mononoke::fbinit_test]
    async fn test_label_survives_into_spawned_task(fb: FacebookInit) {
        // Given: a labelled context.
        let labelled = CoreContext::test_mock(fb).with_access_label("reader");

        // When: a spawned task reads the label from its own clone.
        let ctx = labelled.clone();
        let label = mononoke::spawn_task(async move { ctx.access_label() })
            .await
            .expect("spawned task should not panic");

        // Then: the task sees the label.
        assert_eq!(label, Some("reader"));
    }
}
