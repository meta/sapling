/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::cmp;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::bail;
use anyhow::format_err;
use async_trait::async_trait;
use blobstore::Loadable;
use blobstore::Storable;
use bytes::Bytes;
use clap::Parser;
use clap::ValueEnum;
use context::CoreContext;
use executor_lib::RepoShardedProcess;
use executor_lib::RepoShardedProcessExecutor;
use executor_lib::ShardedProcessExecutor;
use fbinit::FacebookInit;
use filestore::Alias;
use filestore::AliasBlob;
use filestore::Blake3IncrementalHasher;
use filestore::FetchKey;
use filestore::GitSha1IncrementalHasher;
use filestore::Sha1IncrementalHasher;
use filestore::Sha256IncrementalHasher;
use filestore::hash_bytes;
use futures::future::TryFutureExt;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use mercurial_types::FileBytes;
use metaconfig_types::RepoConfig;
use metaconfig_types::ShardedService;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::MononokeReposManager;
use mononoke_app::args::OptRepoArgs;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use mononoke_repos::MononokeRepos;
use mononoke_types::BlobstoreKey;
use mononoke_types::ChangesetId;
use mononoke_types::ContentAlias;
use mononoke_types::ContentId;
use mononoke_types::FileChange;
use mutable_counters::MutableCounters;
use mutable_counters::MutableCountersRef;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_identity::RepoIdentity;
use sharding_ext::RepoShard;
use sql_commit_graph_storage::CommitGraphBulkFetcher;
use sql_commit_graph_storage::CommitGraphBulkFetcherRef;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

const LIMIT: usize = 1000;
const SM_SERVICE_SCOPE: &str = "global";
const SM_CLEANUP_TIMEOUT_SECS: u64 = 120;

