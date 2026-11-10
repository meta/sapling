/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use fbinit::FacebookInit;
use futures::future::Either;
use futures::future::FutureExt;
use futures::future::select;
use futures::stream;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use sharding_ext::RepoShard;
use shardmanager_lib::smtypes;
use shardmanager_lib::{self as sm};
use stats::prelude::*;
use tokio::runtime::Handle;
use tokio::sync::RwLock;
use tokio::sync::oneshot::Receiver;
use tokio::task::JoinHandle;
use tokio::time;
use tracing::error;
use tracing::info;

use crate::RepoProcess::*;
use crate::RepoShardedProcess;
use crate::RepoShardedProcessExecutor;
use crate::RepoState::*;

const MAX_SM_CLIENT_INIT_RETRIES: i32 = 10;
const SM_CLIENT_INIT_RETRY_SECS: i32 = 10;

define_stats! {
    prefix = "mononoke.shardmanager";
    restored_connection_to_shardmanager: timeseries(Rate, Sum),
    lost_connection_to_shardmanager: timeseries(Rate, Sum),
    shard_setup_failures: timeseries(Rate, Sum),
    // Steady-state on_add_shard load failures (parse, setup, or cleanup-then-add),
    // distinct from the best-effort bulk set_shards path above.
    on_add_shard_failures: timeseries(Rate, Sum),
    concurrent_shard_setups: singleton_counter(),
}

/// Whether an on_add_shard outcome is a load failure (as opposed to a success
/// or an in-progress step). Pure so the failure-counting is unit-testable.
fn add_shard_outcome_is_failure(state: &RepoState) -> bool {
    matches!(state, Failed(_))
}

/// Enum representing the states in which the repo-add
/// or repo-drop execution can exist.
pub(crate) enum RepoState {
    /// Repo load or unload has completed successfully.
    Completed(String),
    /// Repo load or unload is still in progress.
    InProgress(String),
    /// Repo load or unload has failed.
    Failed(String),
}

/// Enum representing the states corresponding to the
/// lifecycle events of a repo executing process.
pub(crate) enum RepoProcess {
    /// The repo is currently being setup for processing.
    Setup(RepoSetupProcess),
    /// The repo is currently executing over the target process.
    Execution(RepoExecutionProcess),
    /// The repo is currently terminating execution and
    /// cleaning up its processing state.
    Cleanup(RepoCleanupProcess),
}

impl RepoProcess {
    /// Method returning a reference to the shard contained
    /// within this RepoProcess.
    fn shard(&self) -> &smtypes::Shard {
        match self {
            Setup(repo_setup_process) => &repo_setup_process.shard,
            Execution(repo_execution_process) => &repo_execution_process.shard,
            Cleanup(repo_cleanup_process) => &repo_cleanup_process.shard,
        }
    }

    /// Method returning a reference to the parsed repo
    /// shard contained within this RepoProcess.
    fn repo_shard(&self) -> &RepoShard {
        match self {
            Setup(repo_setup_process) => &repo_setup_process.repo_shard,
            Execution(repo_execution_process) => &repo_execution_process.repo_shard,
            Cleanup(repo_cleanup_process) => &repo_cleanup_process.repo_shard,
        }
    }
}

struct ConcurrentSetupGuard {
    fb: FacebookInit,
}

impl ConcurrentSetupGuard {
    fn new(fb: FacebookInit) -> Self {
        STATS::concurrent_shard_setups.increment_value(fb, 1);
        Self { fb }
    }
}

impl Drop for ConcurrentSetupGuard {
    fn drop(&mut self) {
        STATS::concurrent_shard_setups.increment_value(self.fb, -1);
    }
}

/// Struct representing the setup of the underlying process
/// over a specific repo and the shard associated with it.
pub(crate) struct RepoSetupProcess {
    shard: smtypes::Shard,
    repo_shard: RepoShard,
    setup_handle: JoinHandle<Result<Arc<dyn RepoShardedProcessExecutor>>>,
}

impl RepoSetupProcess {
    /// Initiate the setup process with necessary signals to identify setup completion.
    fn new(
        fb: FacebookInit,
        shard: smtypes::Shard,
        repo_shard: RepoShard,
        setup_job: Arc<dyn RepoShardedProcess>,
        runtime_handle: &Handle,
    ) -> Self {
        Self {
            setup_handle: runtime_handle.spawn({
                let repo_shard = repo_shard.clone();
                async move {
                    // Create guard that will decrement counter on drop
                    let _guard = ConcurrentSetupGuard::new(fb);

                    setup_job.setup(&repo_shard).await
                }
            }),
            shard,
            repo_shard,
        }
    }

    /// Method that yields the RepoExecutionProcess generated on the completion
    /// of setup iff the setup has already completed. Serves as both: indication
    /// of completion and provider of resultant RepoExecutionProcess.
    fn try_execution_process(
        &mut self,
        runtime_handle: &Handle,
    ) -> Result<Option<RepoExecutionProcess>> {
        // Check if the setup process has completed.
        if self.setup_handle.is_finished() {
            // The setup has completed, the execution process can be generated.
            let executor = runtime_handle
                .block_on(&mut self.setup_handle)
                .with_context(|| {
                    format!(
                        "Failed to execute setup for shard {} due to Tokio JoinError",
                        self.repo_shard
                    )
                })?
                .with_context(|| format!("Error during setup for shard {}", self.repo_shard))?;
            Ok(Some(RepoExecutionProcess::new(
                self.shard.clone(),
                self.repo_shard.clone(),
                executor,
                runtime_handle,
            )))
        } else {
            // The setup is still underway
            Ok(None)
        }
    }

