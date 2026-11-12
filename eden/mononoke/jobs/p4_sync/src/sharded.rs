/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use async_trait::async_trait;
use bookmarks::BookmarkKey;
use context::CoreContext;
use executor_lib::RepoShardedProcess;
use executor_lib::RepoShardedProcessExecutor;
use executor_lib::args::ShardedExecutorArgs;
use fbinit::FacebookInit;
use metaconfig_types::RepoConfig;
use metaconfig_types::RepoConfigRef;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArgs;
use p4_import::ImportOptions;
use p4_source::P4Cli;
use p4_source::P4CliSource;
use repo_identity::RepoIdentityRef;
use sharding_ext::RepoShard;
use stats::define_stats;
use stats::prelude::DynamicSingletonCounter;
use tokio::sync::Notify;

use crate::Repo;

const SM_CLEANUP_TIMEOUT_SECS: u64 = 120;
const BOOKMARK: &str = "master";
/// Off, or not on for the repo (switch = repo name): the job holds the shard and imports
/// nothing.
const ENABLED_JUST_KNOB: &str = "scm/mononoke:p4_sync_enabled";
/// Well under `SM_CLEANUP_TIMEOUT_SECS`, so a stop or the exit file is seen in time.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
/// Failed batches of a single changelist in a row before the repo counts as stuck.
const STUCK_AFTER: u32 = 3;

define_stats! {
    prefix = "mononoke.p4_sync";
    // 1 while the repo is stuck on a changelist it can't import, for alerts.
    stuck: dynamic_singleton_counter("stuck.{}", (repo: String)),
}

pub(crate) struct Settings {
    pub(crate) exit_file: PathBuf,
    /// The Perforce servers this job imports from, by `p4_source.server_alias`.
    pub(crate) servers: HashMap<String, P4Cli>,
    pub(crate) batch_size: NonZeroUsize,
    pub(crate) idle_interval: Duration,
}

/// After `failures` failed batches in a row: 5s, 10s, 20s, … up to `MAX_RETRY_DELAY`.
pub(crate) fn retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    (Duration::from_secs(5) * 2u32.pow(doublings)).min(MAX_RETRY_DELAY)
}

/// Halved on each failed batch, down to 1, so a changelist that can't be imported ends up in
/// a batch of its own once the ones before it are imported; doubled on each success, back up
/// to `--batch-size`. Failures before any changelist is fetched (the prepare stage, e.g. p4
/// unreachable) leave it alone.
pub(crate) struct BatchSize {
    full: usize,
    current: usize,
    failed_alone: u32,
}

impl BatchSize {
    pub(crate) fn new(full: NonZeroUsize) -> Self {
        Self {
            full: full.get(),
            current: full.get(),
            failed_alone: 0,
        }
    }

    pub(crate) fn get(&self) -> usize {
        self.current
    }

    pub(crate) fn succeeded(&mut self) {
        self.current = self.current.saturating_mul(2).min(self.full);
        self.failed_alone = 0;
    }

    pub(crate) fn failed(&mut self) {
        if self.current == 1 {
            self.failed_alone += 1;
        }
        self.current = (self.current / 2).max(1);
    }

    /// The next changelist has failed `STUCK_AFTER` times in a row on its own.
    pub(crate) fn stuck(&self) -> bool {
        self.failed_alone >= STUCK_AFTER
    }
}

/// Clears the repo's `stuck` gauge however its loop ends, a panic or a dropped future included.
struct ClearStuckOnDrop {
    fb: FacebookInit,
    repo_name: String,
}

impl Drop for ClearStuckOnDrop {
    fn drop(&mut self) {
        STATS::stuck.set_value(self.fb, 0, (self.repo_name.clone(),));
    }
}

/// What a shard's loop is doing, logged once each time it changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Held,
    Disabled,
    Importing,
    CaughtUp,
}

fn log_state(current: &mut Option<State>, new: State, message: impl FnOnce() -> String) {
    if *current != Some(new) {
        tracing::info!("{}", message());
        *current = Some(new);
    }
}

