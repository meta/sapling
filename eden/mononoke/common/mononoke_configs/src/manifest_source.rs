/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Where a job's repo list and shared config come from: a tier manifest, or
//! one service's view of a deployment. Either is watched for changes.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use anyhow::Result;
use anyhow::bail;
use cached_config::ConfigHandle;
use cached_config::ConfigUpdateWatcher;
use metaconfig_parser::DeploymentLocation;
use metaconfig_parser::ServiceManifest;
use metaconfig_parser::service_view;
use repos::Deployment;
use repos::MononokeService;
use repos::TierManifest;
use stats::prelude::*;
use tracing::error;

define_stats! {
    prefix = "mononoke.config_refresh";
    deployment_view_failure: timeseries(Average, Sum, Count),
}

/// The source of a job's [`ServiceManifest`].
pub(crate) struct ManifestSource {
    kind: SourceKind,
    state: Mutex<State>,
}

enum SourceKind {
    Tier(ConfigHandle<TierManifest>),
    Deployment {
        handle: ConfigHandle<Deployment>,
        location: DeploymentLocation,
        service: MononokeService,
        legacy_tier: String,
    },
}

/// What the source has produced so far.
struct State {
    /// The manifest in use and the config it came from.
    applied: Applied,
    /// A newer config whose conversion failed. It is not tried again until
    /// the config changes, and the applied manifest stays in use.
    rejected: Option<Source>,
}

/// A manifest and the config, and configerator version, it came from.
#[derive(Clone)]
struct Applied {
    source: Source,
    version: Option<String>,
    manifest: Arc<ServiceManifest>,
}

/// The config a manifest is produced from, compared by identity.
#[derive(Clone)]
enum Source {
    Tier(Arc<TierManifest>),
    Deployment(Arc<Deployment>),
}