    /// Method that yields the RepoExecutionProcess generated on the completion
    /// of setup in an async setting. This method consumes self since post executor
    /// generation, the RepoSetupProcess should not be reused. This method can be
    /// called regardless of the completion status of the setup process.
    async fn execution_process(self, runtime_handle: &Handle) -> Result<RepoExecutionProcess> {
        let executor = self
            .setup_handle
            .await
            .with_context(|| {
                format!(
                    "Failed to execute setup for shard {} due to Tokio JoinError",
                    self.repo_shard
                )
            })?
            .with_context(|| format!("Error during setup for shard {}", self.repo_shard))?;
        Ok(RepoExecutionProcess::new(
            self.shard,
            self.repo_shard,
            executor,
            runtime_handle,
        ))
    }
}

/// Struct representing the execution of the underlying process
/// over a specific repo and the shard associated with it.
pub(crate) struct RepoExecutionProcess {
    shard: smtypes::Shard,
    repo_shard: RepoShard,
    executor_job: Arc<dyn RepoShardedProcessExecutor>,
    execution_handle: JoinHandle<Result<()>>,
}

impl RepoExecutionProcess {
    /// Initiates the execution of the underlying process with the
    /// associated context of the provided repo.
    fn new(
        shard: smtypes::Shard,
        repo_shard: RepoShard,
        executor_job: Arc<dyn RepoShardedProcessExecutor>,
        runtime_handle: &Handle,
    ) -> Self {
        Self {
            execution_handle: runtime_handle.spawn({
                let executor_job = Arc::clone(&executor_job);
                async move { executor_job.execute().await }
            }),
            shard,
            repo_shard,
            executor_job,
        }
    }

    /// Performs a destructive close over this repo executing process.
    /// Cannot be used again.
    async fn close(self, timeout_secs: u64) -> Result<()> {
        info!("Terminating execution of shard {}.", self.repo_shard);
        // Invoke the stop() callback allowing the process code to signal its
        // execution unit to prepare for termination via AtomicBool, OneShot
        // etc. This method is supposed to be short-lived. Use a timeout in case
        // it runs for longer than expected period.
        match time::timeout(
            time::Duration::from_secs(timeout_secs),
            self.executor_job.stop(),
        )
        .await
        {
            Err(_) => error!(
                "Timeout while executing 'stop' method for shard {}",
                self.repo_shard
            ),
            Ok(val) => match val {
                Ok(_) => info!(
                    "Successfully executed 'stop' method for shard {}",
                    self.repo_shard
                ),
                Err(e) => error!(
                    "Error terminating shard {} due to failure in 'stop' method. Error: {:#}",
                    self.repo_shard, e
                ),
            },
        };
        // Post return from stop() callback, wait for timeout_secs before
        // scrapping the underlying process execution for the given repo.
        match select(
            self.execution_handle,
            time::sleep(time::Duration::from_secs(timeout_secs)).boxed(),
        )
        .await
        {
            // The repo execution completed before the timeout, exit cleanly.
            Either::Left((_, _)) => {
                info!(
                    "Shard {} execution was terminated gracefully",
                    self.repo_shard
                )
            }
            // The repo execution did not complete
            Either::Right((_, execution_handle)) => {
                // Forcefully terminate the execution post the timeout period.
                execution_handle.abort();
                match execution_handle.await {
                    Err(e) => {
                        if e.is_cancelled() {
                            error!(
                                "Shard {} execution was terminated forcefully",
                                self.repo_shard
                            )
                        } else {
                            error!(
                                "Shard {} execution was terminated due to unexpected error {:#}",
                                self.repo_shard, e
                            )
                        }
                    }
                    Ok(..) => info!("Shard {} execution was aborted", self.repo_shard),
                }
            }
        };
        Ok(())
    }

    /// Checks if the executing process has been terminated either
    /// due to error or successful completion.
    fn is_terminated(&mut self, runtime_handle: &Handle) -> Result<bool> {
        // Check if the underlying task for the execution handle is finished.
        if self.execution_handle.is_finished() {
            // Safe to block on the handle since the underlying task has finished execution.
            match runtime_handle.block_on(&mut self.execution_handle) {
                Ok(Ok(_)) => info!(
                    "Shard {} execution was completed without failure",
                    self.repo_shard
                ),
                Ok(Err(e)) => error!(
                    "Shard {} execution was terminated due to process error {:#}",
                    self.repo_shard, e
                ),
                Err(e) => error!(
                    "Shard {} execution was terminated due to error during task completion {:#}",
                    self.repo_shard, e
                ),
            }
            Ok(true)
        } else {
            // The task is still running.
            info!("Shard {} is still executing", self.repo_shard);
            Ok(false)
        }
    }
}

