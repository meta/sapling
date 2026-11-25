/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
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
use metaconfig_types::P4ServerConfig;
use metaconfig_types::RepoConfig;
use metaconfig_types::RepoConfigRef;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArgs;
use p4_import::ImportOptions;
use p4_import::SkipUnreadable;
use p4_import::UnreadableRevision;
use p4_source::FilelogOptions;
use p4_source::Keychain;
use p4_source::P4Cli;
use p4_source::P4CliSource;
use p4_source::PathCharsets;
use p4_source::PathCharsetsError;
use repo_identity::RepoIdentityRef;
use sharding_ext::RepoShard;
use stats::define_stats;
use stats::prelude::DynamicSingletonCounter;
use stats_facebook::service_data::ServiceData;
use stats_facebook::service_data::get_service_data_singleton;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::Repo;
use crate::server;

const SM_CLEANUP_TIMEOUT_SECS: u64 = 120;
const BOOKMARK: &str = "master";
/// Off, or not on for the repo (switch = repo name): the job holds the shard and imports
/// nothing.
const ENABLED_JUST_KNOB: &str = "scm/mononoke:p4_sync_enabled";
/// Well under `SM_CLEANUP_TIMEOUT_SECS`, so a stop or the exit file is seen in time.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
/// Failed batches of a single changelist in a row before the repo counts as stuck.
const STUCK_AFTER: u32 = 3;
const FRESHNESS_INTERVAL: Duration = Duration::from_secs(30);

define_stats! {
    prefix = "mononoke.p4_sync";
    // 1 while the repo is stuck on a changelist it can't import, for alerts.
    stuck: dynamic_singleton_counter("stuck.{}", (repo: String)),
}

/// The repo's gauge of the seconds since its last progress (see `LastProgress`), set every
/// `FRESHNESS_INTERVAL` while the shard runs and removed when it stops. Not in
/// `define_stats!`, whose key must be a literal, so that setting and removing it share this
/// one name.
pub(crate) fn secs_since_success_key(repo_name: &str) -> String {
    format!("mononoke.p4_sync.secs_since_success.{repo_name}")
}

pub(crate) struct Settings {
    pub(crate) exit_file: PathBuf,
    /// The `--p4-server` clients, by alias: each wins over the tier's `p4_servers` entry.
    pub(crate) servers: HashMap<String, P4Cli>,
    pub(crate) p4_binary: PathBuf,
    /// The revisions imported without content if their server cannot read them, by alias.
    pub(crate) unreadable_revisions: HashMap<String, Arc<HashSet<UnreadableRevision>>>,
    /// The servers, by alias, whose other unreadable revisions are skipped after retries.
    pub(crate) skip_unreadable_servers: HashSet<String>,
    /// How those servers' revisions are retried.
    pub(crate) skip_unreadable: SkipUnreadable,
    /// How integration records are asked for.
    pub(crate) filelog: FilelogOptions,
    pub(crate) p4_connect_retries: u32,
    pub(crate) batch_size: NonZeroUsize,
    pub(crate) idle_interval: Duration,
    pub(crate) import_options: ImportOptions,
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

/// When the shard last made progress: a successful pass (imported a batch, found nothing
/// new, or was switched off by JK on purpose), or a step of the running batch (see
/// `ImportOptions::progress`). Starts at the shard's start, so a shard that never progresses,
/// held ones included, reports a growing value.
pub(crate) struct LastProgress {
    start: Instant,
    secs_after_start: AtomicU64,
    /// The running batch's steps, counted by p4_import.
    steps: Arc<AtomicU64>,
    /// The most steps of any batch since the last success. A retry repeats the failed
    /// attempts' steps, so only steps past them count: a batch that keeps failing at the
    /// same point still shows as a growing value.
    furthest_steps: AtomicU64,
}

impl LastProgress {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            start: now,
            secs_after_start: AtomicU64::new(0),
            steps: Arc::default(),
            furthest_steps: AtomicU64::new(0),
        }
    }

    /// For `ImportOptions::progress`.
    pub(crate) fn steps(&self) -> Arc<AtomicU64> {
        self.steps.clone()
    }

    pub(crate) fn succeeded(&self, now: Instant) {
        self.record(now);
        self.steps.store(0, Ordering::Relaxed);
        self.furthest_steps.store(0, Ordering::Relaxed);
    }

    pub(crate) fn failed(&self, now: Instant) {
        self.note_steps(now);
        self.steps.store(0, Ordering::Relaxed);
    }

    /// Records progress if the running batch has gone further than any since the last
    /// success.
    pub(crate) fn note_steps(&self, now: Instant) {
        let steps = self.steps.load(Ordering::Relaxed);
        if steps > self.furthest_steps.fetch_max(steps, Ordering::Relaxed) {
            self.record(now);
        }
    }

    fn record(&self, now: Instant) {
        self.secs_after_start
            .store(self.secs_to(now), Ordering::Relaxed);
    }

    pub(crate) fn secs_since(&self, now: Instant) -> u64 {
        self.secs_to(now)
            .saturating_sub(self.secs_after_start.load(Ordering::Relaxed))
    }

    fn secs_to(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.start).as_secs()
    }
}

