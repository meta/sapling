/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;
use std::str;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use log::debug;
use log::error;
use log::info;
use parking_lot::Mutex;
use reqwest::Client;
use reqwest::Response;
use reqwest::Url;
use reqwest::header::CONNECTION;
use serde::Deserialize;
use serde_json::Value;

use crate::ActionsMap;
use crate::action::CloudSyncTrigger;
use crate::config::CommitCloudConfig;
use crate::error::*;
use crate::receiver::CommandName;
use crate::receiver::CommandName::CommitCloudCancelSubscriptions;
use crate::receiver::CommandName::CommitCloudRestartSubscriptions;
use crate::receiver::CommandName::CommitCloudStartSubscriptions;
use crate::slapi::SlapiPoller;
use crate::util;

const POLLING_INTERVAL_SEC: u64 = 3;
const POLLING_CONNECTION_KEEP_ALIVE_SEC: u64 = 5 * 60; // 5 minutes
const POLLING_ERROR_DELAY_SEC: u64 = 10;

#[derive(Deserialize)]
pub struct Notification {
    pub(crate) version: u64,
}

#[derive(PartialEq, Eq, Hash)]
pub struct Subscription {
    pub(crate) repo_name: String,
    pub(crate) workspace: String,
}

/// WorkspaceSubscriberService manages a set of running subscriptions
/// and trigger `hg cloud sync` on notifications
/// The workflow is simple:
/// * fire `hg cloud sync` on start in every repo
/// * read and start current set of subscriptions and
///   fire `hg cloud sync` on notifications
/// * fire `hg cloud sync` when connection recovers
/// * also provide actions (callbacks) to a few TcpReceiver commands
///
/// The commands are:
/// * "start_subscriptions"
/// * "restart_subscriptions"
/// * "cancel_subscriptions"
///
/// If a command comes, gracefully cancel all previous subscriptions
/// and restart if requested
///
/// Main use case:
/// * if a client (hg) add itself as a new subscriber (hg cloud join),
///   it is also client's responsibility to send "restart_subscriptions" command
///   same for unsubscribing (hg cloud leave)
///
/// The serve function starts the service
pub struct WorkspaceSubscriberService {
    /// Legacy notification endpoint, or None to poll workspace versions via SLAPI.
    pub(crate) polling_update_url: Option<String>,

    /// OAuth token path (optional) for access to Commit Cloud SSE endpoint
    pub(crate) user_token_path: Option<PathBuf>,

    /// Directory with connected subscribers
    pub(crate) connected_subscribers_path: PathBuf,

    /// Number of retries for `hg cloud sync`
    pub(crate) cloudsync_retries: u32,

    /// Channel for communication between threads
    pub(crate) channel: (mpsc::Sender<CommandName>, mpsc::Receiver<CommandName>),

    /// Interrupt barrier for joining threads
    pub(crate) interrupt: Arc<AtomicBool>,
}