/// Struct representing the cleanup activity associated with the underlying
/// Process over a specific repo and the shard associated with it.
pub(crate) struct RepoCleanupProcess {
    shard: smtypes::Shard,
    repo_shard: RepoShard,
    repo_cleanup_handle: JoinHandle<Result<()>>,
}

impl RepoCleanupProcess {
    /// Create a new repo cleanup process and initiate the clean
    /// up activity for the given repo execution.
    fn new(executor: RepoExecutionProcess, timeout_secs: u64, runtime_handle: &Handle) -> Self {
        Self {
            shard: executor.shard.clone(),
            repo_shard: executor.repo_shard.clone(),
            repo_cleanup_handle: runtime_handle
                .spawn({ async move { executor.close(timeout_secs).await } }),
        }
    }

    /// Validate if the cleanup process is completed and if yes then
    /// terminate the process handle.
    fn try_close(&mut self, runtime_handle: &Handle) -> Result<Option<()>> {
        // Check if the underlying task of the cleanup process handle has completed.
        if self.repo_cleanup_handle.is_finished() {
            // Safe to block on the task handle since its already completed.
            let result = runtime_handle
                .block_on(&mut self.repo_cleanup_handle)
                .with_context(|| {
                    format!(
                        "Failed to execute cleanup for shard {} due Tokio JoinError",
                        self.repo_shard
                    )
                })?
                .with_context(|| format!("Error during cleanup for shard {}", self.repo_shard))?;
            Ok(Some(result))
        } else {
            // Cleanup is still underway.
            Ok(None)
        }
    }

    /// Wait for the cleanup process to finish and then terminate the
    /// process handle.
    async fn close(self) -> Result<()> {
        self.repo_cleanup_handle
            .await
            .with_context(|| {
                format!(
                    "Failed to execute cleanup for shard {} due to Tokio JoinError",
                    self.repo_shard
                )
            })
            .with_context(|| format!("Error during cleanup for shard {}", self.repo_shard))?
    }
}

pub struct ShardedProcessHandler {
    /// FacebookInit handle for stats reporting.
    fb: FacebookInit,
    /// The name of the *Shard_Manager* job that was created for the current process.
    /// This value should be the same as the service_name value provided in the
    /// ShardManager spec.
    service_name: &'static str,
    /// The handle to the underlying process setup job responsible
    /// for providing the corresponding executor job post setup for given repo.
    setup_job: Arc<dyn RepoShardedProcess>,
    /// A handle to the tokio runtime on which the process executes.
    runtime_handle: Handle,
    /// Health tracker for the current process when talking to ShardManager.
    healthy: AtomicBool,
    /// Optional custom health check function. When set, health_check() returns
    /// the AND of `healthy` and this function's result.
    health_check_fn: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Thread-safe map between the repository currently being
    /// setup / executed / cleaned-up for the underlying process and the
    /// corresponding tokio handle for that execution.
    repo_map: RwLock<HashMap<RepoShard, RepoProcess>>,
    /// The timeout period in seconds for which the executor will allow the
    /// underlying process to perform the necessary clean-up before exiting
    /// forcefully.
    timeout_secs: u64,
    /// Flag determining whether shard-level healing is enabled for the service.
    shard_healing: bool,
}

