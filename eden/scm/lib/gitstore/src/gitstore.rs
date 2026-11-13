/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A simple store implementation to access a local git repo's odb.

use std::collections::HashMap;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Read;
use std::io::Write;
use std::ops::Deref;
use std::path::Path;
use std::process::Output;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Result;
use configmodel::Config;
use gitcompat::BareGit;
use gitcompat::GitCmd;
use progress_model::ProgressBar;
use spawn_ext::CommandError;
use types::HgId;
use types::errors::NetworkError;
use types::fetch_mode::FetchMode;

#[derive(Clone)]
pub struct GitStore(Arc<GitStoreInner>);

pub struct GitStoreInner {
    odb: git2::Odb<'static>,

    git: BareGit,

    /// If set, fetch missing objects on demand from the URL.
    fetch_url: Option<String>,
    fetch_filter: String,

    // Makes `odb` valid. Last field drops last.
    // No need to use this field. Just need to keep it alive.
    // Use `Opaque` to forbid access to the underlying repo.
    // See also `safety` notes in `GitStore::open`.
    #[allow(dead_code)]
    opaque_repo: Box<dyn Opaque + Send + Sync>,
}

trait Opaque {}

impl Deref for GitStore {
    type Target = GitStoreInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ObjectType {
    Any,
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ObjectType {
    /// Convert a string object type representation to its object type.
    pub fn from_str(s: &str) -> Option<ObjectType> {
        match s {
            "any" => Some(ObjectType::Any),
            "commit" => Some(ObjectType::Commit),
            "tree" => Some(ObjectType::Tree),
            "blob" => Some(ObjectType::Blob),
            "tag" => Some(ObjectType::Tag),
            _ => None,
        }
    }
}

impl From<ObjectType> for git2::ObjectType {
    fn from(value: ObjectType) -> Self {
        match value {
            ObjectType::Any => git2::ObjectType::Any,
            ObjectType::Commit => git2::ObjectType::Commit,
            ObjectType::Tree => git2::ObjectType::Tree,
            ObjectType::Blob => git2::ObjectType::Blob,
            ObjectType::Tag => git2::ObjectType::Tag,
        }
    }
}

impl GitStore {
    /// `open` a Git bare repo at `git_dir`. Gain access to its odb (object database).
    pub fn open(git_dir: &Path, config: &dyn Config) -> Result<Self> {
        let mut git = BareGit::from_git_dir_and_config(git_dir.to_owned(), config);
        // libgit2 < 2.0 rejects reftable repos ("unsupported extension name
        // extensions.refstorage"). GitStore only uses libgit2 for the odb. References
        // are read by gitcompat, which supports reftable. So tolerating the extension
        // is enough.
        // SAFETY: This sets a libgit2 global. Every caller sets the same value.
        unsafe { git2::opts::set_extensions(&["refstorage"])? };
        let git_repo = match git2::Repository::open(git_dir) {
            Ok(repo) => repo,
            Err(err) if err.message() == "unsupported extension name extensions.partialclone" => {
                // git-repo still sets extensions.partialClone on every filtered fetch,
                // causing libgit2 to reject the repo. Git 2.24+ maintains partial clone
                // settings under remote.<name>.promisor. Without that setting for the remote
                // named by extensions.partialClone, the extension might still be the only
                // partial clone marker.
                //
                // GitStore uses libgit2 for local objects and git fetch for missing objects.
                // Removing the extension preserves partial clone support. A later repo sync
                // can restore it, so this cleanup must run again on affected opens.
                // See https://gerrit-review.googlesource.com/c/git-repo/+/528541
                let output = git.call(
                    "config",
                    &[
                        "--null",
                        "--get-regexp",
                        r"^(extensions\.partialclone|remote\..*\.promisor)$",
                    ],
                )?;
                // --null uses "key\nvalue\0"; valueless settings are just "key\0".
                let configs = std::str::from_utf8(&output.stdout)?
                    .split_terminator('\0')
                    .map(|entry| entry.split_once('\n').unwrap_or((entry, "")))
                    .collect::<HashMap<_, _>>();
                configs
                    .get("extensions.partialclone")
                    .and_then(|remote| configs.get(format!("remote.{remote}.promisor").as_str()))
                    .ok_or(err)?;
                git.run(
                    "config",
                    &["--local", "--unset-all", "extensions.partialclone"],
                )?;
                git2::Repository::open(git_dir)?
            }
            Err(err) => return Err(err.into()),
        };
        // "<format>://<path>" stores references at a custom path. gitcompat cannot
        // read those, so keep rejecting them, like libgit2 did.
        if let Ok(value) = git_repo.config()?.get_string("extensions.refstorage") {
            if value.contains("://") {
                anyhow::bail!(
                    "Git references stored at a custom path are not supported (extensions.refstorage = {value:?})"
                );
            }
        }
        let odb = git_repo.odb()?;

        // Git's negotiation algorithm works on commit reference level and can add significant
        // overhead if we simply want to fetch trees or blobs.
        // See also Git's promisor-remote which sets the same config:
        // https://github.com/git/git/blob/b3d1c85d4833aef546f11e4d37516a1ececaefc3/promisor-remote.c#L30
        git.extra_git_configs
            .push("fetch.negotiationAlgorithm=noop".to_string());

        let fetch_url = config.get("paths", "default").map(|s| s.to_string());

        // "filter" passed to `git fetch`. "blob:none" is used by Git's promisor-remote but that
        // does not deduplicate trees. "tree:0" more aggressively deduplicates trees but might
        // cause more network round trips.
        let fetch_filter = {
            let config = config.get("git", "filter");
            let config = match &config {
                Some(v) => v.as_ref(),
                // PERF: Ideally this is "tree:0" but the tree diff is currently sequential...
                None => "blob:none",
            };
            format!("--filter={config}")
        };

        struct UnsafeForceSync<T: ?Sized>(T);
        unsafe impl<T: ?Sized> Send for UnsafeForceSync<T> {}
        unsafe impl<T: ?Sized> Sync for UnsafeForceSync<T> {}
        impl Opaque for UnsafeForceSync<git2::Repository> {}

        // safety: `odb` is alive as long as `git_repo` is alive.
        let odb = unsafe { std::mem::transmute::<git2::Odb<'_>, git2::Odb<'static>>(odb) };
        // safety: we don't access `opaque_repo` in multiple threads.
        // Cast to `Opaque` and prevents access to `git_repo`.
        let opaque_repo: Box<dyn Opaque + Send + Sync> = Box::new(UnsafeForceSync(git_repo));

        tracing::debug!(
            target: "gitstore::open",
            git_dir = ?git_dir,
            fetch_url = &fetch_url,
            fetch_filter = &fetch_filter,
            "GitStore::open"
        );
        let inner = GitStoreInner {
            odb,
            git,
            fetch_url,
            fetch_filter,
            opaque_repo,
        };
        let store = GitStore(Arc::new(inner));
        Ok(store)
    }

    /// Read an object of the given type.
    pub fn read_obj(&self, id: HgId, kind: ObjectType, mode: FetchMode) -> Result<Vec<u8>> {
        if id.is_null() {
            return Ok(Vec::new());
        }
        if !mode.is_local() {
            self.fetch_objs(&[id])?;
        }
        let oid = hgid_to_git_oid(id);
        let obj = self.odb.read(oid)?;
        let kind = git2::ObjectType::from(kind);
        if kind != git2::ObjectType::Any && obj.kind() != kind {
            return Err(git2::Error::new(
                git2::ErrorCode::NotFound,
                git2::ErrorClass::Object,
                format!("{kind} {oid} not found"),
            )
            .into());
        }
        Ok(obj.data().to_vec())
    }

    /// Read an object. Do not trigger remote fetching.
    /// Returns `None` if the object does not exist.
    pub fn read_local_obj_optional(&self, id: HgId, kind: ObjectType) -> Result<Option<Vec<u8>>> {
        // NOTE: This might be racy.
        if !self.has_obj(id)? {
            return Ok(None);
        }
        let oid = hgid_to_git_oid(id);
        let obj = self.odb.read(oid)?;
        let kind = git2::ObjectType::from(kind);
        if kind != git2::ObjectType::Any && obj.kind() != kind {
            return Ok(None);
        }
        Ok(Some(obj.data().to_vec()))
    }

    /// Test if an object exists in the odb.
    pub fn has_obj(&self, id: HgId) -> Result<bool> {
        let oid = hgid_to_git_oid(id);
        // Note: "exists" triggers "refresh".
        Ok(self.odb.exists(oid))
    }

    /// Read the size of an object without its full content.
    pub fn read_obj_size(&self, id: HgId, kind: ObjectType) -> Result<usize> {
        if id.is_null() {
            return Ok(0);
        }
        self.fetch_objs(&[id])?;
        let oid = hgid_to_git_oid(id);
        let (size, obj_kind) = self.odb.read_header(oid)?;
        let kind = git2::ObjectType::from(kind);
        if kind != git2::ObjectType::Any && obj_kind != kind {
            return Err(git2::Error::new(
                git2::ErrorCode::NotFound,
                git2::ErrorClass::Object,
                format!("{kind} {oid} not found"),
            )
            .into());
        }
        Ok(size)
    }

    /// Write object to the odb.
    pub fn write_obj(&self, kind: ObjectType, data: &[u8]) -> Result<HgId> {
        let oid = self.odb.write(kind.into(), data)?;
        let id = git_oid_to_hgid(oid);
        Ok(id)
    }

    /// Fetch the oids from fetch_remote. Existing oids will be skipped.
    /// If every oid exists locally, then no `git fetch` process is spawned.
    /// Otherwise, block until the `git fetch` command completes.
    /// Report `git fetch` errors as `NetworkError`.
    pub fn fetch_objs(&self, ids: &[HgId]) -> Result<()> {
        let mut missing_ids = ids.iter().filter(|id| {
            if id.is_null() {
                return false;
            }
            let id = hgid_to_git_oid(**id);
            // For performance, disable refresh here.
            !self.odb.exists_ext(id, git2::OdbLookupFlags::NO_REFRESH)
        });

        let first_missing_id = match missing_ids.next() {
            // No need to fetch.
            None => return Ok(()),
            Some(id) => id,
        };
        let missing_ids = std::iter::once(first_missing_id).chain(missing_ids);

        let url = match self.fetch_url.as_ref() {
            Some(url) => url,
            None => anyhow::bail!("paths.default is not set to fetch remotely"),
        };

        // See also git/promisor-remote.c
        let args = [
            url,
            "--no-tags",
            // TODO: Upgrade Git so it supports this flag.
            #[cfg(not(windows))]
            "--no-write-fetch-head",
            "--recurse-submodules=no",
            &self.fetch_filter,
            "--stdin",
            "--progress",
        ];
        let mut cmd = self.git.git_cmd("fetch", &args);
        let mut child = cmd.stdin(Stdio::piped()).stderr(Stdio::piped()).spawn()?;

        if let Some(stdin) = child.stdin.take() {
            let mut stdin = BufWriter::new(stdin);
            let mut count = 0;
            for id in missing_ids {
                let hex = id.to_hex();
                tracing::trace!(target: "gitstore::fetch::detail", hex, "fetch object");
                stdin.write_all(hex.as_bytes())?;
                stdin.write_all(b"\n")?;
                count += 1;
            }
            drop(stdin);
            tracing::debug!(target: "gitstore::fetch", count, "fetch objects");
        }

        // git reads all input before running actual fetch that might print progress info
        // (see builtin/fetch.c). No need to use a thread to read output.
        let mut stderr_output: Vec<u8> = Vec::with_capacity(1024);
        if let Some(stderr) = child.stderr.take() {
            let mut stderr = BufReader::with_capacity(64, stderr);
            let mut buf = [0u8; 1];
            let mut last_line = Vec::with_capacity(64);
            let bar = ProgressBar::new_adhoc("git fetch", 0, "");
            while stderr.read_exact(&mut buf).is_ok() {
                match buf[0] {
                    b'\r' | b'\n' => {
                        if buf[0] == b'\n' {
                            stderr_output.extend_from_slice(&last_line);
                            stderr_output.push(b'\n');
                        }
                        update_progress(&bar, std::str::from_utf8(&last_line).unwrap_or(""));
                        last_line.clear();
                    }
                    c => {
                        last_line.push(c);
                    }
                }
            }
        }

        let status = child.wait()?;
        if !status.success() {
            let output = Output {
                status,
                stdout: Vec::new(),
                stderr: stderr_output,
            };
            let err = CommandError::new(&cmd, None).with_output(&output);
            return Err(NetworkError(err.into()).into());
        }
        Ok(())
    }

    /// Returns true if `fetch_url` is set.
    pub fn has_fetch_url(&self) -> bool {
        self.fetch_url.is_some()
    }
}

fn hgid_to_git_oid(id: HgId) -> git2::Oid {
    git2::Oid::from_bytes(id.as_ref()).expect("HgId should convert to git2::Oid")
}

fn git_oid_to_hgid(oid: git2::Oid) -> HgId {
    HgId::from_slice(oid.as_bytes()).expect("git2::Oid should convert to HgId")
}

fn update_progress(bar: &ProgressBar, line: &str) -> Option<()> {
    // Check if the message looks like a progress, examples:
    //
    // remote: Enumerating objects: 10414, done.
    // remote: Counting objects: 100% (10414/10414), done.
    // remote: Compressing objects: 100% (8992/8992), done.
    // remote: Total 10414 (delta 294), reused 7985 (delta 121), pack-reused 0
    // Receiving objects: 100% (10414/10414), 2.62 MiB | 13.14 MiB/s, done.
    // Resolving deltas: 100% (294/294), done.
    let (left, right) = line.split_once("% (")?;
    let (current, total) = right.split_once(')')?.0.split_once('/')?;
    let message = left.rsplit_once(':')?.0;
    let current = current.parse::<u64>().ok()?;
    let total = total.parse::<u64>().ok()?;
    if total > 0 && total > current {
        bar.set_total(total);
        bar.set_position(current);
        bar.set_message(message.to_string());
    }
    Some(())
}