#[facet::container]
pub struct Repo {
    #[facet]
    repo_identity: RepoIdentity,
    #[facet]
    repo_blobstore: RepoBlobstore,
    #[facet]
    mutable_counters: dyn MutableCounters,
    #[facet]
    commit_graph_bulk_fetcher: CommitGraphBulkFetcher,
    #[facet]
    repo_config: RepoConfig,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Mode {
    /// Mode to verify if the alias exists, and if it doesn't, report the error
    Verify,
    /// Mode to verify if the alias exists, and if it doesn't then generate it.
    Generate,
    /// Mode to generate aliases (along with metadata) for large collection of files.
    /// Can be used for backfilling repos with metadata and new aliases. In this mode,
    /// min_cs_db_id is ignored and {repo_name}_alias_backfill_counter mutable counter
    /// is used to determine the starting changeset for backfilling. If the mutable counter
    /// doesn't exist, the backfilling starts from cs_id 0.
    Backfill,
}

#[derive(Debug)]
enum RunStatus {
    InProgress,
    Finished,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AliasType {
    Sha256,
    SeededBlake3,
    Sha1,
    GitSha1,
}

impl AliasType {
    fn get_alias(&self, content: &Bytes) -> Alias {
        match self {
            AliasType::GitSha1 => {
                Alias::GitSha1(hash_bytes(GitSha1IncrementalHasher::new(content), content).sha1())
            }
            AliasType::SeededBlake3 => {
                Alias::SeededBlake3(hash_bytes(Blake3IncrementalHasher::new_seeded(), content))
            }
            AliasType::Sha1 => Alias::Sha1(hash_bytes(Sha1IncrementalHasher::new(), content)),
            AliasType::Sha256 => Alias::Sha256(hash_bytes(Sha256IncrementalHasher::new(), content)),
        }
    }
}

/// Struct representing the Alias Verify process.
pub struct AliasVerifyProcess {
    app: MononokeApp,
    args: Arc<AliasVerifyArgs>,
    repos_mgr: MononokeReposManager<Repo>,
}

impl AliasVerifyProcess {
    pub fn new(app: MononokeApp, repos_mgr: MononokeReposManager<Repo>) -> Result<Self> {
        let args: Arc<AliasVerifyArgs> = Arc::new(app.args()?);
        Ok(Self {
            app,
            args,
            repos_mgr,
        })
    }
}

#[async_trait]
impl RepoShardedProcess for AliasVerifyProcess {
    async fn setup(&self, repo: &RepoShard) -> anyhow::Result<Arc<dyn RepoShardedProcessExecutor>> {
        let repo_name = repo.repo_name.as_str();
        let start = repo.chunk_id.unwrap_or(0) as u64;
        let total = repo.total_chunks.unwrap_or(1) as u64;
        info!("Setting up alias verify for repo {}", repo_name);
        let ctx = self.app.new_basic_context();
        self.repos_mgr
            .add_repo(repo_name.as_ref())
            .await
            .with_context(|| format!("Failure in opening repo {repo_name}"))?;
        let db_config = self
            .app
            .repo_config_by_name(repo_name.as_ref())?
            .storage_config
            .metadata;
        let common_config = Arc::new(self.app.repo_configs().common.clone());
        let redacted_blobs = self
            .app
            .repo_factory()
            .redacted_blobs(ctx.clone(), &db_config, &common_config)
            .await?;
        let redacted_content_ids = redacted_blobs.redacted().keys().cloned().collect();
        Ok(Arc::new(AliasVerification::new(
            repo_name.to_string(),
            self.repos_mgr.repos().clone(),
            self.args.clone(),
            Arc::new(AtomicBool::new(false)),
            ctx,
            start,
            total,
            redacted_content_ids,
        )))
    }
}

/// Verify and reload all the alias blobs
#[derive(Parser)]
#[clap(about = "Verify and reload all the alias blobs into Mononoke blobstore.")]
struct AliasVerifyArgs {
    /// The type of alias to verify or generate (in case of missing alias)
    #[clap(long, value_enum, default_value_t = AliasType::Sha256)]
    alias_type: AliasType,
    /// Mode for missing blobs
    #[clap(long, value_enum, default_value_t = Mode::Verify)]
    mode: Mode,
    /// Number of commit ids to process at a time
    #[clap(long, default_value_t = 5000)]
    step: u64,
    /// Changeset to start verification from. Id from changeset table. Not connected to hash
    #[clap(long, default_value_t = 0)]
    min_cs_db_id: u64,
    /// Concurrency limit defining how many commits to be processed in parallel
    #[clap(long, default_value_t = LIMIT)]
    concurrency: usize,
    /// The repo against which the alias verify command needs to be executed
    #[clap(flatten)]
    repo: OptRepoArgs,
    /// The name of ShardManager service to be used when running alias verify in sharded setting.
    #[clap(long, conflicts_with_all = &["repo-name", "repo-id"])]
    pub sharded_service_name: Option<String>,
}

/// Struct representing the Alias Verify process over the context of Repo.
struct AliasVerification {
    repos: Arc<MononokeRepos<Repo>>,
    repo_name: String,
    args: Arc<AliasVerifyArgs>,
    ctx: CoreContext,
    err_cnt: Arc<AtomicUsize>,
    cs_processed: Arc<AtomicUsize>,
    cancellation_requested: Arc<AtomicBool>,
    start: u64,
    total: u64,
    redacted_content_ids: HashSet<String>,
}

#[async_trait]
impl RepoShardedProcessExecutor for AliasVerification {
    async fn execute(&self) -> anyhow::Result<()> {
        info!(
            "Initiating alias verify execution for repo {}",
            &self.repo_name,
        );

        let val = self.verify_all().await;
        match val {
            Err(ref e) => {
                error!(
                    "Alias Verify Failure in repo {}. Terminating execution. Cause: {:?}",
                    &self.repo_name, e
                );
                val
            }
            v => v,
        }
    }

    async fn stop(&self) -> anyhow::Result<()> {
        info!(
            "Terminating alias verify execution for repo {}",
            &self.repo_name,
        );
        self.cancellation_requested.store(true, Ordering::Relaxed);
        Ok(())
    }
}

impl AliasVerification {
    pub fn new(
        repo_name: String,
        repos: Arc<MononokeRepos<Repo>>,
        args: Arc<AliasVerifyArgs>,
        cancellation_requested: Arc<AtomicBool>,
        ctx: CoreContext,
        start: u64,
        total: u64,
        redacted_content_ids: HashSet<String>,
    ) -> Self {
        Self {
            repo_name,
            repos,
            args,
            cancellation_requested,
            ctx,
            start,
            total,
            redacted_content_ids,
            err_cnt: Arc::new(AtomicUsize::new(0)),
            cs_processed: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn repo(&self) -> Result<Arc<Repo>> {
        self.repos
            .get_by_name(self.repo_name.as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Requested repo {} does not exist on this server",
                    self.repo_name
                )
            })
    }