impl ShardedProcessHandler {
    pub fn new(
        fb: FacebookInit,
        service_name: &'static str,
        runtime_handle: Handle,
        timeout_secs: u64,
        setup_job: Arc<dyn RepoShardedProcess>,
        shard_healing: bool,
        health_check_fn: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Self {
        Self {
            fb,
            service_name,
            runtime_handle,
            setup_job,
            timeout_secs,
            shard_healing,
            healthy: AtomicBool::new(true),
            health_check_fn,
            repo_map: RwLock::new(HashMap::new()),
        }
    }
}

impl ShardedProcessHandler {
    /// Method that sets up repos corresponding to the provided set of shards. If best_effort_setup is set to true,
    /// errors while setting up individual errors will be ignored
    pub async fn set_shards(
        &self,
        shards: Vec<smtypes::Shard>,
        best_effort_setup: bool,
    ) -> Result<()> {
        let input_shard_count = shards.len();
        // Create map from Shard Domain (i.e. Repo Name) to Shard.
        let new_repo_shards: HashMap<RepoShard, smtypes::Shard> = shards
            .into_iter()
            .map(|shard| Ok((RepoShard::from_shard_id(&shard.id.domain)?, shard)))
            .collect::<Result<_>>()?;
        // NOTE: set_shards perform repo-allocation and repo-removal in
        // a similar fashion to add_shard & drop_shard. The repetition of
        // logic is limited just to the HashMap and its associated RWLock
        // because of difference in scoped vs one-time write to the map.
        // The core repo allocation and removal logic is still contained
        // in a single place within Repo Setup, Executor & Cleanup process.

        info!("Setting up {} shards", input_shard_count);

        // Ensure we are the only ones updating the repos.
        let mut guarded_repo_map = self.repo_map.write().await;
        {
            // Clean-up the repos that are no longer assigned to this Process.
            stream::iter(
                guarded_repo_map.extract_if(|old_repo, _| !new_repo_shards.contains_key(old_repo)),
            )
            .map(anyhow::Ok)
            .try_for_each_concurrent(20, move |(old_repo_name, old_repo_process)| async move {
                match old_repo_process {
                    // A repo that was being setup is no longer assigned to this replica.
                    // Drop the setup process associated with the repo.
                    Setup(old_repo_setup_process) => {
                        info!("Cancelling previous repo setup for shard {old_repo_name}");
                        old_repo_setup_process.setup_handle.abort();
                        match old_repo_setup_process.setup_handle.await {
                            Err(e) if e.is_cancelled() => {
                                info!("Previous repo setup for shard {old_repo_name} was cancelled");
                            }
                            result => {
                                result.with_context(|| {
                                    format!(
                                        "Failed to cancel setup for shard {old_repo_name} due to Tokio JoinError"
                                    )
                                })?.with_context(|| {
                                    format!("Error during cancelled setup for shard {old_repo_name}")
                                })?;
                            }
                        };
                    }
                    // A repo that was being executed for target process is no longer assigned
                    // to this replica. Terminate the execution.
                    // NOTE: The termination happens without cleanup process since the
                    // termination is not time-bounded and can be awaited.
                    Execution(old_repo_execution_process) => {
                        old_repo_execution_process
                            .close(self.timeout_secs)
                            .await?;
                    }
                    // A repo that was being cleaned up post execution is no longer assigned
                    // to this replice. Finish the cleanup.
                    Cleanup(old_repo_cleanup_process) => {
                        old_repo_cleanup_process.close().await.with_context(|| {
                            format!("Failed to execute cleanup for shard {old_repo_name}")
                        })?;
                    }
                }
                Ok(())
            })
            .await?;
            // Count of the actual number of shards setup
            let mut shard_setup_count = 0;
            // Assign new repos to this Process based on the incoming Shards.
            let mut setups = FuturesUnordered::new();
            for (new_repo, new_shard) in new_repo_shards {
                while setups.len() >= 20 {
                    let setup_result = match setups.try_next().await {
                        Ok(setup) => setup,
                        Err(e) if best_effort_setup => {
                            error!(
                                "Failure in setting up shard/repo so skipping it. Error: {:?}",
                                e
                            );
                            STATS::shard_setup_failures.add_value(1);
                            continue;
                        }
                        err => err?,
                    };
                    // Limit the number of concurrent setups.
                    let (new_repo, execution_process) =
                        setup_result.ok_or_else(|| anyhow!("Unexpected empty setup"))?;
                    guarded_repo_map.insert(new_repo, RepoProcess::Execution(execution_process));
                    shard_setup_count += 1;
                }
                if !guarded_repo_map.contains_key(&new_repo) {
                    let setup_process = RepoSetupProcess::new(
                        self.fb,
                        new_shard,
                        new_repo.clone(),
                        Arc::clone(&self.setup_job),
                        &self.runtime_handle,
                    );
                    let setup = async move {
                        let execution_process = setup_process
                            .execution_process(&self.runtime_handle)
                            .await?;
                        anyhow::Ok((new_repo, execution_process))
                    };
                    setups.push(setup.boxed());
                } else {
                    // If the repo was already present in the map, then it means it was either being setup
                    // or already executing. Either case, it counts as a successful setup.
                    shard_setup_count += 1;
                }
            }
            while let Some(setup_result) = setups.next().await {
                match setup_result {
                    Ok((new_repo, execution_process)) => {
                        guarded_repo_map
                            .insert(new_repo, RepoProcess::Execution(execution_process));
                        shard_setup_count += 1;
                    }
                    Err(e) if best_effort_setup => {
                        error!(
                            "Failure in setting up shard/repo so skipping it. Error: {:?}",
                            e
                        );
                        STATS::shard_setup_failures.add_value(1);
                        continue;
                    }
                    Err(e) => anyhow::bail!("Error while setting up shard: {e:?}"),
                }
            }
            if shard_setup_count == 0 && input_shard_count > 0 {
                anyhow::bail!("Failed to setup any shards during initial setup");
            }
            info!("Completed setup for {} shards", shard_setup_count);

            Ok(())
        }
    }

    /// Called upon initiating graceful hand-off of primary replicas (in).
    fn on_prepare_add_shard(&self, key: &RepoShard, shard: &smtypes::Shard) {
        info!("Preparing to add shard at key '{}': {:#?}", key, shard);
        // Include additional book-keeping activities (if applicable)
        // before the repo gets assigned.
    }

    /// Called upon moving a shard (in). Counts steady-state load failures
    /// (parse, setup, or cleanup-then-add) via the single exit point below.
    fn on_add_shard(&self, key: &str, shard: smtypes::Shard) -> RepoState {
        let outcome = self.on_add_shard_inner(key, shard);
        if add_shard_outcome_is_failure(&outcome) {
            STATS::on_add_shard_failures.add_value(1);
        }
        outcome
    }