impl Source {
    fn is(&self, other: &Source) -> bool {
        match (self, other) {
            (Source::Tier(a), Source::Tier(b)) => Arc::ptr_eq(a, b),
            (Source::Deployment(a), Source::Deployment(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl ManifestSource {
    /// A tier manifest.
    pub(crate) fn tier(handle: ConfigHandle<TierManifest>) -> Self {
        let (manifest, info) = handle.get_with_version();
        let applied = Applied {
            manifest: Arc::new(ServiceManifest::from(TierManifest::clone(&manifest))),
            source: Source::Tier(manifest),
            version: info.map(|info| info.version),
        };
        Self::with(SourceKind::Tier(handle), applied)
    }

    /// `service`'s view of the deployment `handle` reads. Fails if the
    /// deployment has no such service.
    pub(crate) fn deployment(
        handle: ConfigHandle<Deployment>,
        location: DeploymentLocation,
        service: MononokeService,
    ) -> Result<Self> {
        let (deployment, info) = handle.get_with_version();
        let view = service_view(&deployment, service, &location)?;
        let applied = Applied {
            source: Source::Deployment(deployment),
            version: info.map(|info| info.version),
            manifest: Arc::new(view.manifest),
        };
        let kind = SourceKind::Deployment {
            handle,
            location,
            service: view.service,
            legacy_tier: view.legacy_tier,
        };
        Ok(Self::with(kind, applied))
    }

    fn with(kind: SourceKind, applied: Applied) -> Self {
        Self {
            kind,
            state: Mutex::new(State {
                applied,
                rejected: None,
            }),
        }
    }

    /// The service and its legacy tier, the tier whose `tier_overrides`
    /// resolve repo configs, when the source is a deployment.
    pub(crate) fn deployment_service(&self) -> Option<(MononokeService, &str)> {
        match &self.kind {
            SourceKind::Tier(_) => None,
            SourceKind::Deployment {
                service,
                legacy_tier,
                ..
            } => Some((*service, legacy_tier)),
        }
    }

    /// The current manifest. The service's legacy tier is fixed for the life of
    /// the process: a deployment change that would change it is refused, and the
    /// last good manifest stays.
    pub(crate) fn get(&self) -> Arc<ServiceManifest> {
        self.current().manifest
    }

    /// The configerator version of the config the current manifest came from,
    /// when it has one. A refused config's version is never reported.
    pub(crate) fn version(&self) -> Option<String> {
        self.current().version
    }

    /// The deployment the current manifest came from, and where it was read
    /// from, when the source is a deployment.
    pub(crate) fn applied_deployment(&self) -> Option<(DeploymentLocation, Arc<Deployment>)> {
        let SourceKind::Deployment { location, .. } = &self.kind else {
            return None;
        };
        match self.current().source {
            Source::Deployment(deployment) => Some((location.clone(), deployment)),
            Source::Tier(_) => None,
        }
    }

    fn current(&self) -> Applied {
        let (source, version) = self.read();
        {
            let state = self.lock();
            if state.applied.source.is(&source)
                || state.rejected.as_ref().is_some_and(|r| r.is(&source))
            {
                return state.applied.clone();
            }
        }

        // Convert outside the lock: a deployment view goes over every repo.
        let converted = self.convert(&source);

        let mut state = self.lock();
        // A reader that saw a newer config may have stored it meanwhile.
        let still_current = self.read().0.is(&source);
        match converted {
            Ok(manifest) => {
                let applied = Applied {
                    source,
                    version,
                    manifest: Arc::new(manifest),
                };
                if still_current {
                    state.applied = applied.clone();
                    state.rejected = None;
                }
                applied
            }
            Err(e) => {
                if still_current && !state.rejected.as_ref().is_some_and(|r| r.is(&source)) {
                    STATS::deployment_view_failure.add_value(1);
                    error!(
                        "Refused a config change, keeping the config at version {}: {e:#}",
                        state.applied.version.as_deref().unwrap_or("unknown")
                    );
                    state.rejected = Some(source);
                }
                state.applied.clone()
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing panics while holding the lock, so the state is consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn read(&self) -> (Source, Option<String>) {
        let (source, info) = match &self.kind {
            SourceKind::Tier(handle) => {
                let (manifest, info) = handle.get_with_version();
                (Source::Tier(manifest), info)
            }
            SourceKind::Deployment { handle, .. } => {
                let (deployment, info) = handle.get_with_version();
                (Source::Deployment(deployment), info)
            }
        };
        (source, info.map(|info| info.version))
    }

    fn convert(&self, source: &Source) -> Result<ServiceManifest> {
        match (&self.kind, source) {
            (SourceKind::Tier(_), Source::Tier(manifest)) => {
                Ok(ServiceManifest::from(TierManifest::clone(manifest)))
            }
            (
                SourceKind::Deployment {
                    location,
                    service,
                    legacy_tier,
                    ..
                },
                Source::Deployment(deployment),
            ) => {
                let view = service_view(deployment, *service, location)?;
                if view.legacy_tier != *legacy_tier {
                    bail!(
                        "The deployment at {location} changed the legacy tier of {service} from \
                         {legacy_tier} to {}, which needs a restart",
                        view.legacy_tier
                    );
                }
                Ok(view.manifest)
            }
            _ => bail!("A config of the wrong kind for its source"),
        }
    }

    /// A watcher that fires when the underlying config changes.
    pub(crate) fn watcher(&self) -> Result<ManifestWatcher> {
        Ok(match &self.kind {
            SourceKind::Tier(handle) => ManifestWatcher::Tier(handle.watcher()?),
            SourceKind::Deployment { handle, .. } => ManifestWatcher::Deployment(handle.watcher()?),
        })
    }
}

/// Fires when a [`ManifestSource`]'s config changes.
pub(crate) enum ManifestWatcher {
    Tier(ConfigUpdateWatcher<TierManifest>),
    Deployment(ConfigUpdateWatcher<Deployment>),
}

impl ManifestWatcher {
    pub(crate) async fn wait_for_next(&mut self) -> Result<()> {
        match self {
            ManifestWatcher::Tier(watcher) => watcher.wait_for_next().await.map(|_| ()),
            ManifestWatcher::Deployment(watcher) => watcher.wait_for_next().await.map(|_| ()),
        }
    }
}