    async fn get_file_changes_vector(&self, bcs_id: ChangesetId) -> Result<Vec<FileChange>, Error> {
        let cs_cnt = self.cs_processed.fetch_add(1, Ordering::Relaxed);
        if cs_cnt.is_multiple_of(1000) {
            info!("Commit processed {:?}", cs_cnt);
        }
        let bcs = bcs_id
            .load(&self.ctx, self.repo()?.repo_blobstore())
            .await
            .with_context(|| {
                format!("Failure in fetching changeset for changeset ID {bcs_id:?}")
            })?;
        let file_changes: Vec<_> = bcs
            .file_changes_map()
            .iter()
            .map(|(_path, fc)| fc.clone())
            .collect();
        Ok(file_changes)
    }

    async fn check_alias_blob(
        &self,
        alias: &Alias,
        expected_content_id: ContentId,
        content_id: ContentId,
    ) -> Result<(), Error> {
        if content_id == expected_content_id {
            // Everything is good
            Ok(())
        } else {
            panic!(
                "Collision: Wrong content_id by alias for {alias:?},
                ContentId in the blobstore {content_id:?},
                Expected ContentId {expected_content_id:?}"
            );
        }
    }

    async fn process_missing_alias_blob(
        &self,
        ctx: &CoreContext,
        alias: &Alias,
        content_id: ContentId,
    ) -> Result<(), Error> {
        self.err_cnt.fetch_add(1, Ordering::Relaxed);
        debug!(
            "Missing alias blob: alias {}, content_id {:?}",
            alias, content_id
        );
        match self.args.mode {
            Mode::Verify => Ok(()),
            Mode::Generate | Mode::Backfill => {
                let blobstore = self.repo()?.repo_blobstore().clone();
                let maybe_meta =
                    filestore::get_metadata(&blobstore, ctx, &FetchKey::Canonical(content_id))
                        .await?;
                let meta = maybe_meta.ok_or_else(|| {
                    format_err!("Missing content {content_id:?} for alias {alias:?}")
                })?;
                let is_valid_match = match *alias {
                    Alias::Sha256(hash_val) => meta.sha256 == hash_val,
                    Alias::GitSha1(hash_val) => meta.git_sha1.sha1() == hash_val,
                    Alias::SeededBlake3(hash_val) => meta.seeded_blake3 == hash_val,
                    Alias::Sha1(hash_val) => meta.sha1 == hash_val,
                };
                if is_valid_match {
                    AliasBlob(alias.clone(), ContentAlias::from_content_id(content_id))
                        .store(ctx, &blobstore)
                        .await
                } else {
                    Err(format_err!(
                        "Inconsistent hashes for {:?}, got {:?}, metadata hashes are (Sha1: {:?}, Sha256: {:?}, GitSha1: {:?}, SeededBlake3: {:?})",
                        content_id,
                        alias,
                        meta.sha1,
                        meta.sha256,
                        meta.git_sha1.sha1(),
                        meta.seeded_blake3,
                    ))
                }
            }
        }
    }

    async fn process_alias(
        &self,
        ctx: &CoreContext,
        alias: &Alias,
        content_id: ContentId,
    ) -> Result<(), Error> {
        let result = FetchKey::from(alias.clone())
            .load(ctx, self.repo()?.repo_blobstore())
            .await;
        match result {
            Ok(content_id_from_blobstore) => {
                self.check_alias_blob(alias, content_id, content_id_from_blobstore)
                    .await
            }
            Err(_) => {
                // the blob with alias is not found
                self.process_missing_alias_blob(ctx, alias, content_id)
                    .await
            }
        }
    }

    pub async fn process_file_content(&self, content_id: ContentId) -> Result<(), Error> {
        let ctx = &self.ctx;
        let alias = filestore::fetch_concat(self.repo()?.repo_blobstore(), ctx, content_id)
            .map_ok(FileBytes)
            .map_ok(|content| self.args.alias_type.get_alias(&content.into_bytes()))
            .await
            .with_context(|| format!("Failure in fetching content at content ID {content_id:?}"))?;
        self.process_alias(ctx, &alias, content_id).await
    }