    fn on_add_shard_inner(&self, key: &str, shard: smtypes::Shard) -> RepoState {
        let key = match RepoShard::from_shard_id(key) {
            Ok(repo_shard) => repo_shard,
            Err(e) => {
                let details =
                    format!("On Add Shard failed while parsing shard {key}. Error: {e:#}");
                error!("{}", &details);
                return RepoState::Failed(details);
            }
        };
        // Repo-assignment related logging
        info!("Adding shard {key}");
        let mut guarded_repo_map = self.runtime_handle.block_on(self.repo_map.write());
        {
            if let Some(repo_process) = guarded_repo_map.get_mut(&key) {
                match repo_process {
                    // The repo has already initiated setup. Need to validate if the
                    // setup is complete and the repo is ready for execution.
                    Setup(repo_setup_process) => {
                        match repo_setup_process.try_execution_process(&self.runtime_handle) {
                            // The setup is complete and repo execution has been initiated.
                            Ok(Some(repo_execution_process)) => {
                                guarded_repo_map.remove(&key);
                                let details = format!("Adding shard {key} completed successfully");
                                guarded_repo_map
                                    .insert(key, RepoProcess::Execution(repo_execution_process));
                                info!("{}", &details);
                                RepoState::Completed(details)
                            }
                            // The setup is still in-progress.
                            Ok(None) => RepoState::InProgress(format!(
                                "Setup still incomplete. Adding shard {key} is in progress."
                            )),
                            // The setup for the repo failed. Need to return failure to SM.
                            // NOTE: Its safe to return failure in this case (as compared
                            // to drop shard) since the setup process errored out, i.e. there
                            // is no active execution of this process for the given repo. SM can
                            // safely retry executing the repo on this or some other task.
                            Err(e) => {
                                let details =
                                    format!("Setup failed while adding shard {key}. Error: {e:#}");
                                error!("{}", &details);
                                // NOTE: Returning failure without removing the process from the map can
                                // cause Panics due to re-polling of the underlying JoinHandle.
                                guarded_repo_map.remove(&key);
                                RepoState::Failed(details)
                            }
                        }
                    }
                    // The repo already reached execution stage and there is nothing
                    // more to do.
                    Execution(_) => RepoState::Completed(format!("Shard {key} was already added")),
                    // The repo being added is under clean-up. This could occur if there
                    // was a rapid remove-and-add operation for the same repo on this replica
                    // and the repo being added is still finishing up its cleanup. This could
                    // also happen if SM disconnected with the server and forgot that it was
                    // dropping this repo. When SM reconnects, we just return the executing repos
                    // and SM thinks this repo is not even present on the server. In such a case,
                    // SM might try to add this repo back. We need to finish cleanup and then
                    // continue with addition.
                    Cleanup(repo_cleanup_process) => {
                        info!(
                            "Adding of shard {} will require dropping it first due to pending cleanup.",
                            &key
                        );
                        match repo_cleanup_process.try_close(&self.runtime_handle) {
                            // Repo clean-up has completed. Can safely remove the cleanup
                            // process and drop repo so that repo can be added again.
                            Ok(Some(_)) => {
                                let details = format!(
                                    "Shard addition is still in progress but shard {key} cleanup was completed successfully"
                                );
                                info!("{}", &details,);
                                guarded_repo_map.remove(&key);
                                RepoState::InProgress(details)
                            }
                            // Repo clean-up process is still underway. Return in-progress
                            // status.
                            Ok(None) => {
                                let details = format!(
                                    "Shard {key} addition is still in process since it hasn't been dropped yet"
                                );
                                RepoState::InProgress(details)
                            }
                            // Repo cleanup failed. At this point, we just remove the cleanup process
                            // and let SM re-add it.
                            Err(e) => {
                                let details = format!(
                                    "Shard {key} failed while completing a past clean-up. It cannot be added right now. Error: {e:#}"
                                );
                                guarded_repo_map.remove(&key);
                                RepoState::Failed(details)
                            }
                        }
                    }
                }
            }
            // The setup has not been initiated yet. Need to setup the execution
            // for this repo.
            else {
                let details = format!("Initiating setup. Adding shard {key} is in progress");
                let repo_setup_process = RepoSetupProcess::new(
                    self.fb,
                    shard,
                    key.clone(),
                    Arc::clone(&self.setup_job),
                    &self.runtime_handle,
                );
                guarded_repo_map.insert(key, RepoProcess::Setup(repo_setup_process));
                RepoState::InProgress(details)
            }
        }
    }

    /// Called upon initiating graceful hand-off of primary replicas (out).
    fn on_prepare_drop_shard(&self, key: &RepoShard) {
        info!("Preparing to drop shard at key '{}'", key);
        // Include additional book-keeping activities (if applicable)
        // before the repo gets taken away.
    }