/// Runs `work`, setting `secs_since_success` every `FRESHNESS_INTERVAL` beside it, so a
/// batch that never returns and makes no progress shows as a growing value. Removes the
/// counter when `work` ends or is dropped: a stale value would read as fresh on a task that
/// no longer has the shard.
pub(crate) async fn report_freshness<T>(
    fb: FacebookInit,
    repo_name: &str,
    last_progress: &LastProgress,
    work: impl Future<Output = T>,
) -> T {
    let gauge = RemoveFreshnessOnDrop {
        fb,
        key: secs_since_success_key(repo_name),
    };
    let mut ticker = tokio::time::interval(FRESHNESS_INTERVAL);
    tokio::pin!(work);
    loop {
        tokio::select! {
            output = &mut work => return output,
            _ = ticker.tick() => {
                let now = Instant::now();
                last_progress.note_steps(now);
                get_service_data_singleton(fb)
                    .set_counter(&gauge.key, last_progress.secs_since(now) as i64);
            }
        }
    }
}

struct RemoveFreshnessOnDrop {
    fb: FacebookInit,
    key: String,
}

impl Drop for RemoveFreshnessOnDrop {
    fn drop(&mut self) {
        get_service_data_singleton(self.fb).clear_counter(&self.key);
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

/// Only `Depot` imports; the others hold the shard. `P` is the server entry, a [`Server`] in
/// the job.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConfiguredDepot<'a, P> {
    NotAMirror,
    /// `p4_source.server_alias` is neither in the tier's `p4_servers` nor a `--p4-server`.
    UnknownServer {
        configured: String,
    },
    /// `p4_source.path_charsets` is empty or names a charset encoding_rs doesn't know.
    InvalidCharsets(PathCharsetsError),
    Depot {
        alias: &'a str,
        depot: String,
        path_charsets: PathCharsets,
        p4: &'a P,
    },
}

/// A server of [`servers`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Server<'a, F> {
    /// An entry of the tier's `p4_servers`; its client is built at shard setup, so a running
    /// shard keeps it until the task restarts or the shard moves, whatever configerator says.
    Tier(&'a P4ServerConfig),
    /// A `--p4-server`'s client.
    Flag(&'a F),
}

/// Every alias's server: its `--p4-server` while the spec gives one, else its entry in the
/// tier's `p4_servers`. Dropping an alias's flag from the spec moves that alias to the map;
/// putting the flag back rolls it back.
pub(crate) fn servers<'a, F>(
    tier: &'a BTreeMap<String, P4ServerConfig>,
    flags: &'a HashMap<String, F>,
) -> HashMap<String, Server<'a, F>> {
    tier.iter()
        .map(|(alias, server)| (alias.clone(), Server::Tier(server)))
        .chain(
            flags
                .iter()
                .map(|(alias, p4)| (alias.clone(), Server::Flag(p4))),
        )
        .collect()
}