/// Only `Depot` imports; the others hold the shard. `P` is the server entry, a `P4Cli` in
/// the job.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConfiguredDepot<'a, P> {
    NotAMirror,
    /// `p4_source.server_alias` names a server this job has no `--p4-server` for.
    UnknownServer {
        configured: String,
    },
    Depot {
        alias: &'a str,
        depot: String,
        p4: &'a P,
    },
}

pub(crate) fn configured_depot<'a, P>(
    config: &RepoConfig,
    servers: &'a HashMap<String, P>,
) -> ConfiguredDepot<'a, P> {
    let Some(p4_source) = &config.p4_source else {
        return ConfiguredDepot::NotAMirror;
    };
    match servers.get_key_value(&p4_source.server_alias) {
        None => ConfiguredDepot::UnknownServer {
            configured: p4_source.server_alias.clone(),
        },
        Some((alias, p4)) => ConfiguredDepot::Depot {
            alias,
            depot: p4_source.depot.clone(),
            p4,
        },
    }
}

pub(crate) fn import_enabled(repo_name: &str) -> bool {
    justknobs::eval(ENABLED_JUST_KNOB, None, Some(repo_name))
}

fn exit_file_present(exit_file: &Path) -> bool {
    exit_file.exists()
}

pub(crate) struct P4SyncProcess {
    app: Arc<MononokeApp>,
    settings: Arc<Settings>,
}

/// What a shard imports: its repo's depot, on the server the client reaches.
pub(crate) struct Source {
    alias: String,
    depot: String,
    p4: P4Cli,
}

#[async_trait]
impl RepoShardedProcess for P4SyncProcess {
    async fn setup(&self, shard: &RepoShard) -> Result<Arc<dyn RepoShardedProcessExecutor>> {
        if exit_file_present(&self.settings.exit_file) {
            tracing::warn!("Exit file detected, refusing shard {}", shard);
            return Err(anyhow!("Exit file detected, refusing shard {shard}"));
        }
        let repo_args = RepoArgs::from_repo_name(shard.repo_name.clone());
        let repo: Repo = self.app.open_repo(&repo_args).await?;
        let source = match configured_depot(repo.repo_config(), &self.settings.servers) {
            ConfiguredDepot::Depot { alias, depot, p4 } => Ok(Source {
                alias: alias.to_owned(),
                depot,
                p4: p4.clone(),
            }),
            ConfiguredDepot::NotAMirror => Err("no p4_source in its repo config".to_owned()),
            ConfiguredDepot::UnknownServer { configured } => Err(format!(
                "its p4_source.server_alias is {configured}, which this job has no --p4-server for"
            )),
        };
        if let Err(reason) = &source {
            tracing::warn!(
                "{}: {reason}: holding its shard, importing nothing",
                shard.repo_name
            );
        }

        Ok(Arc::new(P4SyncProcessExecutor {
            app: self.app.clone(),
            repo,
            source,
            settings: self.settings.clone(),
            cancellation_requested: AtomicBool::new(false),
            wake: Notify::new(),
        }))
    }
}

pub(crate) struct P4SyncProcessExecutor {
    app: Arc<MononokeApp>,
    repo: Repo,
    /// Err: why the shard is held, importing nothing.
    source: Result<Source, String>,
    settings: Arc<Settings>,
    cancellation_requested: AtomicBool,
    /// Ends a wait or a running batch when the shard is stopped.
    wake: Notify,
}

impl P4SyncProcessExecutor {
    /// A context per batch: its own `session_uuid` on the importer's rows and on each p4
    /// command's, plus the repo and its server.
    fn batch_context(&self, p4_server: &str) -> CoreContext {
        let ctx = self.app.new_basic_context();
        ctx.with_mutated_scuba(|mut scuba| {
            scuba
                .add_metadata(ctx.metadata())
                .add("repo", self.repo.repo_identity().name())
                .add("p4_server", p4_server);
            scuba
        })
    }

    async fn wait(&self, delay: Duration) {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = self.wake.notified() => {}
        }
    }

    fn should_stop(&self) -> bool {
        self.cancellation_requested.load(Ordering::Relaxed)
            || exit_file_present(&self.settings.exit_file)
    }
}