    fn print_report(&self, status: RunStatus) {
        let resolution = match status {
            RunStatus::InProgress => "continues",
            RunStatus::Finished => "finished",
        };
        info!(
            "Alias Verification {}: {:?} errors found",
            resolution,
            self.err_cnt.load(Ordering::Relaxed)
        );
    }

    fn counter_name(&self) -> String {
        let counter_prefix = if self.start == 0 {
            self.repo_name.to_string()
        } else {
            format!("{}_{}_{}", self.repo_name, self.start, self.total)
        };
        format!("{counter_prefix}_alias_backfill_counter")
    }

    async fn update_overall_progress(&self, completed: usize, max_id: u64) -> Result<()> {
        let repo = self.repo()?;
        info!("Processed {} changesets", completed);
        if let Mode::Backfill = self.args.mode {
            info!("Completed processing till changeset ID {:?}", max_id);
            let counter_name = self.counter_name();
            repo.mutable_counters()
                .set_counter(&self.ctx, &counter_name, max_id as i64, None)
                .await
                .with_context(|| {
                    format!(
                        "Failed to set {} for {} to {}",
                        counter_name, self.repo_name, max_id
                    )
                })?;
            let mut updated = false;
            if self.args.sharded_service_name.is_some() && completed > 0 {
                // Counter to keep track of overall number of commits for the repo.
                let overall_counter_name =
                    format!("overall_{}_alias_backfill_counter", self.repo_name);
                while !updated {
                    let maybe_counter_val = repo
                        .mutable_counters()
                        .get_counter(&self.ctx, &overall_counter_name)
                        .await
                        .with_context(|| {
                            format!("Error while getting mutable counter {overall_counter_name}")
                        })?;
                    let new_counter_val =
                        maybe_counter_val.clone().unwrap_or(0) + (completed as i64);
                    updated = repo
                        .mutable_counters()
                        .set_counter(
                            &self.ctx,
                            &overall_counter_name,
                            new_counter_val,
                            maybe_counter_val,
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to set {} for {} to {}",
                                overall_counter_name, self.repo_name, new_counter_val
                            )
                        })?;
                    info!(
                        "Updated total commits processed for {} to {}",
                        self.repo_name, new_counter_val
                    );
                }
            }
        }
        Ok(())
    }

    async fn get_bounded(&self, bounds: Range<u64>) -> Result<(), Error> {
        if self.cancellation_requested.load(Ordering::Relaxed) {
            return Ok(());
        }
        info!(
            "Process Changesets with ids: [{:?}, {:?})",
            bounds.start, bounds.end,
        );
        let repo = self.repo()?;
        let bcs_ids = repo.commit_graph_bulk_fetcher().oldest_first_stream(
            &self.ctx,
            bounds.clone(),
            500,
            true,
        );
        let count = AtomicUsize::new(0);
        let rcount = &count;
        let file_changes_stream = bcs_ids
            .and_then(move |item| async move {
                let file_changes_vec = self.get_file_changes_vector(item.cs_id).await?;
                Ok(stream::iter(file_changes_vec).map(anyhow::Ok))
            })
            .try_flatten()
            .boxed();

        file_changes_stream
            .try_for_each_concurrent(self.args.concurrency, move |file_change| async move {
                rcount.fetch_add(1, Ordering::Relaxed);
                match file_change.simplify() {
                    Some(tc) => {
                        if self
                            .redacted_content_ids
                            .contains(&tc.content_id().blobstore_key())
                        {
                            warn!(
                                "Skipping content id {:?} since it is part of the redaction list",
                                tc.content_id()
                            );
                            Ok(())
                        } else {
                            self.process_file_content(tc.content_id().clone()).await
                        }
                    }
                    None => Ok(()),
                }
            })
            .await?;
        self.update_overall_progress(rcount.load(Ordering::Relaxed), bounds.end)
            .await?;
        self.print_report(RunStatus::InProgress);
        Ok(())
    }

