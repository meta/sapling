/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;

use anyhow::Error;
use anyhow::Result;
use clap::Args;
use fbinit::FacebookInit;
// Re-eport AliveService for convenience so callers do not have to get the services dependency to
// get AliveService.
pub use services::AliveService;
use services::Fb303Service;
use services::FbStatus;
use tokio::runtime::Handle;
use tracing::info;

use crate::AppExtension;

/// Command line arguments that fb303 for service
#[derive(Args, Debug)]
pub struct MonitoringArgs {
    /// Port for fb303 service
    // thrift_port alias is for compatibility with mononoke server
    // TODO: switch mononoke server to use the same flags as everybody.
    #[clap(long, alias = "thrift_port", value_name = "PORT")]
    pub fb303_thrift_port: Option<i32>,

    /// Spawn promethus metrics exporter listening on HOST:PORT, requires
    /// fb303_thrift_port to be set
    #[clap(long, requires("fb303_thrift_port"), value_name = "HOST:PORT")]
    #[cfg(fbcode_build)]
    pub prometheus_host_port: Option<String>,
}

impl MonitoringArgs {
    /// This is a lower-level function that requires you to spawn the stats aggregation future
    /// yourself. This is useful if you'd like to be able to drop it in order to cancel it.
    ///
    /// Usually starting the fb303 server and stats aggregation is done by functions like
    /// `MononokeApp::run_with_monitoring_and_logging`.
    pub fn start_monitoring_server<S: Fb303Service + Sync + Send + 'static>(
        &self,
        fb: FacebookInit,
        handle: &Handle,
        service_name: &str,
        service: S,
    ) -> Result<Option<()>, Error> {
        let service_name = service_name.to_string();
        self.fb303_thrift_port
            .map(|port| {
                info!("Initializing fb303 thrift server on port {}", port);

                thread::Builder::new()
                    .name("fb303_thrift_service".to_owned())
                    .spawn(move || {
                        services::run_service_framework(
                            fb,
                            service_name,
                            port,
                            0, // Disables separate status http server
                            Box::new(service),
                        )
                        .expect("failure while running thrift service framework")
                    })
                    .map_err(Error::from)?;

                #[cfg(fbcode_build)]
                {
                    if let Some(prometheus_host_port) = &self.prometheus_host_port {
                        info!(
                            "Initializing prometheus exporter on {}",
                            prometheus_host_port
                        );
                        fb303_prometheus_exporter::run_fb303_to_prometheus_exporter(
                            fb,
                            handle,
                            prometheus_host_port.clone(),
                            format!("[::0]:{port}"),
                        )?;
                    }
                }
                #[cfg(not(fbcode_build))]
                {
                    _ = &handle;
                }
                Ok(())
            })
            .transpose()
    }
}

pub struct MonitoringAppExtension;

impl AppExtension for MonitoringAppExtension {
    type Args = MonitoringArgs;
}

/// A FB303 service that reports healthy once set_ready has been called.
#[derive(Clone)]
pub struct ReadyFlagService {
    ready: Arc<AtomicBool>,
}

impl ReadyFlagService {
    pub fn new() -> Self {
        Self {
            ready: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::Relaxed);
    }
}

impl Fb303Service for ReadyFlagService {
    fn getStatus(&self) -> FbStatus {
        if self.ready.load(Ordering::Relaxed) {
            FbStatus::Alive
        } else {
            FbStatus::Starting
        }
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn ready_flag_preserves_exported_status_contract() {
        let service = ReadyFlagService::new();
        let clone = service.clone();
        assert_eq!(service.getStatus(), FbStatus::Starting);
        clone.set_ready();
        assert_eq!(service.getStatus(), FbStatus::Alive);
    }

    #[mononoke::test]
    fn alive_service_preserves_exported_boxed_trait_contract() {
        let service: Box<dyn Fb303Service> = AliveService::new();
        assert_eq!(service.getStatus(), FbStatus::Alive);
    }
}
