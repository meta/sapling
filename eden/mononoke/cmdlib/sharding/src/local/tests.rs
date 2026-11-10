/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;
use std::fs;
use std::fs::File;
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::bail;
use async_trait::async_trait;
use mononoke_macros::mononoke;

use super::*;

/// Records each setup and stop, fails the setup of the repos in `broken`, and
/// fails the execution of the repos in `dying`.
#[derive(Default)]
struct FakeProcess {
    events: Arc<Mutex<Vec<String>>>,
    broken: Mutex<BTreeSet<String>>,
    dying: Mutex<BTreeSet<String>>,
}

impl FakeProcess {
    fn set_broken(&self, repo: &str, broken: bool) {
        let mut set = self.broken.lock().unwrap();
        if broken {
            set.insert(repo.to_owned());
        } else {
            set.remove(repo);
        }
    }

    /// The events since the last call, in any order.
    fn take_events(&self) -> BTreeSet<String> {
        self.events.lock().unwrap().drain(..).collect()
    }
}

#[async_trait]
impl RepoShardedProcess for FakeProcess {
    async fn setup(&self, repo: &RepoShard) -> Result<Arc<dyn RepoShardedProcessExecutor>> {
        if self.broken.lock().unwrap().contains(&repo.repo_name) {
            bail!("{} is broken", repo.repo_name);
        }
        self.events
            .lock()
            .unwrap()
            .push(format!("setup {}", repo.repo_name));
        Ok(Arc::new(FakeExecutor {
            repo: repo.repo_name.clone(),
            events: self.events.clone(),
            dies: self.dying.lock().unwrap().contains(&repo.repo_name),
        }))
    }
}

struct FakeExecutor {
    repo: String,
    events: Arc<Mutex<Vec<String>>>,
    dies: bool,
}

#[async_trait]
impl RepoShardedProcessExecutor for FakeExecutor {
    async fn execute(&self) -> Result<()> {
        if self.dies {
            bail!("{} died", self.repo);
        }
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(format!("stop {}", self.repo));
        Ok(())
    }
}

fn events(events: &[&str]) -> BTreeSet<String> {
    events.iter().map(|event| event.to_string()).collect()
}

fn status(path: &Path, suffix: &str) -> String {
    fs::read_to_string(with_suffix(path, suffix)).unwrap()
}

#[mononoke::test]
async fn test_shards_follow_the_file() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("shards");
    let process = Arc::new(FakeProcess::default());
    let mut executor = LocalShardExecutor::new(path.clone(), Handle::current(), 1, process.clone());

    executor.sync().await?;
    assert_eq!(
        status(&path, ".active"),
        "",
        "a missing file lists no shards"
    );

    fs::write(&path, "a\n\n b \n")?;
    executor.sync().await?;
    assert_eq!(status(&path, ".active"), "a\nb\n");
    assert_eq!(status(&path, ".errors"), "");
    assert_eq!(process.take_events(), events(&["setup a", "setup b"]));

    executor.sync().await?;
    assert_eq!(
        process.take_events(),
        events(&[]),
        "an unchanged file changes nothing"
    );

    fs::write(&path, "b\nc\n")?;
    executor.sync().await?;
    assert_eq!(status(&path, ".active"), "b\nc\n");
    assert_eq!(process.take_events(), events(&["stop a", "setup c"]));
    Ok(())
}

#[mononoke::test]
async fn test_failed_setups_are_listed_and_retried() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("shards");
    let process = Arc::new(FakeProcess::default());
    let mut executor = LocalShardExecutor::new(path.clone(), Handle::current(), 1, process.clone());

    process.set_broken("b", true);
    fs::write(&path, "a\nb\nrepo_CHUNK_1_OF_x\n")?;
    executor.sync().await?;
    assert_eq!(status(&path, ".active"), "a\n");
    let errors = status(&path, ".errors");
    assert!(
        errors.starts_with("b: b is broken\nrepo_CHUNK_1_OF_x: "),
        "{errors}"
    );

    process.set_broken("b", false);
    executor.sync().await?;
    assert_eq!(
        status(&path, ".active"),
        "a\n",
        "a failed setup isn't retried until the file is rewritten"
    );

    // Rewriting the file unchanged retries it.
    File::options()
        .write(true)
        .open(&path)?
        .set_modified(SystemTime::now() + Duration::from_secs(10))?;
    executor.sync().await?;
    assert_eq!(status(&path, ".active"), "a\nb\n");
    assert!(status(&path, ".errors").starts_with("repo_CHUNK_1_OF_x: "));

    fs::write(&path, "a\nb\n")?;
    executor.sync().await?;
    assert_eq!(
        status(&path, ".errors"),
        "",
        "a shard the file no longer lists has no error"
    );
    Ok(())
}

#[mononoke::test]
async fn test_failed_executions_are_listed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("shards");
    let process = Arc::new(FakeProcess::default());
    let mut executor = LocalShardExecutor::new(path.clone(), Handle::current(), 1, process.clone());

    process.dying.lock().unwrap().insert("b".to_owned());
    fs::write(&path, "a\nb\n")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        executor.sync().await?;
        if status(&path, ".active") == "a\n" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the failed execution was never listed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(status(&path, ".errors"), "b: b died\n");
    Ok(())
}

#[mononoke::test]
async fn test_run_polls_until_terminated() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("shards");
    fs::write(&path, "a\n")?;
    let executor = LocalShardExecutor::new(
        path.clone(),
        Handle::current(),
        1,
        Arc::new(FakeProcess::default()),
    );
    let (terminate, terminated) = tokio::sync::oneshot::channel::<()>();
    let running = mononoke::spawn_task(executor.run(async {
        let _ = terminated.await;
    }));

    fs::write(&path, "a\nb\n")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while fs::read_to_string(with_suffix(&path, ".active")).ok() != Some("a\nb\n".to_owned()) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the change was never applied"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    terminate.send(()).unwrap();
    running.await??;
    Ok(())
}