    async fn adjust_bounds(&self, repo_bounds: Range<u64>) -> Result<Range<u64>> {
        let counter_name = self.counter_name();
        let counter_val = self
            .repo()?
            .mutable_counters()
            .get_counter(&self.ctx, &counter_name)
            .await
            .with_context(|| format!("Error while getting mutable counter {counter_name}"))?;
        // No chunking if no start or end provided.
        if self.start == 0 {
            if self.args.min_cs_db_id != 0 {
                return Ok(self.args.min_cs_db_id..repo_bounds.end);
            } else {
                return Ok(
                    counter_val.unwrap_or(self.args.min_cs_db_id as i64) as u64..repo_bounds.end
                );
            }
        }
        match self.args.mode {
            // Chunking only in backfilling mode
            Mode::Backfill => {
                let factor = (repo_bounds.end - repo_bounds.start) / self.total;
                let prev_chunk_id = std::cmp::max(self.start, 1) - 1;
                let default_start_point = (repo_bounds.start + factor * prev_chunk_id) as i64;
                // If the mutable counter "counter_val" doesn't have a value, then use the
                // default_start_point as the starting point of the chunk. If the counter has
                // a value, then use that unless the value is less than default_start_point.
                let start_point = counter_val.map_or(default_start_point, |counter| {
                    std::cmp::max(counter, default_start_point)
                }) as u64;
                let end_point = repo_bounds.start + factor * self.start;
                Ok(start_point..end_point)
            }
            _ => Ok(self.args.min_cs_db_id..repo_bounds.end),
        }
    }

    pub async fn verify_all(&self) -> Result<(), Error> {
        let (ctx, step, repo) = (&self.ctx, self.args.step, self.repo()?);
        let repo_bounds = repo
            .commit_graph_bulk_fetcher()
            .repo_bounds(ctx, true)
            .await?;
        let cur_bounds = self.adjust_bounds(repo_bounds.clone()).await?;
        let mut bounds = vec![];
        let mut cur_id = cmp::max(repo_bounds.start, cur_bounds.start);
        info!(
            "Initiating aliasverify in {:?} mode with bounds [{}, {}). Starting with changeset with id {}.",
            self.args.mode, cur_bounds.start, cur_bounds.end, cur_id,
        );
        while cur_id < cur_bounds.end {
            let max = cmp::min(cur_bounds.end, cur_id + step);
            bounds.push(cur_id..max);
            cur_id += step;
        }
        stream::iter(bounds)
            .map(Ok)
            .try_for_each(move |chunk_bounds| self.get_bounded(chunk_bounds))
            .await?;
        self.print_report(RunStatus::Finished);
        if self.args.sharded_service_name.is_some() {
            let finished_counter_name = format!("finished_{}", self.counter_name());
            repo.mutable_counters()
                .set_counter(&self.ctx, &finished_counter_name, 0, None)
                .await
                .with_context(|| {
                    format!(
                        "Failed to set {} for {}",
                        finished_counter_name, self.repo_name
                    )
                })?;
        }
        Ok(())
    }
}

async fn async_main(app: MononokeApp) -> Result<(), Error> {
    let args: AliasVerifyArgs = app.args()?;
    let repo_mgr = app
        .open_managed_repos(Some(ShardedService::AliasVerify))
        .await?;
    let process = AliasVerifyProcess::new(app, repo_mgr)?;
    match args.sharded_service_name {
        None => {
            let maybe_repo_arg = args.repo.as_repo_arg();
            let (repo_name, _) = match maybe_repo_arg {
                Some(repo_arg) => process.app.repo_config(repo_arg)?,
                None => bail!(
                    "Repo name or ID not provided. Either sharded-service-name or repo id/name should be provided."
                ),
            };
            let alias_verify = process
                .setup(&RepoShard::with_repo_name(repo_name.as_ref()))
                .await?;
            alias_verify.execute().await
        }
        Some(name) => {
            // The service name needs to be 'static to satisfy SM contract
            static SM_SERVICE_NAME: OnceLock<String> = OnceLock::new();
            let executor = ShardedProcessExecutor::new(
                process.app.fb,
                process.app.runtime().clone(),
                SM_SERVICE_NAME.get_or_init(|| name),
                SM_SERVICE_SCOPE,
                SM_CLEANUP_TIMEOUT_SECS,
                Arc::new(process),
                true, // enable shard (repo) level healing
                None,
            )?;
            let (sender, receiver) = tokio::sync::oneshot::channel::<bool>();
            executor.block_and_execute(receiver).await?;
            drop(sender);
            Ok(())
        }
    }
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    let app = MononokeAppBuilder::new(fb)
        .with_app_extension(MonitoringAppExtension {})
        .build::<AliasVerifyArgs>()?;
    app.run_with_monitoring_and_logging(async_main, "aliasverify", AliveService)
}
