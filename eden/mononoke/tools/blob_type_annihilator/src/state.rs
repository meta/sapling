/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! The operator-managed state directory holding a run's plan, inventory and
//! journals.
//!
//! Files that are complete records are written to a temporary name, synced
//! and renamed into place, so they are either absent or whole.
//! New files and directories are synced into their parent directories, so a
//! crash cannot lose them once they are relied on.

use std::fs::File as StdFile;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::fs;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::io::BufWriter;

use crate::scope::Plan;

const PLAN_FILE: &str = "plan.json";
const LOCK_FILE: &str = "lock";

/// An open state directory, exclusively held by this process.
pub struct StateDir {
    root: PathBuf,
    // Holds the exclusive lock for as long as the directory is open.
    _lock: StdFile,
}

impl StateDir {
    /// Records `plan` in a new state directory, or confirms an existing
    /// directory already holds the same plan.
    pub async fn create(root: &Path, plan: &Plan) -> Result<Self> {
        create_dir_durably(root)
            .await
            .with_context(|| format!("creating state directory {}", root.display()))?;
        let state = Self::lock(root)?;
        match state.read_json::<Plan>(Path::new(PLAN_FILE)).await? {
            Some(existing) if existing.digest == plan.digest => {}
            Some(existing) => bail!(
                "{} already holds plan {}; use a new state directory for a different plan",
                root.display(),
                existing.digest
            ),
            None => state.write_json(Path::new(PLAN_FILE), plan).await?,
        }
        Ok(state)
    }

    /// Opens an existing state directory and returns the plan it holds.
    pub async fn open(root: &Path) -> Result<(Self, Plan)> {
        let state = Self::lock(root)?;
        let plan = state
            .read_json::<Plan>(Path::new(PLAN_FILE))
            .await?
            .with_context(|| format!("{} holds no plan; run `plan` first", root.display()))?;
        Ok((state, plan))
    }

    fn lock(root: &Path) -> Result<Self> {
        let lock_path = root.join(LOCK_FILE);
        // Lock files are created once and never removed; `try_lock` is the
        // only operation, so this blocking open is brief.
        let lock = StdFile::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        lock.try_lock().with_context(|| {
            format!(
                "state directory {} is in use by another process",
                root.display()
            )
        })?;
        Ok(Self {
            root: root.to_path_buf(),
            _lock: lock,
        })
    }

    pub fn path(&self, relative: &Path) -> PathBuf {
        self.root.join(relative)
    }

    /// Writes `bytes` so that `relative` is either absent or complete.
    pub async fn write_atomic(&self, relative: &Path, bytes: &[u8]) -> Result<()> {
        let path = self.path(relative);
        let parent = path
            .parent()
            .with_context(|| format!("{} has no parent", path.display()))?;
        create_dir_durably(parent).await?;
        let tmp = tmp_path(&path);
        let mut file = File::create(&tmp)
            .await
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(bytes).await?;
        commit_tmp(file, &tmp, &path).await
    }

    pub async fn write_json<T: Serialize>(&self, relative: &Path, value: &T) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(value)?;
        bytes.push(b'\n');
        self.write_atomic(relative, &bytes).await
    }

    pub async fn read_json<T: DeserializeOwned>(&self, relative: &Path) -> Result<Option<T>> {
        let path = self.path(relative);
        match fs::read(&path).await {
            Ok(bytes) => {
                Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                    format!("corrupt state file {}", path.display())
                })?))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Starts writing a file that becomes visible under `relative` only once
    /// committed.
    pub async fn staged_lines(&self, relative: &Path) -> Result<StagedLines> {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            create_dir_durably(parent).await?;
        }
        let tmp = tmp_path(&path);
        let file = File::create(&tmp)
            .await
            .with_context(|| format!("creating {}", tmp.display()))?;
        Ok(StagedLines {
            writer: BufWriter::new(file),
            tmp,
            path,
        })
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Syncs the directory holding `path`, so its entry for `path` is durable.
async fn sync_parent(path: &Path) -> Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    File::open(parent)
        .await
        .with_context(|| format!("opening {}", parent.display()))?
        .sync_all()
        .await
        .with_context(|| format!("syncing {}", parent.display()))
}

/// Makes `tmp`, written through `file`, durable under `path`, so `path` is
/// either absent or complete.
async fn commit_tmp(file: File, tmp: &Path, path: &Path) -> Result<()> {
    file.sync_all().await?;
    drop(file);
    fs::rename(tmp, path)
        .await
        .with_context(|| format!("renaming {} into place", path.display()))?;
    sync_parent(path).await
}

/// Creates `dir` and any missing ancestors, syncing each new directory into
/// its parent.
async fn create_dir_durably(dir: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = dir;
    while !fs::try_exists(current).await? {
        missing.push(current);
        current = match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => break,
        };
    }
    for new_dir in missing.into_iter().rev() {
        if let Err(e) = fs::create_dir(new_dir).await
            && e.kind() != ErrorKind::AlreadyExists
        {
            return Err(e).with_context(|| format!("creating {}", new_dir.display()));
        }
        sync_parent(new_dir).await?;
    }
    Ok(())
}

/// A JSON-lines file that appears under its final name only when committed.
pub struct StagedLines {
    writer: BufWriter<File>,
    tmp: PathBuf,
    path: PathBuf,
}

impl StagedLines {
    pub async fn push<T: Serialize>(&mut self, record: &T) -> Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        Ok(())
    }

    pub async fn commit(mut self) -> Result<()> {
        self.writer.flush().await?;
        commit_tmp(self.writer.into_inner(), &self.tmp, &self.path).await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;
    use serde::Deserialize;
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Record {
        n: u32,
    }

    pub(crate) fn locked(root: &TempDir) -> Result<StateDir> {
        StateDir::lock(root.path())
    }

    #[mononoke::fbinit_test]
    async fn staged_lines_are_invisible_until_committed(_fb: FacebookInit) -> Result<()> {
        let root = TempDir::new()?;
        let state = locked(&root)?;
        let relative = Path::new("a/b.jsonl");
        let mut staged = state.staged_lines(relative).await?;
        staged.push(&Record { n: 7 }).await?;
        assert!(!fs::try_exists(state.path(relative)).await?);
        staged.commit().await?;
        assert_eq!(
            fs::read_to_string(state.path(relative)).await?,
            "{\"n\":7}\n"
        );
        assert!(locked(&root).is_err(), "a second holder must be refused");
        Ok(())
    }
}
