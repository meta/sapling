/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! A stand-in for ShardManager, for tests and local deployments. A file lists
//! the shards the process serves, one shard id per line. The executor polls it
//! and sets up or drops shards as it changes, then lists the shards that are
//! fully set up in `<path>.active` and those whose setup or execution failed
//! in `<path>.errors`, so a test can wait for an assignment to apply.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::future::Future;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Context;
use anyhow::Result;
use futures::StreamExt;
use futures::stream;
use sharding_ext::RepoShard;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tracing::error;
use tracing::info;

use crate::RepoShardedProcess;
use crate::RepoShardedProcessExecutor;

/// How often the shard file is read: as often as a local config store is.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How many shards are set up or dropped at once.
const CONCURRENCY: usize = 20;

/// The shard file's contents and modification time, so that rewriting it
/// unchanged retries the shards whose setup failed.
type FileVersion = (String, Option<SystemTime>);

/// Runs a process's repos as the shard file at `path` assigns them.
pub struct LocalShardExecutor {
    path: PathBuf,
    process: Arc<dyn RepoShardedProcess>,
    runtime_handle: Handle,
    timeout: Duration,
    applied: Option<FileVersion>,
    active: BTreeMap<String, ActiveShard>,
    errors: BTreeMap<String, String>,
    /// The status files may not match `active` and `errors`.
    status_stale: bool,
}

/// A shard that is set up, and the task executing it until it finishes
/// without error.
struct ActiveShard {
    executor: Arc<dyn RepoShardedProcessExecutor>,
    execution: Option<JoinHandle<Result<()>>>,
}

impl LocalShardExecutor {
    pub fn new(
        path: PathBuf,
        runtime_handle: Handle,
        timeout_secs: u64,
        process: Arc<dyn RepoShardedProcess>,
    ) -> Self {
        Self {
            path,
            process,
            runtime_handle,
            timeout: Duration::from_secs(timeout_secs),
            applied: None,
            active: BTreeMap::new(),
            errors: BTreeMap::new(),
            status_stale: false,
        }
    }

    /// Follows the shard file until `terminate` completes.
    pub async fn run(mut self, terminate: impl Future<Output = ()>) -> Result<()> {
        info!("Following shard assignments in {}", self.path.display());
        tokio::pin!(terminate);
        loop {
            if let Err(e) = self.sync().await {
                error!(
                    "Failed to apply shard assignments from {}: {e:#}",
                    self.path.display()
                );
            }
            tokio::select! {
                _ = &mut terminate => return Ok(()),
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }
    }

    /// Applies the shard file if it changed since it was last applied, moves
    /// shards whose execution failed to the errors, and writes the status
    /// files if either changed them.
    async fn sync(&mut self) -> Result<()> {
        let version = read_shard_file(&self.path).await?;
        let failed = self.remove_failed_executions().await;
        if self.applied.as_ref() != Some(&version) {
            self.apply(&version).await;
            self.applied = Some(version);
        } else if !failed && !self.status_stale {
            return Ok(());
        }
        self.status_stale = true;
        self.write_status().await?;
        self.status_stale = false;
        Ok(())
    }

    /// Drops the shards `version` no longer lists, and sets up the ones it
    /// newly lists (and retries failed ones).
    async fn apply(&mut self, version: &FileVersion) {
        let wanted: BTreeSet<String> = version
            .0
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();

        let (kept, dropped): (BTreeMap<_, _>, BTreeMap<_, _>) = std::mem::take(&mut self.active)
            .into_iter()
            .partition(|(id, _)| wanted.contains(id));
        self.active = kept;
        let timeout = self.timeout;
        stream::iter(dropped)
            .for_each_concurrent(CONCURRENCY, |(id, shard)| shard.stop(id, timeout))
            .await;
        self.errors.retain(|id, _| wanted.contains(id));

        let added: Vec<String> = wanted
            .into_iter()
            .filter(|id| !self.active.contains_key(id))
            .collect();
        let (process, runtime_handle) = (&self.process, &self.runtime_handle);
        let set_up: Vec<(String, Result<ActiveShard>)> = stream::iter(added)
            .map(|id| async move {
                let shard = ActiveShard::set_up(process, runtime_handle, &id).await;
                (id, shard)
            })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;
        for (id, shard) in set_up {
            match shard {
                Ok(shard) => {
                    info!("Shard {id} is set up");
                    self.errors.remove(&id);
                    self.active.insert(id, shard);
                }
                Err(e) => {
                    error!("Failed to set up shard {id}: {e:#}");
                    self.errors.insert(id, format!("{e:#}"));
                }
            }
        }
    }

    /// Moves the shards whose execution failed from `active` to `errors`, and
    /// returns whether there were any.
    async fn remove_failed_executions(&mut self) -> bool {
        let mut failed = Vec::new();
        for (id, shard) in self.active.iter_mut() {
            let Some(execution) = shard.execution.take_if(|execution| execution.is_finished())
            else {
                continue;
            };
            match execution.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => failed.push((id.clone(), format!("{e:#}"))),
                Err(e) => failed.push((id.clone(), format!("execution did not complete: {e}"))),
            }
        }
        let any = !failed.is_empty();
        for (id, error) in failed {
            error!("Shard {id} execution failed: {error}");
            self.active.remove(&id);
            self.errors.insert(id, error);
        }
        any
    }

