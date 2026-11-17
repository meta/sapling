/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::mem::take;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use configloader::hg::ConfigSetHgExt;
use configloader::hg::RepoInfo;
use configmodel::Config;
use configmodel::ConfigExt;
use configset::ConfigSet;
use configset::config::Options;
use edenapi::SaplingRemoteApi;
use identity::sniff_root;
use log::warn;
use repo::Repo;
use repo::RepoMinimalInfo;

use crate::subscriber::Notification;
use crate::subscriber::Subscription;

const POLLING_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(crate) struct SlapiPoller {
    client: Option<Arc<dyn SaplingRemoteApi>>,
    // The rollout check and client construction consume the same loaded Repo.
    repo: Option<Repo>,
    decision: SlapiDecision,
    version: Option<u64>,
    next_root: usize,
    repo_load_attempts: u64,
    repo_load_failures: u64,
}

impl SlapiPoller {
    pub(crate) async fn is_enabled(&mut self, repo_roots: &[PathBuf]) -> bool {
        if !self.decision.needs_refresh(Instant::now()) {
            return self.decision.enabled();
        }

        let repo_roots = repo_roots.to_vec();
        let next_root = self.next_root;
        let old_client = self.client.take();
        let old_repo = self.repo.take();
        let loaded = tokio::task::spawn_blocking(move || {
            // Retiring a client may join its HTTP dispatcher threads.
            drop((old_client, old_repo));
            load_repository(&repo_roots, next_root, POLLING_TIMEOUT)
        })
        .await;
        let now = Instant::now();
        match loaded {
            Ok(loaded) => {
                self.repo_load_attempts = self.repo_load_attempts.saturating_add(loaded.attempts);
                self.repo_load_failures = self.repo_load_failures.saturating_add(loaded.failures);
                match loaded.result {
                    Ok((root, repo)) => {
                        self.next_root = root;
                        self.decision.refreshed(repo.is_some(), now);
                        self.repo = repo;
                    }
                    Err(err) => {
                        warn!(
                            "Failed to load the SLAPI rollout configuration: {err:#}; using legacy polling"
                        );
                        self.decision.failed(now);
                    }
                }
            }
            Err(err) => {
                warn!(
                    "Failed to load the SLAPI rollout configuration: {err}; using legacy polling"
                );
                self.decision.failed(now);
            }
        }
        self.decision.enabled()
    }

    pub(crate) fn take_sync_reason(&mut self, reason: &str, using_slapi: bool) -> String {
        let backend = if using_slapi { "slapi" } else { "interngraph" };
        let attempts = take(&mut self.repo_load_attempts);
        let failures = take(&mut self.repo_load_failures);
        format!(
            "{reason} (backend={backend}, repo_load_attempts={attempts}, repo_load_failures={failures})"
        )
    }

    pub(crate) async fn poll(
        &mut self,
        subscription: &Subscription,
        repo_roots: &[PathBuf],
    ) -> Result<Option<Notification>> {
        let result = self.poll_once(subscription, repo_roots).await;
        if result.is_err() {
            // Try another checkout's configuration/credentials after a failed request.
            self.next_root = self.next_root.saturating_add(1);
            self.decision.request_refresh();
            let client = self.client.take();
            let repo = self.repo.take();
            if client.is_some() || repo.is_some() {
                // HTTP client teardown may join dispatcher threads.
                if let Err(err) = tokio::task::spawn_blocking(move || drop((client, repo))).await {
                    warn!(
                        "Failed to release the SLAPI client for {} @ {}: {err}",
                        subscription.repo_name, subscription.workspace,
                    );
                }
            }
        }
        result
    }

    async fn poll_once(
        &mut self,
        subscription: &Subscription,
        repo_roots: &[PathBuf],
    ) -> Result<Option<Notification>> {
        let client = match &self.client {
            Some(client) => client.clone(),
            None => {
                if self.repo.is_none() && !self.is_enabled(repo_roots).await {
                    bail!("SLAPI polling is not enabled for the subscribed repositories");
                }
                let repo = self.repo.take().context("missing SLAPI repository")?;
                let client = tokio::task::spawn_blocking(move || {
                    repo.eden_api().context("constructing the SLAPI client")
                })
                .await
                .context("loading the CommitCloud SLAPI client")??;
                self.client = Some(client.clone());
                client
            }
        };
        let response = tokio::time::timeout(
            POLLING_TIMEOUT,
            client.cloud_workspace(
                subscription.workspace.clone(),
                subscription.repo_name.clone(),
            ),
        )
        .await
        .context("CommitCloud workspace poll timed out")?
        .context("polling the CommitCloud workspace")?;
        let workspace = response.data.context("reading the CommitCloud workspace")?;

        // Also emit the first observed version: an update can race with the
        // startup sync, before this poll establishes its baseline.
        if self
            .version
            .is_none_or(|version| workspace.version > version)
        {
            self.version = Some(workspace.version);
            Ok(Some(Notification {
                version: workspace.version,
            }))
        } else {
            Ok(None)
        }
    }
}

