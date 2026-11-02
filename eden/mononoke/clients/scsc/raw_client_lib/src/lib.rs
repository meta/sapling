/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Raw SCS client.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

#[cfg(fbcode_build)]
mod facebook;
#[cfg(not(fbcode_build))]
mod oss;

#[cfg(fbcode_build)]
pub use facebook::SCS_DEFAULT_TIER;
#[cfg(fbcode_build)]
pub use facebook::ScsClientBuilder;
#[cfg(fbcode_build)]
pub use facebook::ScsClientHostBuilder;
pub use source_control as thrift;
use source_control_clients::SourceControlService;

#[derive(Clone)]
pub struct ScsClient {
    client: Arc<dyn SourceControlService + Sync>,
    partial_response: Arc<AtomicBool>,
    partial_groups: Arc<Mutex<Vec<thrift::OmittedRestrictionGroup>>>,
    correlator: Option<String>,
}

impl ScsClient {
    pub(crate) fn new(
        client: Arc<dyn SourceControlService + Sync>,
        correlator: Option<String>,
    ) -> Self {
        Self {
            client,
            partial_response: Arc::new(AtomicBool::new(false)),
            partial_groups: Arc::new(Mutex::new(Vec::new())),
            correlator,
        }
    }

    /// Return the correlator for this SCS client.
    pub fn get_client_corrrelator(&self) -> Option<String> {
        self.correlator.clone()
    }

    /// Record that a response seen by this client carried the
    /// partial-response signal, keeping its omission groups for the final
    /// note. Never reset: a client serves one invocation, and any partial
    /// response taints its output.
    pub fn note_partial(&self, groups: Vec<thrift::OmittedRestrictionGroup>) {
        self.partial_response.store(true, Ordering::Relaxed);
        self.partial_groups
            .lock()
            .expect("lock poisoned")
            .extend(groups);
    }

    /// Whether any response seen by this client was partial.
    pub fn is_partial(&self) -> bool {
        self.partial_response.load(Ordering::Relaxed)
    }

    /// Omission groups stashed from every partial response seen, unmerged.
    pub fn partial_groups(&self) -> Vec<thrift::OmittedRestrictionGroup> {
        self.partial_groups.lock().expect("lock poisoned").clone()
    }
}

impl std::ops::Deref for ScsClient {
    type Target = dyn SourceControlService + Sync;

    fn deref(&self) -> &Self::Target {
        &*self.client
    }
}

#[cfg(not(fbcode_build))]
impl ScsClient {
    /// Connect to a loopback OSS SCS server.
    ///
    /// The OSS transport does not provide TLS, authentication, tier routing,
    /// client correlators, or production service discovery.
    pub async fn from_host_port(host_port: &str) -> anyhow::Result<Self> {
        oss::from_host_port(host_port)
    }
}