    /// Writes `<path>.active` and then `<path>.errors`, so a reader that sees
    /// a shard's error also sees the active shards that go with it.
    async fn write_status(&self) -> Result<()> {
        let active: String = self.active.keys().map(|id| format!("{id}\n")).collect();
        let errors: String = self
            .errors
            .iter()
            .map(|(id, e)| format!("{id}: {}\n", e.replace('\n', " ")))
            .collect();
        write_atomically(&with_suffix(&self.path, ".active"), &active).await?;
        write_atomically(&with_suffix(&self.path, ".errors"), &errors).await
    }
}

impl ActiveShard {
    async fn set_up(
        process: &Arc<dyn RepoShardedProcess>,
        runtime_handle: &Handle,
        id: &str,
    ) -> Result<Self> {
        let repo_shard = RepoShard::from_shard_id(id)?;
        info!("Setting up shard {repo_shard}");
        let executor = process.setup(&repo_shard).await?;
        let execution = runtime_handle.spawn({
            let executor = executor.clone();
            async move { executor.execute().await }
        });
        Ok(Self {
            executor,
            execution: Some(execution),
        })
    }

    /// Stops the shard as ShardManager's drop does: calls `stop`, then gives
    /// the execution up to `timeout` to finish before aborting it.
    async fn stop(self, id: String, timeout: Duration) {
        info!("Dropping shard {id}");
        match tokio::time::timeout(timeout, self.executor.stop()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!("Failed to stop shard {id}: {e:#}"),
            Err(_) => error!("Timed out stopping shard {id}"),
        }
        if let Some(mut execution) = self.execution {
            match tokio::time::timeout(timeout, &mut execution).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => error!("Shard {id} execution failed: {e:#}"),
                Ok(Err(e)) => error!("Shard {id} execution did not complete: {e}"),
                Err(_) => {
                    execution.abort();
                    error!("Shard {id} execution was aborted after {timeout:?}");
                }
            }
        }
        info!("Dropped shard {id}");
    }
}

/// The shard file's contents and modification time. A missing file lists no
/// shards.
async fn read_shard_file(path: &Path) -> Result<FileVersion> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => {
            let modified = tokio::fs::metadata(path)
                .await
                .ok()
                .and_then(|metadata| metadata.modified().ok());
            Ok((contents, modified))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok((String::new(), None)),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// `path` with `suffix` appended to its file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Writes `contents` so that readers see the old file or the new one, never
/// part of one.
async fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let tmp = with_suffix(path, ".tmp");
    tokio::fs::write(&tmp, contents)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
}

#[cfg(test)]
mod tests;
