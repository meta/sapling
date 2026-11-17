/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use configloader::hg::ConfigSetHgExt;
use configloader::hg::RepoInfo;
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
    version: Option<u64>,
    next_root: usize,
}

impl SlapiPoller {
    pub(crate) async fn poll(
        &mut self,
        subscription: &Subscription,
        repo_roots: &[PathBuf],
    ) -> Result<Option<Notification>> {
        let result = self.poll_once(subscription, repo_roots).await;
        if result.is_err() {
            // Try another checkout's configuration/credentials after a failed request.
            self.next_root = self.next_root.saturating_add(1);
            if let Some(client) = self.client.take() {
                // HTTP client teardown may join dispatcher threads.
                if let Err(err) = tokio::task::spawn_blocking(move || drop(client)).await {
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
                let repo_roots = repo_roots.to_vec();
                let next_root = self.next_root;
                let (root, client) = tokio::task::spawn_blocking(move || {
                    create_client(&repo_roots, next_root, POLLING_TIMEOUT)
                })
                .await
                .context("loading the CommitCloud SLAPI client")??;
                self.next_root = root;
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

fn create_client(
    repo_roots: &[PathBuf],
    next_root: usize,
    timeout: Duration,
) -> Result<(usize, Arc<dyn SaplingRemoteApi>)> {
    if repo_roots.is_empty() {
        bail!("no subscribed repositories");
    }
    let pinned = polling_config(timeout);

    for (index, root) in repo_roots
        .iter()
        .enumerate()
        .cycle()
        .skip(next_root % repo_roots.len())
        .take(repo_roots.len())
    {
        // Subscriptions may name a shared .hg/.sl directory rather than a checkout root.
        let result = sniff_root(root)
            .and_then(|repo| {
                let (path, _) = repo.context("repository not found")?;
                let info = RepoMinimalInfo::from_repo_root(path)?;
                let mut config = pinned.clone();
                config.load(RepoInfo::Disk(&info), Options::default())?;
                Repo::load_with_config(info.path, config)?
                    .eden_api()
                    .context("constructing the SLAPI client")
            })
            .with_context(|| format!("loading the SLAPI client for {}", root.display()));
        match result {
            Ok(client) => return Ok((index, client)),
            Err(err) => warn!("{err:#}"),
        }
    }
    bail!("no subscribed repository has a usable SLAPI configuration")
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
            assert!(poller.poll(&subscription(), &[]).await.is_err());
            assert!(poller.client.is_none());
            assert_eq!(poller.version, Some(3));
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