pub(crate) fn slapi_enabled(config: &dyn Config) -> bool {
    config
        .get_or("commitcloud", "use-slapi-subscriber", || false)
        .unwrap_or_else(|err| {
            warn!(
                "invalid 'commitcloud.use-slapi-subscriber' config: {err:#}; using legacy polling"
            );
            false
        })
}

// Bound staleness of successful reads, but retry unavailable configuration sooner.
const DECISION_REFRESH_INTERVAL: Duration = Duration::from_secs(900);
const DECISION_RETRY_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Default)]
struct SlapiDecision {
    enabled: bool,
    refresh_at: Option<Instant>,
}

impl SlapiDecision {
    fn enabled(&self) -> bool {
        self.enabled
    }

    fn needs_refresh(&self, now: Instant) -> bool {
        self.refresh_at.is_none_or(|refresh_at| now >= refresh_at)
    }

    fn refreshed(&mut self, enabled: bool, now: Instant) {
        self.enabled = enabled;
        self.refresh_at = Some(now + DECISION_REFRESH_INTERVAL);
    }

    fn failed(&mut self, now: Instant) {
        self.enabled = false;
        self.refresh_at = Some(now + DECISION_RETRY_INTERVAL);
    }

    fn request_refresh(&mut self) {
        self.refresh_at = None;
    }
}

struct RepositoryLoad {
    result: Result<(usize, Option<Repo>)>,
    attempts: u64,
    failures: u64,
}

fn load_repository(repo_roots: &[PathBuf], next_root: usize, timeout: Duration) -> RepositoryLoad {
    let mut loaded = RepositoryLoad {
        result: Err(anyhow!(
            "no subscribed repository has readable configuration"
        )),
        attempts: 0,
        failures: 0,
    };
    if repo_roots.is_empty() {
        return loaded;
    }
    let pinned = polling_config(timeout);

    for (index, root) in repo_roots
        .iter()
        .enumerate()
        .cycle()
        .skip(next_root % repo_roots.len())
        .take(repo_roots.len())
    {
        loaded.attempts += 1;
        // CommitCloud subscription records contain repo.sharedpath (.hg/.sl).
        let result = sniff_root(root)
            .and_then(|repo| {
                let (path, _) = repo.context("repository not found")?;
                let info = RepoMinimalInfo::from_repo_root(path)?;
                let mut config = pinned.clone();
                config.load(RepoInfo::Disk(&info), Options::default())?;
                Repo::load_with_config(info.path, config)
            })
            .with_context(|| format!("loading the repository for {}", root.display()));
        match result {
            Ok(repo) => {
                loaded.result = Ok((index, slapi_enabled(repo.config()).then_some(repo)));
                return loaded;
            }
            Err(err) => {
                loaded.failures += 1;
                warn!("{err:#}");
            }
        }
    }
    loaded
}