    /// Called upon moving a shard (out).
    fn on_drop_shard(&self, key: &str) -> RepoState {
        let key = match RepoShard::from_shard_id(key) {
            Ok(key) => key,
            Err(e) => {
                let details = format!("Drop shard failed while parsing shard {key}. Error: {e:#}");
                error!("{}", &details);
                return RepoState::Failed(details);
            }
        };
        let mut guarded_repo_map = self.runtime_handle.block_on(self.repo_map.write());
        {
            if let Some(repo_process) = guarded_repo_map.remove(&key) {
                match repo_process {
                    // The repo is currently being executed. We need to
                    // update the repo state from execution to cleanup
                    // by creating the corresponding RepoCleanupProcess.
                    Execution(repo_execution_process) => {
                        info!("Initiating drop of shard '{}'", key,);
                        let details = format!("Dropping shard {key} in progress");
                        // SM requires the drop shard callback to be near-instantaneous.
                        // Since the unloading of a repo can take time, the cleanup is
                        // offloaded to tokio runtime returning an InProgress status to SM.
                        // This enables SM to keep periodically polling us to validate repo
                        // unloading thereby providing a wider time window for repo-cleanup.
                        let repo_cleanup_process = RepoCleanupProcess::new(
                            repo_execution_process,
                            self.timeout_secs.clone(),
                            &self.runtime_handle,
                        );
                        guarded_repo_map.insert(key, RepoProcess::Cleanup(repo_cleanup_process));
                        RepoState::InProgress(details)
                    }
                    // The repo is already undergoing cleanup. Validate if the
                    // cleanup is completed or still in-progress.
                    Cleanup(mut repo_cleanup_process) => {
                        match repo_cleanup_process.try_close(&self.runtime_handle) {
                            // Repo clean-up has completed. Can safely remove the cleanup
                            // process and drop repo.
                            Ok(Some(_)) => {
                                info!("Dropped shard '{}'", key);
                                RepoState::Completed(format!("Dropped shard {key} successfully"))
                            }
                            // Repo clean-up process is still underway. Return in-progress
                            // status. Added the cleanup process back to the map.
                            Ok(None) => {
                                let details = format!("Dropping shard {key} in still in-progress");
                                guarded_repo_map
                                    .insert(key, RepoProcess::Cleanup(repo_cleanup_process));
                                RepoState::InProgress(details)
                            }
                            // Repo clean-up failed. One of two things can happen here based
                            // on the status returned. Returning failure will make SM retry
                            // dropping this repo which no longer exists and hence cannot be
                            // dropped. If we return failure, there would be no progress made
                            // in this process for the given repo and no other task will be allowed
                            // to pick up the repo. If we return success and the underlying
                            // execution wasn't really terminated, then we can potentially have
                            // two tasks executing the same repo. It makes sense to return an error
                            // and have SM bubble up the error allowing the on-call to restart the
                            // task and resume activity instead of having potentially multiple tasks
                            // executing the same repo.
                            Err(e) => {
                                error!("Failure in dropping shard '{}'. Error: {:#}", key, e);
                                RepoState::Failed(format!(
                                    "Dropping shard {key} failed due to error in cleanup. Error: {e:#}"
                                ))
                            }
                        }
                    }
                    // The repo has not yet finished setting up and SM requires us to drop
                    // it. Dropping it in non-blocking way would require us to transition it
                    // to an execution process which can then be dropped.
                    // NOTE: This can potentially result in a lot of waiting which is why if SM
                    // asks to drop a shard that is currently being setup, let's just transition it
                    // to execution stage and then drop it.
                    Setup(mut repo_setup_process) => {
                        match repo_setup_process.try_execution_process(&self.runtime_handle) {
                            // The setup is complete and repo execution has been initiated.
                            Ok(Some(repo_execution_process)) => {
                                let details = format!(
                                    "Dropping shard {key} is in progress. The repo is in execution state and now needs to be cleaned up"
                                );
                                guarded_repo_map
                                    .insert(key, RepoProcess::Execution(repo_execution_process));
                                info!("{}", &details);
                                RepoState::InProgress(details)
                            }
                            // The setup is still in-progress.
                            Ok(None) => RepoState::InProgress(format!(
                                "Setup still incomplete. Dropping shard {key} is in progress."
                            )),
                            // The setup for the repo failed. However, we would still return success
                            // to SM since this repo was anyway required to be dropped.
                            Err(e) => {
                                let details = format!(
                                    "Setup failed while dropping shard {key}. Error: {e:#}"
                                );
                                error!("{}", &details);
                                RepoState::Completed(format!(
                                    "Shard {key} was dropped after setup failure"
                                ))
                            }
                        }
                    }
                }
            // The repo being asked to drop doesn't even exist in the map. It is possible that we already
            // deleted it and this is a duplicate request. Log the anomaly but return success.
            } else {
                error!("Couldn't find shard {} in repo_map for removal", key);
                RepoState::Completed(format!("Shard {key} was already dropped"))
            }
        }
    }

    /// Called during periodic health check of executing shards.
    fn on_shard_health_check(&self) {
        let mut guarded_repo_map = self.runtime_handle.block_on(self.repo_map.write());
        {
            guarded_repo_map.retain(|repo_name, repo_process| match repo_process {
                Execution(repo_execution_process) => {
                    match repo_execution_process.is_terminated(&self.runtime_handle) {
                        Ok(true) => {
                            error!(
                                "Removing unhealthy shard {} after valid termination",
                                &repo_name
                            );
                            false
                        }
                        Err(e) => {
                            error!(
                                "Removing unhealthy shard {} due to unexpected error: {:#}",
                                &repo_name, e
                            );
                            false
                        }
                        _ => {
                            info!("Shard {} is executing in healthy state", &repo_name);
                            true
                        }
                    }
                }
                _ => {
                    info!("Shard {} is either setting up or winding down", &repo_name);
                    true
                }
            })
        }
    }
}

impl sm::ShardManagerHandler for ShardedProcessHandler {
    fn name(&self) -> &'static str {
        self.service_name
    }