#[async_trait]
impl RepoShardedProcessExecutor for P4SyncProcessExecutor {
    async fn execute(&self) -> Result<()> {
        let repo_name = self.repo.repo_identity().name();
        let bookmark = BookmarkKey::new(BOOKMARK)?;
        let options = ImportOptions::default();

        let mut state = None;
        let mut failures = 0;
        let mut batch_size = BatchSize::new(self.settings.batch_size);
        let _clear_stuck = ClearStuckOnDrop {
            fb: self.app.fb,
            repo_name: repo_name.to_owned(),
        };
        while !self.should_stop() {
            let Source { alias, depot, p4 } = match &self.source {
                Ok(source) => source,
                Err(reason) => {
                    log_state(&mut state, State::Held, || {
                        format!("{repo_name}: {reason}, importing nothing")
                    });
                    self.wait(self.settings.idle_interval).await;
                    continue;
                }
            };
            if !import_enabled(repo_name) {
                log_state(&mut state, State::Disabled, || {
                    format!("{repo_name}: {ENABLED_JUST_KNOB} is off, importing nothing")
                });
                self.wait(self.settings.idle_interval).await;
                continue;
            }

            let ctx = self.batch_context(alias);
            let source = P4CliSource::new(p4.with_scuba(ctx.scuba().clone()));
            let batch = p4_import::import_next(
                &ctx,
                &self.repo,
                &source,
                depot,
                &bookmark,
                batch_size.get(),
                &options,
            );
            // A batch can outlast `SM_CLEANUP_TIMEOUT_SECS` (derivation included), so a stop
            // drops it: its bookmark and mapping move in one transaction, all or nothing, and
            // what it saved is reused by the next import.
            let result = tokio::select! {
                result = batch => result,
                _ = self.wake.notified() => {
                    tracing::info!("{repo_name}: stopped mid-batch");
                    break;
                }
            };
            match &result {
                Ok(_) => batch_size.succeeded(),
                Err(error) if p4_import::failed_stage(error) != Some(p4_import::Stage::Prepare) => {
                    batch_size.failed()
                }
                Err(_) => {}
            }
            STATS::stuck.set_value(
                self.app.fb,
                batch_size.stuck() as i64,
                (repo_name.to_owned(),),
            );
            match result {
                Ok(batch) if batch.changelists.is_empty() => {
                    failures = 0;
                    log_state(&mut state, State::CaughtUp, || {
                        format!(
                            "{repo_name} is caught up with {depot} on {alias}; looking again every {:?}",
                            self.settings.idle_interval,
                        )
                    });
                    self.wait(self.settings.idle_interval).await;
                }
                Ok(_) => {
                    failures = 0;
                    log_state(&mut state, State::Importing, || {
                        format!("{repo_name}: importing {depot} from {alias}")
                    });
                }
                // import_next has logged the failure, with its stage and changelist.
                Err(_) => {
                    failures += 1;
                    let delay = retry_delay(failures);
                    if batch_size.stuck() {
                        tracing::error!(
                            "{repo_name}: stuck on the changelist of {depot} after {bookmark}, {failures} failed batches in a row; retrying it alone in {delay:?}"
                        );
                    } else {
                        tracing::warn!(
                            "{repo_name}: {failures} failed batches of {depot} in a row, retrying with {} changelists in {delay:?}",
                            batch_size.get()
                        );
                    }
                    state = None;
                    self.wait(delay).await;
                }
            }
        }

        tracing::info!("p4_sync released shard for repo {repo_name}");
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancellation_requested.store(true, Ordering::Relaxed);
        self.wake.notify_one();
        Ok(())
    }
}

pub(crate) async fn run(
    app: MononokeApp,
    sharded_executor_args: ShardedExecutorArgs,
    settings: Settings,
) -> Result<()> {
    let process = Arc::new(P4SyncProcess {
        app: Arc::new(app),
        settings: Arc::new(settings),
    });

    let Some(executor) = sharded_executor_args.build_executor(
        process.app.fb,
        process.app.runtime().clone(),
        || process.clone(),
        true, // enable shard (repo) level healing
        SM_CLEANUP_TIMEOUT_SECS,
    )?
    else {
        bail!("p4_sync can only run sharded: pass --sharded-service-name and --sharded-scope-name")
    };

    tracing::info!("Running sharded p4_sync loop");
    let (_sender, receiver) = tokio::sync::oneshot::channel::<bool>();
    executor.block_and_execute(receiver).await
}