fn polling_config(timeout: Duration) -> ConfigSet {
    let mut config = ConfigSet::new().named("root");
    let opts = Options::default().source("scm_daemon").pin(true);
    // Bound the actual transfer, not just the future waiting for its response.
    config.set(
        "edenapi",
        "timeout.cloud/workspace",
        Some(format!("{}ms", timeout.as_millis())),
        &opts,
    );
    // The polling loop provides backoff and retries.
    config.set("edenapi", "max-retry-per-request", Some("0"), &opts);
    config.set("http", "dispatcher-threads", Some("1"), &opts);
    config
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::VecDeque;

    use async_trait::async_trait;
    use configset::Config;
    use edenapi::SaplingRemoteApiError;
    use edenapi::types::ServerError;
    use edenapi::types::WorkspaceData;
    use edenapi::types::WorkspaceDataResponse;
    use parking_lot::Mutex;

    use super::*;

    struct MockClient {
        responses: Mutex<VecDeque<edenapi::Result<WorkspaceDataResponse>>>,
    }

    #[async_trait]
    impl SaplingRemoteApi for MockClient {
        async fn cloud_workspace(
            &self,
            workspace: String,
            reponame: String,
        ) -> edenapi::Result<WorkspaceDataResponse> {
            assert_eq!(workspace, "user/test/workspace");
            assert_eq!(reponame, "test-repo");
            self.responses.lock().pop_front().unwrap()
        }
    }

    fn response(version: u64) -> edenapi::Result<WorkspaceDataResponse> {
        Ok(WorkspaceDataResponse {
            data: Ok(WorkspaceData {
                version,
                ..Default::default()
            }),
        })
    }

    fn subscription() -> Subscription {
        Subscription {
            repo_name: "test-repo".to_owned(),
            workspace: "user/test/workspace".to_owned(),
        }
    }

    #[test]
    fn polling_config_preserves_overrides() {
        let mut config = polling_config(Duration::from_millis(123));
        config.clear_unpinned();
        let opts = Options::default().source("repo").pin(false);
        for (section, name, value, expected) in [
            ("edenapi", "timeout.cloud/workspace", "60s", "123ms"),
            ("edenapi", "max-retry-per-request", "10", "0"),
            ("http", "dispatcher-threads", "0", "1"),
            (
                "paths",
                "default",
                "mononoke://test/repo",
                "mononoke://test/repo",
            ),
        ] {
            config.set(section, name, Some(value), &opts);
            assert_eq!(
                config.get(section, name).as_deref(),
                Some(expected),
                "{section}.{name}",
            );
        }
    }

    #[test]
    fn slapi_disabled_by_default_and_on_invalid_value() {
        let unset: BTreeMap<&str, &str> = BTreeMap::new();
        assert!(!slapi_enabled(&unset));

        let enabled: BTreeMap<&str, &str> = [("commitcloud.use-slapi-subscriber", "true")]
            .into_iter()
            .collect();
        assert!(slapi_enabled(&enabled));

        let disabled: BTreeMap<&str, &str> = [("commitcloud.use-slapi-subscriber", "false")]
            .into_iter()
            .collect();
        assert!(!slapi_enabled(&disabled));

        let invalid: BTreeMap<&str, &str> = [("commitcloud.use-slapi-subscriber", "maybe")]
            .into_iter()
            .collect();
        assert!(!slapi_enabled(&invalid));
    }

    #[test]
    fn generic_slapi_key_does_not_enable_subscriber() {
        let config: BTreeMap<&str, &str> =
            [("commitcloud.use_slapi", "true")].into_iter().collect();
        assert!(!slapi_enabled(&config));
    }

    #[tokio::test]
    async fn is_enabled_fails_closed_without_repositories() {
        let mut poller = SlapiPoller::default();
        assert!(!poller.is_enabled(&[]).await);
        assert!(poller.client.is_none());
        assert!(poller.repo.is_none());
        assert!(!poller.decision.needs_refresh(Instant::now()));
    }

    #[tokio::test]
    async fn cached_decision_reuses_client_until_refresh() {
        let mut poller = SlapiPoller {
            client: Some(Arc::new(MockClient {
                responses: Mutex::new(VecDeque::new()),
            })),
            version: Some(3),
            ..Default::default()
        };
        poller.decision.refreshed(true, Instant::now());
        assert!(poller.is_enabled(&[]).await);
        assert!(poller.client.is_some());
        assert_eq!(poller.repo_load_attempts, 0);

        poller.decision.request_refresh();
        assert!(!poller.is_enabled(&[]).await);
        assert!(poller.client.is_none());
        assert!(poller.repo.is_none());
        assert_eq!(poller.version, Some(3));
    }

    #[test]
    fn sync_reason_reports_backend_and_load_diagnostics_once() {
        let mut poller = SlapiPoller {
            repo_load_attempts: 3,
            repo_load_failures: 2,
            ..Default::default()
        };
        assert_eq!(
            poller.take_sync_reason("after recovering from errors", false),
            "after recovering from errors (backend=interngraph, repo_load_attempts=3, repo_load_failures=2)",
        );
        assert_eq!(
            poller.take_sync_reason("on new version update", true),
            "on new version update (backend=slapi, repo_load_attempts=0, repo_load_failures=0)",
        );
    }

    #[test]
    fn decision_refreshes_when_stale_or_requested() {
        let mut decision = SlapiDecision::default();
        let checked_at = Instant::now();
        assert!(decision.needs_refresh(checked_at));
        assert!(!decision.enabled());

        decision.refreshed(true, checked_at);
        assert!(decision.enabled());
        assert!(
            !decision
                .needs_refresh(checked_at + DECISION_REFRESH_INTERVAL - Duration::from_secs(1))
        );
        assert!(decision.needs_refresh(checked_at + DECISION_REFRESH_INTERVAL));

        decision.request_refresh();
        assert!(decision.needs_refresh(checked_at));
        decision.refreshed(false, checked_at);
        assert!(!decision.enabled());
        assert!(!decision.needs_refresh(checked_at));
    }

    #[test]
    fn failed_config_read_retries_before_regular_refresh() {
        let mut decision = SlapiDecision::default();
        let now = Instant::now();
        decision.refreshed(true, now);
        decision.failed(now);
        assert!(!decision.enabled());
        assert!(!decision.needs_refresh(now + DECISION_RETRY_INTERVAL - Duration::from_secs(1)));
        assert!(decision.needs_refresh(now + DECISION_RETRY_INTERVAL));
    }

    #[tokio::test]
    async fn poll_emits_initial_and_newer_versions() -> Result<()> {
        let mut poller = SlapiPoller {
            client: Some(Arc::new(MockClient {
                responses: Mutex::new([3, 3, 2, 7].into_iter().map(response).collect()),
            })),
            ..Default::default()
        };
        for expected in [Some(3), None, None, Some(7)] {
            assert_eq!(
                poller.poll(&subscription(), &[]).await?.map(|n| n.version),
                expected,
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn poll_errors_refresh_client_but_preserve_version() -> Result<()> {
        let client: Arc<dyn SaplingRemoteApi> = Arc::new(MockClient {
            responses: Mutex::new(VecDeque::from([
                response(3),
                Ok(WorkspaceDataResponse {
                    data: Err(ServerError::generic("workspace unavailable")),
                }),
                Err(SaplingRemoteApiError::NotSupported),
                response(3),
                response(4),
            ])),
        });
        let mut poller = SlapiPoller {
            client: Some(client.clone()),
            ..Default::default()
        };
        assert_eq!(poller.poll(&subscription(), &[]).await?.unwrap().version, 3);
        for _ in 0..2 {
            poller.decision.refreshed(true, Instant::now());
            assert!(poller.poll(&subscription(), &[]).await.is_err());
            assert!(poller.client.is_none());
            assert!(poller.decision.needs_refresh(Instant::now()));
            assert_eq!(poller.version, Some(3));
            assert_eq!(poller.repo_load_failures, 0);
            poller.client = Some(client.clone());
        }
        assert!(poller.poll(&subscription(), &[]).await?.is_none());
        assert_eq!(poller.poll(&subscription(), &[]).await?.unwrap().version, 4);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_poll_times_out_without_forgetting_version() -> Result<()> {
        use std::future::pending;

        struct PendingClient;

        #[async_trait]
        impl SaplingRemoteApi for PendingClient {
            async fn cloud_workspace(
                &self,
                _workspace: String,
                _reponame: String,
            ) -> edenapi::Result<WorkspaceDataResponse> {
                pending().await
            }
        }

        let mut poller = SlapiPoller {
            client: Some(Arc::new(PendingClient)),
            version: Some(3),
            ..Default::default()
        };
        let err = poller
            .poll(&subscription(), &[])
            .await
            .err()
            .context("expected timeout")?;
        assert!(err.is::<tokio::time::error::Elapsed>());
        assert!(poller.client.is_none());
        assert_eq!(poller.version, Some(3));
        Ok(())
    }

    #[tokio::test]
    async fn cleanup_failure_preserves_poll_error() -> Result<()> {
        struct PanickingClient;
        impl SaplingRemoteApi for PanickingClient {}
        impl Drop for PanickingClient {
            fn drop(&mut self) {
                panic!("test client teardown failed");
            }
        }
        let mut poller = SlapiPoller {
            client: Some(Arc::new(PanickingClient)),
            ..Default::default()
        };
        let err = poller
            .poll(&subscription(), &[])
            .await
            .err()
            .context("expected poll failure")?;
        assert!(matches!(
            err.downcast_ref::<SaplingRemoteApiError>(),
            Some(SaplingRemoteApiError::NotSupported),
        ));
        Ok(())
    }
}