    fn health_check(&self) -> sm::Result<bool> {
        let base_healthy = self.healthy.load(Ordering::SeqCst);
        let custom_healthy = self.health_check_fn.as_ref().is_none_or(|f| f());
        Ok(base_healthy && custom_healthy)
    }

    fn prepare_add_shard(
        &self,
        request: smtypes::PrepareAddShardRequest,
    ) -> sm::Result<smtypes::PrepareAddShardResponse> {
        info!(
            "Prepare Add Shard Request from SM for {}",
            request.shard.id.domain
        );

        let key = match RepoShard::from_shard_id(&request.shard.id.domain) {
            Ok(key) => key,
            Err(e) => {
                let details = format!(
                    "On Prepare Add Shard failed while parsing shard {}. Error: {:#}",
                    request.shard.id.domain, e
                );
                error!("{}", &details);
                return Ok(smtypes::PrepareAddShardResponse {
                    status: sm::smtypes::CallbackCompletionStatus::error,
                    details,
                    ..Default::default()
                });
            }
        };
        self.on_prepare_add_shard(&key, &request.shard);

        Ok(smtypes::PrepareAddShardResponse {
            status: sm::smtypes::CallbackCompletionStatus::success,
            details: format!("Beginning repo (raw format) {key} assignment"),
            ..Default::default()
        })
    }

    fn add_shard(
        &self,
        request: smtypes::AddShardRequest,
    ) -> sm::Result<smtypes::AddShardResponse> {
        info!("Add Shard Request from SM for {}", request.shard.id.domain);
        match self.on_add_shard(&request.shard.id.domain.clone(), request.shard) {
            Completed(details) => Ok(smtypes::AddShardResponse {
                status: smtypes::CallbackCompletionStatus::success,
                details,
                ..Default::default()
            }),
            InProgress(details) => Ok(smtypes::AddShardResponse {
                status: smtypes::CallbackCompletionStatus::inprogress,
                details,
                ..Default::default()
            }),
            Failed(err) => Ok(smtypes::AddShardResponse {
                status: smtypes::CallbackCompletionStatus::error,
                details: err,
                ..Default::default()
            }),
        }
    }

    fn prepare_drop_shard(
        &self,
        request: smtypes::PrepareDropShardRequest,
    ) -> sm::Result<smtypes::PrepareDropShardResponse> {
        info!(
            "Prepare Drop Shard Request from SM for {}",
            request.shardID.domain
        );

        let key = match RepoShard::from_shard_id(&request.shardID.domain) {
            Ok(key) => key,
            Err(e) => {
                let details = format!(
                    "On Prepare Drop Shard failed while parsing shard {}. Error: {:#}",
                    request.shardID.domain, e
                );
                error!("{}", &details);
                return Ok(smtypes::PrepareDropShardResponse {
                    status: sm::smtypes::CallbackCompletionStatus::error,
                    details,
                    ..Default::default()
                });
            }
        };
        self.on_prepare_drop_shard(&key);
        Ok(smtypes::PrepareDropShardResponse {
            status: smtypes::CallbackCompletionStatus::success,
            details: format!("Beginning repo {key} removal"),
            ..Default::default()
        })
    }

    fn drop_shard(
        &self,
        request: smtypes::DropShardRequest,
    ) -> sm::Result<smtypes::DropShardResponse> {
        info!("Drop Shard Request from SM for {}", request.shardID.domain);
        match self.on_drop_shard(&request.shardID.domain) {
            Completed(details) => Ok(smtypes::DropShardResponse {
                status: smtypes::CallbackCompletionStatus::success,
                details,
                ..Default::default()
            }),
            InProgress(details) => Ok(smtypes::DropShardResponse {
                status: smtypes::CallbackCompletionStatus::inprogress,
                details,
                ..Default::default()
            }),
            Failed(err) => Ok(smtypes::DropShardResponse {
                status: smtypes::CallbackCompletionStatus::error,
                details: err,
                ..Default::default()
            }),
        }
    }

    fn lost_connection_to_shardmanager(&self) -> sm::Result<()> {
        let repos = self
            .runtime_handle
            .block_on(self.repo_map.read())
            .values()
            .map(|repo_process| format!("{}", repo_process.repo_shard()))
            .collect::<Vec<_>>();
        error!(
            "Connection lost to SM. Server will continue executing with existing {} repos: {}",
            repos.len(),
            repos.join(", ")
        );

        STATS::lost_connection_to_shardmanager.add_value(1);

        Ok(())
    }

    fn restored_connection_to_shardmanager(
        &self,
        shards_to_serve: Vec<smtypes::Shard>,
    ) -> sm::Result<()> {
        let repos = shards_to_serve
            .iter()
            .map(|s| Ok(format!("{}", RepoShard::from_shard_id(&s.id.domain)?)))
            .collect::<Result<Vec<_>>>()?;
        info!(
            "Connection restored to SM after disconnection. Resetting {} shards: {}",
            repos.len(),
            repos.join(", ")
        );

        STATS::restored_connection_to_shardmanager.add_value(1);

        // SM callbacks are sync so async calls need to be transformed to support
        // the callback contract.
        self.runtime_handle
            .block_on(self.set_shards(shards_to_serve, false))
            .with_context(|| {
                format!(
                    "Error while setting shards for {} repos: {}",
                    repos.len(),
                    repos.join(", ")
                )
            })?;
        Ok(())
    }

