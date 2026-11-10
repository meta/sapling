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
//! and renamed into place, so they are either absent or whole.  Journals are
//! append-only, and every record is synced before the action it announces.
//! New files and directories are synced into their parent directories, so a
//! crash cannot lose them once they are relied on.

use std::fs::File as StdFile;
use std::io::ErrorKind;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::fs;
use tokio::fs::File;
use tokio::fs::OpenOptions;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
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

    pub async fn exists(&self, relative: &Path) -> Result<bool> {
        Ok(fs::try_exists(self.path(relative)).await?)
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

    async fn read_text(&self, relative: &Path) -> Result<String> {
        let path = self.path(relative);
        match fs::read_to_string(&path).await {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Reads a complete file of JSON lines; a missing file is empty.
    pub async fn read_json_lines<T: DeserializeOwned>(&self, relative: &Path) -> Result<Vec<T>> {
        let text = self.read_text(relative).await?;
        parse_json_lines(&text)
            .with_context(|| format!("corrupt state file {}", self.path(relative).display()))
    }

    /// Reads a journal; a missing journal is empty.  An unterminated last
    /// line was never acknowledged, so it is ignored.
    pub async fn read_journal<T: DeserializeOwned>(&self, relative: &Path) -> Result<Vec<T>> {
        let text = self.read_text(relative).await?;
        let acknowledged = text.rfind('\n').map_or("", |end| &text[..=end]);
        parse_json_lines(acknowledged)
            .with_context(|| format!("corrupt journal {}", self.path(relative).display()))
    }

    /// A journal at `relative`.  Nothing is created until its first record.
    pub fn journal(&self, relative: &Path) -> Journal {
        Journal {
            path: self.path(relative),
            file: None,
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

/// Opens `path` for reading and appending, creating it and its directories
/// durably.  Returns whether it was created.
async fn open_durably(path: &Path) -> Result<(File, bool)> {
    if let Some(parent) = path.parent() {
        create_dir_durably(parent).await?;
    }
    let created = !fs::try_exists(path).await?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    if created {
        sync_parent(path).await?;
    }
    Ok((file, created))
}

/// Drops an unterminated last line, so the next record starts on its own
/// line.
async fn truncate_torn_tail(file: &mut File, path: &Path) -> Result<()> {
    let len = file.metadata().await?.len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(len - 1)).await?;
    if file.read_u8().await? == b'\n' {
        return Ok(());
    }
    file.seek(SeekFrom::Start(0)).await?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await?;
    let keep = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |end| end + 1);
    file.set_len(u64::try_from(keep)?)
        .await
        .with_context(|| format!("truncating the torn last line of {}", path.display()))?;
    file.sync_data().await?;
    Ok(())
}

fn parse_json_lines<T: DeserializeOwned>(text: &str) -> Result<Vec<T>> {
    text.lines()
        .filter(|line| !line.is_empty())
        .enumerate()
        .map(|(n, line)| serde_json::from_str(line).with_context(|| format!("line {}", n + 1)))
        .collect()
}

/// An append-only file of JSON records, created by its first record.
pub struct Journal {
    path: PathBuf,
    file: Option<File>,
}

impl Journal {
    /// Appends `record` and syncs it to disk before returning.
    pub async fn append<T: Serialize>(&mut self, record: &T) -> Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let file = match &mut self.file {
            Some(file) => file,
            None => {
                let (mut file, created) = open_durably(&self.path).await?;
                if !created {
                    truncate_torn_tail(&mut file, &self.path).await?;
                }
                self.file.insert(file)
            }
        };
        file.write_all(&line)
            .await
            .with_context(|| format!("appending to {}", self.path.display()))?;
        file.sync_data().await?;
        Ok(())
    }
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

    #[mononoke::test]
    fn json_lines_reject_corruption() {
        let records: Vec<Record> =
            parse_json_lines("{\"n\":1}\n\n{\"n\":2}\n").expect("valid lines parse");
        assert_eq!(records, vec![Record { n: 1 }, Record { n: 2 }]);
        assert!(parse_json_lines::<Record>("{\"n\":1}\n{\"n\":").is_err());
    }

    #[mononoke::fbinit_test]
    async fn journal_records_are_read_back(_fb: FacebookInit) -> Result<()> {
        let root = TempDir::new()?;
        let state = locked(&root)?;
        let relative = Path::new("j/j.jsonl");
        let mut journal = state.journal(relative);
        assert!(
            !state.exists(Path::new("j")).await?,
            "a journal is created by its first record"
        );
        assert!(state.read_journal::<Record>(relative).await?.is_empty());
        journal.append(&Record { n: 1 }).await?;
        journal.append(&Record { n: 2 }).await?;
        assert_eq!(
            state.read_journal::<Record>(relative).await?,
            vec![Record { n: 1 }, Record { n: 2 }]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_torn_last_journal_line_is_dropped(_fb: FacebookInit) -> Result<()> {
        let root = TempDir::new()?;
        let state = locked(&root)?;
        let relative = Path::new("j.jsonl");
        fs::write(state.path(relative), "{\"n\":1}\n{\"n\":").await?;
        assert_eq!(
            state.read_journal::<Record>(relative).await?,
            vec![Record { n: 1 }],
            "the unacknowledged fragment is ignored"
        );
        assert!(
            state.read_json_lines::<Record>(relative).await.is_err(),
            "complete files stay strict"
        );
        state.journal(relative).append(&Record { n: 2 }).await?;
        assert_eq!(
            fs::read_to_string(state.path(relative)).await?,
            "{\"n\":1}\n{\"n\":2}\n",
            "the fragment is truncated before the next record"
        );
        Ok(())
    }
}