impl WorkspaceSubscriberService {
    pub fn new(config: &CommitCloudConfig) -> Result<WorkspaceSubscriberService> {
        Ok(WorkspaceSubscriberService {
            polling_update_url: if config.use_slapi {
                None
            } else {
                Some(
                    config
                        .polling_update_url
                        .clone()
                        .ok_or(ErrorKind::CommitCloudConfigError(
                            "undefined 'polling_update_url'",
                        ))?,
                )
            },
            user_token_path: config.user_token_path.clone(),
            connected_subscribers_path: config.connected_subscribers_path.clone().ok_or(
                ErrorKind::CommitCloudConfigError("undefined 'connected_subscribers_path'"),
            )?,
            cloudsync_retries: config.cloudsync_retries,
            channel: mpsc::channel(),
            interrupt: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn actions(&self) -> ActionsMap {
        let mut actions = ActionsMap::new();
        actions.insert(CommitCloudRestartSubscriptions, {
            let sender = Mutex::new(self.channel.0.clone());
            let interrupt = self.interrupt.clone();
            Box::new(
                move || match sender.lock().send(CommitCloudRestartSubscriptions) {
                    Err(err) => error!(
                        "Send CommitCloudRestartSubscriptions via mpsc::channel failed, reason: {err}"
                    ),
                    Ok(_) => {
                        info!("Restart subscriptions can take a while because it is graceful");
                        interrupt.store(true, Ordering::Relaxed);
                    }
                },
            )
        });
        actions.insert(CommitCloudCancelSubscriptions, {
            let sender = Mutex::new(self.channel.0.clone());
            let interrupt = self.interrupt.clone();
            Box::new(
                move || match sender.lock().send(CommitCloudCancelSubscriptions) {
                    Err(err) => error!(
                        "Send CommitCloudCancelSubscriptions via mpsc::channel failed with {err}"
                    ),
                    Ok(_) => {
                        info!("Cancel subscriptions can take a while because it is graceful");
                        interrupt.store(true, Ordering::Relaxed);
                    }
                },
            )
        });
        actions.insert(CommitCloudStartSubscriptions, {
            let sender = Mutex::new(self.channel.0.clone());
            let interrupt = self.interrupt.clone();
            Box::new(
                move || match sender.lock().send(CommitCloudStartSubscriptions) {
                    Err(err) => error!(
                        "Send CommitCloudStartSubscriptions via mpsc::channel failed with {err}"
                    ),
                    Ok(_) => {
                        info!("Starting subscriptions.");
                        interrupt.store(true, Ordering::Relaxed);
                    }
                },
            )
        });
        actions
    }

    pub fn serve(self) -> Result<tokio::task::JoinHandle<Result<()>>> {
        self.channel.0.send(CommitCloudStartSubscriptions)?;
        Ok(tokio::spawn(async move {
            info!("Starting CommitCloud Workspace Subscriber Service");
            loop {
                let command = self.channel.1.recv_timeout(Duration::from_mins(1));
                match command {
                    Ok(CommitCloudCancelSubscriptions) => {
                        info!(
                            "All previous subscriptions have been canceled! \
                             Waiting for another commands..."
                        );
                        self.interrupt.store(false, Ordering::Relaxed);
                    }
                    Ok(CommitCloudRestartSubscriptions) => {
                        info!(
                            "All previous subscriptions have been canceled! \
                             Restarting subscriptions..."
                        );
                        self.interrupt.store(false, Ordering::Relaxed);

                        let subscriptions = self.run_polling_updates()?;
                        for subscription in subscriptions {
                            let _ = subscription.await;
                        }
                    }
                    Ok(CommitCloudStartSubscriptions) => {
                        info!("Starting subscriptions...");
                        self.interrupt.store(false, Ordering::Relaxed);
                        let subscriptions = self.run_polling_updates()?;
                        for subscription in subscriptions {
                            let _ = subscription.await;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if !util::read_subscriptions(&self.connected_subscribers_path)?.is_empty() {
                            self.channel.0.send(CommitCloudStartSubscriptions)?;
                        }
                        continue;
                    }
                    Err(e) => {
                        error!("Receive from mpsc::channel failed with {e}");
                        bail!("Receive and wait on mpsc::channel failed with {e}");
                    }
                }
            }
        }))
    }

    /// This helper function builds the URL for the notification polling endpoint
    fn build_polling_update_url(
        polling_update_url: &str,
        access_token: &util::Token,
        subscription: &Subscription,
        polling_cursor: &Option<String>,
    ) -> Result<Url> {
        let mut polling_update_url = Url::parse(polling_update_url)?;

        polling_update_url
            .query_pairs_mut()
            .append_pair("workspace", &subscription.workspace)
            .append_pair("repo_name", &subscription.repo_name);

        match access_token.token_type {
            util::TokenType::OAuth => {
                polling_update_url
                    .query_pairs_mut()
                    .append_pair("access_token", &access_token.token);
            }
            util::TokenType::Cat => {
                polling_update_url
                    .query_pairs_mut()
                    .append_pair("cat_app", &util::COMMIT_CLOUD_APP_ID.to_string())
                    .append_pair("crypto_auth_tokens", &access_token.token);
            }
        }

        if let Some(cursor) = polling_cursor {
            polling_update_url
                .query_pairs_mut()
                .append_pair("polling_cursor", cursor);
        }

        Ok(polling_update_url)
    }

    /// This helper function to parse the response from the notification polling endpoint
    /// It returns the latest notification (optional) data and optional cursor
    ///
    /// if 200 OK returns json in one of the following formats:
    ///
    /// For valid responses:
    /// {
    ///  "rc": 0,
    ///  "new_cursor": "<cursor as string>",
    ///  "payload": [
    ///        { "notification_data": <thrift structure NotificationData serialized into a json string using Thrift JSON serialization> }
    ///   ]
    /// }
    /// The exact format of the notification data: https://www.internalfb.com/code/fbsource/fbcode/scm/commitcloud/if/CommitCloudService.thrift?lines=43
    ///
    /// Some errors are embedded into the response.
    ///
    /// For errors:
    /// {
    ///  "rc": 1,
    ///  "error": "some error message"
    /// }
    async fn parse_polling_update_response(
        response: Response,
        sid: &str,
    ) -> Result<(Option<Notification>, Option<String>)> {
        let body = response.text().await?;
        let parsed_body: Value = serde_json::from_str(&body)?;
        if let Some(err) = parsed_body.get("error") {
            error!("{sid}: unexpected error: {err}");
            return Err(ErrorKind::PollingUpdatesServerError(err.to_string()).into());
        }
        let cursor = parsed_body
            .get("new_cursor")
            .and_then(|v| v.as_str().map(str::to_string));

        match parsed_body.get("payload").and_then(|v| v.as_array()) {
            Some(payloads) if payloads.is_empty() => {
                debug!("{sid}: Success, received an empty payload");
                Ok((None, cursor))
            }
            Some(payloads) => {
                info!("{sid}: Success, received non empty payload!");
                let maybe_notification: Option<Notification> = payloads
                    .iter()
                    .filter_map(|v| {
                        v.get("notification_data").map(|notification_object| {
                            notification_object
                                .as_str()
                                .map(|s| serde_json::from_str::<Notification>(s).ok())
                        })
                    })
                    .flatten()
                    .filter_map(std::convert::identity)
                    .max_by_key(|n| n.version);

                if let Some(notification) = &maybe_notification {
                    info!(
                        "{}: Received notification update for version {}!",
                        sid, notification.version
                    );
                }
                Ok((maybe_notification, cursor))
            }
            _ => {
                error!("{sid}: payload is missing");
                Err(ErrorKind::PollingUpdatesPayloadError.into())
            }
        }
    }

    /// This helper function to poll a single notification
    ///
    /// Cursor represents a position in the underlying pubsub queue.
    /// Once non empty cursor is returned, subsequent calls will need to provide it as an argument.
    /// If intermediate polls do not return any updates,
    /// they will not return a cursor, so we need to keep track of the latest non empty cursor.
    async fn poll_single_update(
        client: &Client,
        subscription: &Subscription,
        polling_update_url: &str,
        access_token: &util::Token,
        polling_cursor: &Option<String>,
    ) -> Result<(Option<Notification>, Option<String>)> {
        let sid = format!(
            "({} @ {}) [Poll Update]",
            subscription.repo_name, subscription.workspace
        );
        let url = Self::build_polling_update_url(
            polling_update_url,
            access_token,
            subscription,
            polling_cursor,
        )?;
        let response = client
            .get(url)
            .header(CONNECTION, "Keep-Alive")
            .send()
            .await?;
        match response.status() {
            reqwest::StatusCode::OK => Self::parse_polling_update_response(response, &sid).await,
            reqwest::StatusCode::UNAUTHORIZED => {
                error!("{} Need to grab a new token", &sid);
                Err(ErrorKind::PollingUpdatesUnauthorizedError.into())
            }
            status => {
                error!("{} Unexpected error: {:?}", &sid, response);
                Err(ErrorKind::PollingUpdatesHttpError(status).into())
            }
        }
    }

    /// This helper function reads the list of current connected subscribers
    /// It starts polling updates for the connected repos/workspaces
    /// All tasks keep checking the interrupt flag and join gracefully if it is restart or stop
    fn run_polling_updates(&self) -> Result<Vec<tokio::task::JoinHandle<()>>> {
        util::read_subscriptions(&self.connected_subscribers_path)?
            .into_iter()
            .map(|(subscription, repo_roots)| {
                self.run_polling_updates_for_repo_workspace(subscription, repo_roots)
            })
            .collect::<Result<Vec<tokio::task::JoinHandle<()>>>>()
    }

    /// Helper function to run polling updates for a single repo/workspace
    fn run_polling_updates_for_repo_workspace(
        &self,
        subscription: Subscription,
        repo_roots: Vec<PathBuf>,
    ) -> Result<tokio::task::JoinHandle<()>> {
        let sid = format!("({} @ {})", subscription.repo_name, subscription.workspace);
        let cloudsync_retries = self.cloudsync_retries;
        let user_token_path = self.user_token_path.clone();
        let polling_update_url = self.polling_update_url.clone();
        let interrupt = self.interrupt.clone();

        Ok(tokio::spawn(async move {
            let sync_me = |reason: &'static str, version: Option<u64>| {
                for repo_root in repo_roots.iter() {
                    info!(
                        "{} Fire CloudSyncTrigger in '{}' {}",
                        sid,
                        repo_root.display(),
                        reason,
                    );
                    {
                        let workspace = subscription.workspace.clone();
                        let sid = sid.clone();
                        let repo_root = repo_root.clone();
                        tokio::spawn(async move {
                            // log outputs, results and continue even if unsuccessful
                            let _res = CloudSyncTrigger::fire(
                                &sid,
                                repo_root,
                                cloudsync_retries,
                                version,
                                workspace,
                                format!("scm_daemon: {reason}"),
                            );
                        });
                    }
                }
            };
            sync_me("before starting polling updates", None);
            if interrupt.load(Ordering::Relaxed) {
                return;
            }
            info!("{sid} Start polling updates...");

            let interngraph = match polling_update_url {
                Some(url) => {
                    let client = match Client::builder()
                        .http2_keep_alive_while_idle(true)
                        .http2_keep_alive_interval(Duration::from_secs(
                            POLLING_CONNECTION_KEEP_ALIVE_SEC,
                        ))
                        .build()
                    {
                        Ok(client) => client,
                        Err(err) => {
                            error!(
                                "{sid} Cancelling subscription: cannot build polling client: {err}"
                            );
                            interrupt.store(true, Ordering::Relaxed);
                            return;
                        }
                    };
                    Some((url, client))
                }
                None => None,
            };

            let mut slapi = SlapiPoller::default();
            let mut cursor = None;
            let mut access_token = None;
            let mut long_sleep_after_fail = false;

            while !interrupt.load(Ordering::Relaxed) {
                let result = if let Some((url, client)) = &interngraph {
                    if access_token.as_ref().is_none() {
                        match util::read_or_generate_access_token(&user_token_path) {
                            Err(err) => {
                                error!(
                                    "{sid} Cancelling this task due to unexpected error with token creation: {err}..."
                                );
                                interrupt.store(true, Ordering::Relaxed);
                                return;
                            }
                            Ok(token) => {
                                access_token = Some(token);
                            }
                        }
                    }
                    Self::poll_single_update(
                        client,
                        &subscription,
                        url,
                        access_token.as_ref().unwrap(),
                        &cursor,
                    )
                    .await
                } else {
                    slapi
                        .poll(&subscription, &repo_roots)
                        .await
                        .map(|update| (update, None))
                };
                match result {
                    Ok((maybe_update, maybe_new_cursor)) => {
                        if maybe_new_cursor.is_some() {
                            cursor = maybe_new_cursor;
                        }
                        match maybe_update {
                            Some(new_update) => {
                                sync_me("on new version update", Some(new_update.version));
                            }
                            None => {
                                // sync since we had probably missed updates
                                if long_sleep_after_fail {
                                    sync_me("after recovering from errors", None);
                                }
                            }
                        }
                        long_sleep_after_fail = false;
                        tokio::time::sleep(Duration::from_secs(POLLING_INTERVAL_SEC)).await;
                    }
                    Err(err)
                        if matches!(
                            err.downcast_ref(),
                            Some(&ErrorKind::PollingUpdatesUnauthorizedError)
                        ) =>
                    {
                        info!("{sid} Access token is probably expired, retrying...");
                        // clean up the token and try again
                        access_token = None;
                        continue;
                    }
                    Err(err) => {
                        error!("{sid} Polling updates failed with {err:#}");
                        long_sleep_after_fail = true;
                        // sleep longer before trying again
                        tokio::time::sleep(Duration::from_secs(POLLING_ERROR_DELAY_SEC)).await;
                    }
                }
            }
        }))
    }
}