    fn get_state_for_consistency_checking(
        &self,
    ) -> sm::Result<smtypes::GetStateForConsistencyCheckingResponse> {
        info!("Sending state information to SM");
        // If shard-healing is enabled, update the repo map to reflect
        // only the healthy repos before responding back to SM.
        if self.shard_healing {
            self.on_shard_health_check();
        }
        Ok(smtypes::GetStateForConsistencyCheckingResponse {
            shards: self
                .runtime_handle
                .block_on(self.repo_map.read())
                .values()
                .filter_map(|repo_process| match repo_process {
                    // SM is trying to find out which shards are actually on this server.
                    // Only the shards in execution state are actually running, the rest
                    // are in the process of coming in or going out. So just return the
                    // executing shards.
                    Execution(_) => Some(repo_process.shard().clone()),
                    _ => None,
                })
                .collect(),
            status: smtypes::CallbackCompletionStatus::success,
            details: "Successfully sent state information to SM".into(),
            ..Default::default()
        })
    }
}

/// High-level entity abstracting the ShardManager dependency for executing
/// sharded repos for a given process. Responsible for managing
/// the ShardedProcessHandler for the given process.
pub struct ShardManagerExecutor {
    /// The ShardManager client handle
    client: sm::client::ShardManagerClient,
    /// The core process executor handle responsible for maintaining the state
    /// of repo execution and assigning and removing repos from the underlying process.
    handler: Arc<ShardedProcessHandler>,
}

impl ShardManagerExecutor {
    pub fn new(
        fb: FacebookInit,
        runtime_handle: Handle,
        service_name: &'static str,
        service_scope: &'static str,
        timeout_secs: u64,
        process_handle: Arc<dyn RepoShardedProcess>,
        shard_healing: bool,
        health_check_fn: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Result<Self> {
        // Disable ShardManager log spam
        folly_logging::update_logging_config(fb, "CRITICAL");
        let server_id: i32 = env::var("TW_TASK_ID")
            .context("Task ID absent in the TW task")?
            .parse()
            .context("Invalid Task ID in TW task")?;
        let service_port: u16 = env::var("TW_PORT_thrift")
            .context("Thrift port absent in the TW task")?
            .parse()
            .context("Invalid Thrift port in TW task")?;
        let shard_manager_port: u16 = env::var("TW_PORT_smclient_port")
            .context("Shard Manager port absent in the TW task")?
            .parse()
            .context("Invalid Shard Manager port in TW task")?;

        let config = sm::AppServerConfigBuilder::default()
            .service_name(service_name)
            .service_scope_name(service_scope)
            .server_id(server_id)
            .service_port(service_port)
            .shardmgr_port(Some(shard_manager_port))
            // Process execution for individual repos can fail due to transient
            // errors without bringing down the entire server. Shard-level
            // health feedback enables restarting of the specific shard/repo
            // without affecting the remaining repos on the server.
            .enable_shard_health_feedback(shard_healing)
            .max_sm_client_init_retries(MAX_SM_CLIENT_INIT_RETRIES)
            .sm_client_init_retry_interval_secs(SM_CLIENT_INIT_RETRY_SECS)
            .build()
            .map_err(|x| anyhow!("Error while building SM AppServerConfig: {x}"))?;
        let handler = Arc::new(ShardedProcessHandler::new(
            fb,
            service_name,
            runtime_handle,
            timeout_secs,
            process_handle,
            shard_healing,
            health_check_fn,
        ));
        Ok(Self {
            client: sm::client::ShardManagerClient::with_handler(fb, config, handler.clone())?,
            handler,
        })
    }

    /// Non-blocking call to begin execution of the underlying process based on the
    /// repos assigned by ShardManager
    pub fn execute(&mut self) {
        info!("Initiating sharded execution for service");
        self.client.start_callbacks_server();
    }

    /// Blocking call to begin execution of the underlying process based on the repos
    /// assigned by ShardManager
    pub async fn block_and_execute(
        mut self,
        terminate_signal_receiver: Receiver<bool>,
    ) -> Result<()> {
        info!("Initiating sharded execution for service");
        let shards = self.client.get_my_shards()?;
        let shard_ids = shards
            .iter()
            .map(|s| Ok(format!("{}", RepoShard::from_shard_id(&s.id.domain)?)))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        info!("Got initial Shard Set: {}", shard_ids);
        self.handler.set_shards(shards, true).await?;
        self.client.start_callbacks_server();
        // Keep running until the terminate signal is received. Once the signal is received,
        // exit.
        terminate_signal_receiver.await?;
        match self.client.request_failover_and_remove_handler() {
            Ok(_) => {
                info!("Successfully requested failover from shard manager");
            }
            Err(err) => {
                error!(
                    "error while requesting failover from shard manager: {}",
                    err
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