pub(crate) fn configured_depot<'a, P>(
    config: &RepoConfig,
    servers: &'a HashMap<String, P>,
) -> ConfiguredDepot<'a, P> {
    let Some(p4_source) = &config.p4_source else {
        return ConfiguredDepot::NotAMirror;
    };
    let path_charsets = match p4_source.path_charsets.as_deref().map(PathCharsets::new) {
        None => PathCharsets::default(),
        Some(Ok(path_charsets)) => path_charsets,
        Some(Err(err)) => return ConfiguredDepot::InvalidCharsets(err),
    };
    match servers.get_key_value(&p4_source.server_alias) {
        None => ConfiguredDepot::UnknownServer {
            configured: p4_source.server_alias.clone(),
        },
        Some((alias, p4)) => ConfiguredDepot::Depot {
            alias,
            depot: p4_source.depot.clone(),
            path_charsets,
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
    /// Its server's `--unreadable-revision`s in its depot only.
    unreadable_revisions: Arc<HashSet<UnreadableRevision>>,
    /// Set if its server is one of the `--skip-unreadable-revisions`.
    skip_unreadable: Option<SkipUnreadable>,
}

/// Those of a server's `--unreadable-revision`s that are under `depot_root`: another depot's
/// do not concern this repo's import.
pub(crate) fn unreadable_in_depot(
    of_server: Option<&Arc<HashSet<UnreadableRevision>>>,
    depot_root: &str,
) -> Arc<HashSet<UnreadableRevision>> {
    let in_depot = of_server
        .into_iter()
        .flat_map(|revisions| revisions.iter())
        .filter(|revision| revision.depot_path.starts_with(depot_root))
        .cloned()
        .collect();
    Arc::new(in_depot)
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
        let repo_configs = self.app.repo_configs();
        let servers = servers(&repo_configs.common.p4_servers, &self.settings.servers);
        let source = match configured_depot(repo.repo_config(), &servers) {
            ConfiguredDepot::Depot {
                alias,
                depot,
                path_charsets,
                p4,
            } => {
                let (from, p4) = match p4 {
                    Server::Tier(server) => {
                        let p4 = server::client(
                            server,
                            &self.settings.p4_binary,
                            Arc::new(Keychain(self.app.fb)),
                            self.app.environment().scuba_sample_builder.clone(),
                        )
                        .await
                        .with_context(|| format!("{}: p4_servers[{alias}]", shard.repo_name))?;
                        ("its p4_servers entry", p4)
                    }
                    Server::Flag(p4) => ("its --p4-server", P4Cli::clone(p4)),
                };
                tracing::info!(
                    "{}: server {alias} from {from}, at {}",
                    shard.repo_name,
                    p4.port()
                );
                Ok(Source {
                    unreadable_revisions: unreadable_in_depot(
                        self.settings.unreadable_revisions.get(alias),
                        &depot,
                    ),
                    skip_unreadable: self
                        .settings
                        .skip_unreadable_servers
                        .contains(alias)
                        .then_some(self.settings.skip_unreadable),
                    alias: alias.to_owned(),
                    depot,
                    p4: p4
                        .with_path_charsets(path_charsets)
                        .with_connect_retries(self.settings.p4_connect_retries),
                })
            }
            ConfiguredDepot::NotAMirror => Err("no p4_source in its repo config".to_owned()),
            ConfiguredDepot::UnknownServer { configured } => Err(format!(
                "its p4_source.server_alias is {configured}, which is in neither the tier's p4_servers nor a --p4-server"
            )),
            ConfiguredDepot::InvalidCharsets(err) => {
                Err(format!("its p4_source.path_charsets: {err}"))
            }
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
        let last_progress = LastProgress::new(Instant::now());
        report_freshness(
            self.app.fb,
            repo_name,
            &last_progress,
            self.import_loop(repo_name, &last_progress),
        )
        .await
    }

    async fn stop(&self) -> Result<()> {
        self.cancellation_requested.store(true, Ordering::Relaxed);
        self.wake.notify_one();
        Ok(())
    }
}

impl P4SyncProcessExecutor {
    async fn import_loop(&self, repo_name: &str, last_progress: &LastProgress) -> Result<()> {
        let bookmark = BookmarkKey::new(BOOKMARK)?;
        let options = ImportOptions {
            progress: last_progress.steps(),
            unreadable_revisions: self
                .source
                .as_ref()
                .map(|source| source.unreadable_revisions.clone())
                .unwrap_or_default(),
            skip_unreadable: self
                .source
                .as_ref()
                .ok()
                .and_then(|source| source.skip_unreadable),
            ..self.settings.import_options.clone()
        };
        // A caught-up depot has little to list, and windows would only add listings.
        let caught_up_options = ImportOptions {
            listing_window: None,
            ..options.clone()
        };

        let mut state = None;
        let mut failures = 0;
        let mut batch_size = BatchSize::new(self.settings.batch_size);
        let _clear_stuck = ClearStuckOnDrop {
            fb: self.app.fb,
            repo_name: repo_name.to_owned(),
        };
        while !self.should_stop() {
            let Source {
                alias, depot, p4, ..
            } = match &self.source {
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
                last_progress.succeeded(Instant::now());
                log_state(&mut state, State::Disabled, || {
                    format!("{repo_name}: {ENABLED_JUST_KNOB} is off, importing nothing")
                });
                self.wait(self.settings.idle_interval).await;
                continue;
            }

            let ctx = self.batch_context(alias);
            let source = P4CliSource::new(p4.with_scuba(ctx.scuba().clone()))
                .with_filelog_options(self.settings.filelog);
            let batch = p4_import::import_next(
                &ctx,
                &self.repo,
                &source,
                depot,
                &bookmark,
                batch_size.get(),
                match state {
                    Some(State::CaughtUp) => &caught_up_options,
                    _ => &options,
                },
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
                Ok(_) => {
                    last_progress.succeeded(Instant::now());
                    batch_size.succeeded()
                }
                // The changelists before the failing one are committed, so it is already
                // first in the next batch: halving would only shrink the batches after it.
                Err(error) if p4_import::committed_before_failure(error) => {
                    last_progress.failed(Instant::now())
                }
                Err(error) if p4_import::failed_stage(error) != Some(p4_import::Stage::Prepare) => {
                    last_progress.failed(Instant::now());
                    batch_size.failed()
                }
                Err(_) => last_progress.failed(Instant::now()),
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
