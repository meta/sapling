/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
#[cfg(unix)]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::future::Future;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::io::ErrorKind;
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(target_os = "linux")]
use std::os::linux::fs::MetadataExt as MetadataLinuxExt;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
#[cfg(unix)]
use std::process::Stdio;
use std::str::FromStr;

use anyhow::Context;
use anyhow::anyhow;
use async_recursion::async_recursion;
use edenfs_error::EdenFsError;
use edenfs_error::Result;
use edenfs_error::ResultExt;
use edenfs_utils::metadata::MetadataExt;
use edenfs_utils::remove_symlink;
use edenfs_utils::trace_env;
use hg_util::no_follow::NoFollowRoot;
#[cfg(target_os = "windows")]
use mkscratch::zzencode;
#[cfg(target_os = "macos")]
use psutil::disk::disk_usage;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use toml::value::Value;

use crate::checkout::EdenFsCheckout;
use crate::checkout::find_checkout;
use crate::fsutil::forcefully_remove_dir_all;
use crate::fsutil::remove_file;
use crate::instance::EdenFsInstance;
#[cfg(target_os = "linux")]
use crate::mounttable::MountTableSnapshot;
#[cfg(target_os = "linux")]
use crate::mounttable::is_mount_point;
use crate::mounttable::read_mount_table;

pub const REPO_SOURCE: &str = ".eden-redirections";
pub const USER_REDIRECTION_SOURCE: &str = ".eden/client/config.toml:redirections";
pub const APFS_HELPER: &str = "/usr/local/libexec/eden/eden_apfs_mount_helper";
#[cfg(unix)]
const MKSCRATCH_SUCCESS_MARKER: &[u8] = b"\x1dEDEN_MKSCRATCH_SUCCESS\x1e";
#[cfg(unix)]
const MKSCRATCH_WRAPPER: &str = r#""$@" && printf '\035EDEN_MKSCRATCH_SUCCESS\036' >&2"#;

#[derive(Debug)]
enum MkscratchExitStatus {
    Exited(ExitStatus),
    RecoveredAfterReap,
}

impl MkscratchExitStatus {
    fn success(&self) -> bool {
        match self {
            MkscratchExitStatus::Exited(status) => status.success(),
            MkscratchExitStatus::RecoveredAfterReap => true,
        }
    }
}

#[derive(Debug)]
struct MkscratchOutput {
    status: MkscratchExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Clone, Serialize, Copy, Debug, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum RedirectionType {
    /// Linux: a bind mount to a mkscratch generated path
    /// macOS: a mounted dmg file in a mkscratch generated path
    /// Windows: equivalent to symlink type
    Bind,
    /// A symlink to a mkscratch generated path
    Symlink,
    Unknown,
}

impl fmt::Display for RedirectionType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                RedirectionType::Bind => "bind",
                RedirectionType::Symlink => "symlink",
                RedirectionType::Unknown => "unknown",
            }
        )
    }
}

impl FromStr for RedirectionType {
    type Err = EdenFsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "bind" {
            Ok(RedirectionType::Bind)
        } else if s == "symlink" {
            Ok(RedirectionType::Symlink)
        } else {
            // deliberately did not implement "Unknown"
            Err(EdenFsError::ConfigurationError(format!(
                "Unknown redirection type: {s}. Must be one of: bind, symlink"
            )))
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DarwinBindRedirectionType {
    APFS,
    DMG,
    SYMLINK,
}

impl fmt::Display for DarwinBindRedirectionType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                DarwinBindRedirectionType::APFS => "apfs",
                DarwinBindRedirectionType::DMG => "dmg",
                DarwinBindRedirectionType::SYMLINK => "symlink",
            }
        )
    }
}

impl FromStr for DarwinBindRedirectionType {
    type Err = EdenFsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.to_lowercase() == "apfs" || s.is_empty() {
            Ok(DarwinBindRedirectionType::APFS)
        } else if s.to_lowercase() == "dmg" {
            Ok(DarwinBindRedirectionType::DMG)
        } else if s.to_lowercase() == "symlink" {
            Ok(DarwinBindRedirectionType::SYMLINK)
        } else {
            // deliberately did not implement "Unknown"
            Err(EdenFsError::ConfigurationError(format!(
                "Unknown darwin bind redirection type: {s}. Must be one of: apfs, dmg, symlink"
            )))
        }
    }
}

#[derive(PartialEq, Debug)]
pub enum RepoPathDisposition {
    DoesNotExist,
    IsSymlink,
    IsBindMount,
    IsEmptyDir,
    IsNonEmptyDir,
    IsFile,
}

impl RepoPathDisposition {
    pub fn analyze(path: &Path) -> Result<RepoPathDisposition> {
        // We can't simply check path.exists() since that follows symlinks and checks whether the
        // symlink target exists (not the symlink itself). We want to know whether a symlink exists
        // regardless of whether the target exists or not.
        //
        // symlink_metadata() returns an error type if the path DNE and it returns the file
        // metadata otherwise. We can leverage this to tell whether or not the file exists, and
        // whether it's a symlink if it does exist.
        match std::fs::symlink_metadata(path).map(|m| m.file_type()) {
            Ok(file_type) if file_type.is_symlink() => Ok(RepoPathDisposition::IsSymlink),
            Ok(file_type) if file_type.is_dir() => match is_bind_mount(path.to_path_buf()) {
                Ok(true) => Ok(RepoPathDisposition::IsBindMount),
                Ok(false) => match is_empty_dir(path) {
                    Ok(true) => Ok(RepoPathDisposition::IsEmptyDir),
                    Ok(false) => Ok(RepoPathDisposition::IsNonEmptyDir),
                    Err(e) => Err(e).with_context(|| {
                        format!(
                            "failed to determine whether {} is an empty dir",
                            path.display()
                        )
                    })?,
                },
                Err(e) => Err(e).with_context(|| {
                    format!(
                        "failed to determine whether {} is a bind mount",
                        path.display()
                    )
                })?,
            },
            Ok(_) => Ok(RepoPathDisposition::IsFile),
            Err(_) => Ok(RepoPathDisposition::DoesNotExist),
        }
    }
}

impl fmt::Display for RepoPathDisposition {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                Self::DoesNotExist => "does-not-exist",
                Self::IsSymlink => "is-symlink",
                Self::IsBindMount => "is-bind-mount",
                Self::IsEmptyDir => "is-empty-dir",
                Self::IsNonEmptyDir => "is-non-empty-dir",
                Self::IsFile => "is-file",
            }
        )
    }
}

#[derive(Debug, Serialize, PartialEq, Clone)]
pub enum RedirectionState {
    #[serde(rename = "ok")]
    /// Matches the expectations of our configuration as far as we can tell
    MatchesConfiguration,
    #[serde(rename = "unknown-mount")]
    /// Something Mounted that we don't have configuration for
    UnknownMount,
    #[serde(rename = "not-mounted")]
    /// We Expected It To be mounted, but it isn't
    NotMounted,
    #[serde(rename = "symlink-missing")]
    /// We Expected It To be a symlink, but it is not present
    SymlinkMissing,
    #[serde(rename = "symlink-incorrect")]
    /// The Symlink Is Present but points to the wrong place
    SymlinkIncorrect,
}

impl fmt::Display for RedirectionState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                Self::MatchesConfiguration => "ok",
                Self::UnknownMount => "unknown-mount",
                Self::NotMounted => "not-mounted",
                Self::SymlinkMissing => "symlink-missing",
                Self::SymlinkIncorrect => "symlink-incorrect",
            }
        )
    }
}

#[derive(Debug, Serialize)]
pub struct Redirection {
    pub repo_path: PathBuf,
    #[serde(rename = "type")]
    pub redir_type: RedirectionType,
    pub source: String,
    pub state: RedirectionState,
    /// This field is lazily calculated by [`get_effective_redirs_for_mount`].
    pub target: Option<PathBuf>,
}

#[derive(Debug)]
struct ValidatedRepoPath(PathBuf);

impl TryFrom<&Path> for ValidatedRepoPath {
    type Error = EdenFsError;

    fn try_from(path: &Path) -> Result<Self> {
        // Configured keys are kept verbatim, so `./buck-out` reaches here;
        // `components()` only yields `.` as a leading component.
        let normalized: PathBuf = path
            .components()
            .filter(|component| *component != Component::CurDir)
            .collect();
        if normalized.as_os_str().is_empty()
            || normalized
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(EdenFsError::ConfigurationError(format!(
                "redirection path {} must be a non-empty canonical path relative to the repository root",
                path.display()
            )));
        }

        Ok(Self(normalized))
    }
}

impl AsRef<Path> for ValidatedRepoPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug)]
enum BackingCleanupAction {
    ScratchDirectory {
        scratch_root_parent: NoFollowRoot,
        scratch_root_name: PathBuf,
        parent: NoFollowRoot,
        name: PathBuf,
    },
    #[cfg(target_os = "macos")]
    ApfsVolume(eden_apfs::ManagedVolume),
}

/// A fully resolved action for deleting one managed redirection backing target.
#[derive(Debug)]
pub struct RedirectionBackingCleanupAction {
    target: PathBuf,
    action: BackingCleanupAction,
}

impl RedirectionBackingCleanupAction {
    /// Return the resolved target represented by this cleanup action.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Delete this resolved backing target.
    /// A scratch target also takes the checkout's scratch root with it once nothing else is
    /// left there.
    pub fn execute(self) -> Result<()> {
        match self.action {
            BackingCleanupAction::ScratchDirectory {
                scratch_root_parent,
                scratch_root_name,
                parent,
                name,
            } => {
                match parent.remove_dir_all(&name) {
                    Ok(()) => {}
                    // An already-absent target counts as success so an
                    // interrupted cleanup can be retried.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(EdenFsError::Other(anyhow!(
                            "failed to delete redirection backing directory {}: {error}",
                            self.target.display()
                        )));
                    }
                }
                // Windows can't delete a directory while a handle to it is open, and in the
                // default scratch layout `parent` is the scratch root.
                drop(parent);
                if let Err(error) =
                    remove_unused_scratch_root(&scratch_root_parent, &scratch_root_name)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(
                        "failed to delete the unused scratch root of {}: {error}",
                        self.target.display()
                    );
                }
                Ok(())
            }
            #[cfg(target_os = "macos")]
            BackingCleanupAction::ApfsVolume(volume) => {
                match eden_apfs::ApfsUtil::global().delete_managed_volume(&volume)? {
                    eden_apfs::DeleteManagedVolumeOutcome::Deleted
                    | eden_apfs::DeleteManagedVolumeOutcome::NotFound => Ok(()),
                }
            }
        }
    }

    /// Delete everything inside this resolved backing target but keep the target itself, for
    /// a redirection that may still be mounted into the checkout. APFS volumes are kept whole.
    pub fn execute_contents(self) -> Result<()> {
        let contents = match &self.action {
            BackingCleanupAction::ScratchDirectory { parent, name, .. } => parent.open_root(name),
            // The mount point of a volume that can't be unmounted might hold a different volume.
            #[cfg(target_os = "macos")]
            BackingCleanupAction::ApfsVolume(_) => {
                return Err(EdenFsError::Other(anyhow!(
                    "kept APFS volume at {} because it could not be unmounted",
                    self.target.display()
                )));
            }
        };
        let result = match contents {
            Ok(contents) => remove_dir_contents(&contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        result.map_err(|error| {
            EdenFsError::Other(anyhow!(
                "failed to delete the contents of redirection backing target {}: {error}",
                self.target.display()
            ))
        })
    }
}

/// mkscratch writes this into a checkout's scratch root.
const SCRATCH_README: &str = "README.txt";

/// Delete a checkout's scratch root if it holds nothing but mkscratch's README. Other tools
/// can keep their own files there, so any other entry leaves it in place.
fn remove_unused_scratch_root(
    scratch_root_parent: &NoFollowRoot,
    scratch_root_name: &Path,
) -> std::io::Result<()> {
    let entries = scratch_root_parent.list_dir(Some(scratch_root_name))?;
    if entries.iter().any(|entry| entry != SCRATCH_README) {
        return Ok(());
    }
    if !entries.is_empty() {
        scratch_root_parent.remove_file(scratch_root_name.join(SCRATCH_README).as_path())?;
    }
    scratch_root_parent.remove_dir(scratch_root_name)
}

/// Remove every entry in `dir` without following symlinks, attempting all of them before
/// returning the failures.
fn remove_dir_contents(dir: &NoFollowRoot) -> std::io::Result<()> {
    let failures: Vec<String> = dir
        .list_dir(None::<&Path>)?
        .iter()
        .filter_map(|entry| {
            let entry = Path::new(entry);
            let removed = dir.symlink_metadata(Some(entry)).and_then(|metadata| {
                if metadata.is_dir() {
                    dir.remove_dir_all(entry)
                } else {
                    dir.remove_file(entry)
                }
            });
            match removed {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Some(error.to_string())
                }
                _ => None,
            }
        })
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(std::io::Error::other(failures.join("; ")))
    }
}

impl Redirection {
    pub fn repo_path(&self) -> PathBuf {
        self.repo_path.clone()
    }

    /// Determine if the APFS volume helper is installed with appropriate
    /// permissions such that we can use it to mount things
    pub fn have_apfs_helper() -> Result<bool> {
        match std::fs::symlink_metadata(APFS_HELPER) {
            Ok(metadata) => Ok(metadata.is_setuid_set()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
        .from_err()
    }

    /// Read the configured darwin bind redirection implementation without
    /// applying apfs-helper availability fallbacks.
    #[cfg(target_os = "macos")]
    pub fn configured_bind_redirection_type(
        instance: &EdenFsInstance,
    ) -> Result<DarwinBindRedirectionType> {
        instance
            .get_config()
            .map(|config| config.redirections.darwin_redirection_type)
            .and_then(|ty| DarwinBindRedirectionType::from_str(&ty))
    }

    /// Determine what bind redirection type should be used on macOS. There are currently only 2
    /// options: apfs or dmg. We default to the old behavior, apfs.
    #[cfg(target_os = "macos")]
    pub fn determine_bind_redirection_type(instance: &EdenFsInstance) -> DarwinBindRedirectionType {
        match Self::configured_bind_redirection_type(instance) {
            Ok(DarwinBindRedirectionType::SYMLINK) => DarwinBindRedirectionType::SYMLINK,
            Ok(DarwinBindRedirectionType::APFS) if !Self::have_apfs_helper().unwrap_or(false) => {
                eprintln!(
                    "cannot use apfs redirections since apfs_helper '{APFS_HELPER}' is not available. Defaulting to dmg redirections."
                );
                DarwinBindRedirectionType::DMG
            }
            Ok(v) => v,
            Err(e) if Self::have_apfs_helper().unwrap_or(false) => {
                eprintln!("{}. Defaulting to apfs.", e);
                DarwinBindRedirectionType::APFS
            }
            Err(e) => {
                eprintln!("{}. Defaulting to dmg.", e);
                DarwinBindRedirectionType::DMG
            }
        }
    }

    pub fn mkscratch_bin() -> PathBuf {
        // mkscratch is provided by the hg deployment at facebook, which has a
        // different installation prefix on macOS vs Linux, so we need to resolve
        // it via the PATH.  In the integration test environment we'll set the
        // MKSCRATCH_BIN to point to the binary under test
        match std::env::var("MKSCRATCH_BIN") {
            Ok(s) => PathBuf::from(s),
            Err(_) => PathBuf::from("mkscratch"),
        }
    }

    pub fn scratch_subdir() -> PathBuf {
        PathBuf::from("edenfs").join("redirections")
    }

    fn parse_mkscratch_stdout(stdout: &[u8]) -> Result<PathBuf> {
        #[cfg(unix)]
        {
            let path = stdout.strip_suffix(b"\n").unwrap_or(stdout);
            Ok(PathBuf::from(OsStr::from_bytes(path)))
        }
        #[cfg(windows)]
        Ok(PathBuf::from(
            std::str::from_utf8(stdout).from_err()?.trim_end(),
        ))
    }

    #[cfg(unix)]
    fn classify_mkscratch_recovery(
        status: Option<ExitStatus>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    ) -> Result<MkscratchOutput> {
        // None means another thread reaped the child; the marker is still
        // required to prove that the command succeeded.
        if status.is_some_and(|status| !status.success()) {
            return Err(EdenFsError::Other(anyhow!(
                "mkscratch recovery wrapper failed with status {status:?}; an underlying signal may be encoded as 128 + signal; stderr: {}",
                String::from_utf8_lossy(&stderr),
            )));
        }

        let stderr = stderr
            .strip_suffix(MKSCRATCH_SUCCESS_MARKER)
            .ok_or_else(|| {
                EdenFsError::Other(anyhow!(
                    "mkscratch recovery wrapper completed without a success marker; status: {status:?}; stderr: {}",
                    String::from_utf8_lossy(&stderr),
                ))
            })?;
        Ok(MkscratchOutput {
            status: MkscratchExitStatus::RecoveredAfterReap,
            stdout,
            stderr: stderr.to_vec(),
        })
    }

    #[cfg(unix)]
    fn finish_mkscratch_recovery(
        mut read_output: impl FnMut(&mut [Vec<u8>; 2]) -> io::Result<()>,
        mut wait: impl FnMut() -> io::Result<ExitStatus>,
    ) -> Result<MkscratchOutput> {
        // Keep bytes already consumed from the pipes across interrupted reads,
        // including a partially read success marker.
        let mut output = [Vec::new(), Vec::new()];
        loop {
            match read_output(&mut output) {
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                result => {
                    result.from_err()?;
                    break;
                }
            }
        }

        let status = loop {
            match wait() {
                Ok(status) => break Some(status),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => break None,
                Err(error) => return Err(error).from_err(),
            }
        };
        let [stdout, stderr] = output;
        Self::classify_mkscratch_recovery(status, stdout, stderr)
    }

    #[cfg(unix)]
    fn read_mkscratch_output(
        pipes: &mut [Option<File>; 2],
        output: &mut [Vec<u8>; 2],
    ) -> io::Result<()> {
        while pipes.iter().any(Option::is_some) {
            let mut fds = pipes.each_ref().map(|pipe| libc::pollfd {
                fd: pipe.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            });
            // SAFETY: fds is a writable array of the indicated length;
            // pipes owns every nonnegative descriptor throughout poll.
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } == -1 {
                return Err(io::Error::last_os_error());
            }
            for ((pipe, fd), bytes) in pipes.iter_mut().zip(fds).zip(output.iter_mut()) {
                if fd.revents & libc::POLLNVAL != 0 {
                    return Err(io::Error::from_raw_os_error(libc::EBADF));
                }
                if fd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
                    continue;
                }
                let Some(reader) = pipe else {
                    continue;
                };
                // One bounded read per ready pipe prevents either writer
                // from blocking behind a full stdout or stderr pipe.
                let mut buffer = [0; 8192];
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    *pipe = None;
                } else {
                    bytes.extend_from_slice(&buffer[..count]);
                }
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn retry_mkscratch_after_reap(mkscratch: &Path, args: &[&OsStr]) -> Result<MkscratchOutput> {
        // Keep this command eligible for std's posix_spawn path: pre_exec
        // callbacks would run daemon at-fork handlers before the shell exec.
        let mut child = Command::new("/bin/sh")
            .envs(trace_env("mkscratch"))
            .arg("-c")
            .arg(MKSCRATCH_WRAPPER)
            .arg("--")
            .arg(mkscratch)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .from_err()?;
        let mut pipes = [
            child
                .stdout
                .take()
                .map(|pipe| File::from(OwnedFd::from(pipe))),
            child
                .stderr
                .take()
                .map(|pipe| File::from(OwnedFd::from(pipe))),
        ];
        let result = Self::finish_mkscratch_recovery(
            |output| Self::read_mkscratch_output(&mut pipes, output),
            || child.wait(),
        );
        // Child does not reap on drop. Close readers before waiting on an
        // error so the child cannot remain blocked on its output pipes.
        drop(pipes);
        if result.is_err() {
            loop {
                match child.wait() {
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) if error.raw_os_error() != Some(libc::ECHILD) => {
                        tracing::warn!(%error, "failed to reap mkscratch recovery child");
                    }
                    _ => {}
                }
                break;
            }
        }
        result
    }

    fn run_mkscratch(mkscratch: &Path, args: &[&OsStr]) -> Result<MkscratchOutput> {
        let output = Command::new(mkscratch)
            .envs(trace_env("mkscratch"))
            .args(args)
            .output();
        match output {
            Ok(output) => Ok(MkscratchOutput {
                status: MkscratchExitStatus::Exited(output.status),
                stdout: output.stdout,
                stderr: output.stderr,
            }),
            #[cfg(unix)]
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                // `mkscratch path` is idempotent, so retry the operation to
                // recover an exit status instead of trusting output from the
                // child whose status was reaped by another thread.
                Redirection::retry_mkscratch_after_reap(mkscratch, args)
            }
            Err(error) => Err(error).from_err(),
        }
    }

    fn resolve_scratch_dir(
        checkout_path: &Path,
        subdir: &Path,
        no_create: bool,
    ) -> Result<PathBuf> {
        Self::resolve_scratch_path(
            checkout_path,
            Some(&Self::scratch_subdir().join(subdir)),
            no_create,
        )
    }

    fn resolve_scratch_path(
        checkout_path: &Path,
        subdir: Option<&Path>,
        no_create: bool,
    ) -> Result<PathBuf> {
        // This client-library function is also called in the EdenFS daemon by
        // EdenServiceHandler::listRedirections() through redirect_ffi.
        // TODO(zeyi): we can probably embed the logic from mkscratch here directly, without asking the CLI
        let mkscratch = Redirection::mkscratch_bin();
        let mut args = Vec::with_capacity(5);
        if no_create {
            args.push(OsStr::new("--no-create"));
        }
        args.extend([OsStr::new("path"), checkout_path.as_os_str()]);
        if let Some(subdir) = subdir {
            args.extend([OsStr::new("--subdir"), subdir.as_os_str()]);
        }
        let command = || {
            let args: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
            format!(
                "{} {}",
                mkscratch.display(),
                shlex::try_join(args.iter().map(|arg| arg.as_ref()))
                    .unwrap_or_else(|_| "<undecodable arguments>".to_owned()),
            )
        };
        let output = Redirection::run_mkscratch(&mkscratch, &args)
            .with_context(|| format!("Failed to execute mkscratch cmd: `{}`", command()))?;

        match output.status {
            MkscratchExitStatus::Exited(status) if status.success() => {
                Redirection::parse_mkscratch_stdout(&output.stdout)
            }
            MkscratchExitStatus::RecoveredAfterReap => {
                let path = Redirection::parse_mkscratch_stdout(&output.stdout)?;
                tracing::info!(
                    command = %command(),
                    stderr = %String::from_utf8_lossy(&output.stderr),
                    "mkscratch direct exit status was reaped; retry succeeded"
                );
                Ok(path)
            }
            status => Err(EdenFsError::Other(anyhow!(
                "Failed to execute `{}`, stderr: {}, exit status: {:?}",
                command(),
                String::from_utf8_lossy(&output.stderr),
                status,
            ))),
        }
    }

    pub fn expand_target_abspath(
        &self,
        instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
    ) -> Result<Option<PathBuf>> {
        self.resolve_target_abspath(instance, checkout, true)
    }

    fn ensure_target_abspath(
        &self,
        instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
    ) -> Result<Option<PathBuf>> {
        self.resolve_target_abspath(instance, checkout, false)
    }

    fn resolve_target_abspath(
        &self,
        instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
        no_create: bool,
    ) -> Result<Option<PathBuf>> {
        match self.redir_type {
            RedirectionType::Unknown => Ok(None),
            RedirectionType::Bind | RedirectionType::Symlink
                if self.uses_checkout_path_as_target(instance, checkout) =>
            {
                Ok(Some(checkout.path().join(&self.repo_path)))
            }
            RedirectionType::Bind | RedirectionType::Symlink => Ok(Some(
                Self::resolve_scratch_dir(&checkout.path(), &self.repo_path, no_create)?,
            )),
        }
    }

    #[cfg(target_os = "macos")]
    fn uses_checkout_path_as_target(
        &self,
        instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
    ) -> bool {
        self.redir_type == RedirectionType::Bind
            && !self.repo_path_is_symlink(checkout)
            && Self::determine_bind_redirection_type(instance) == DarwinBindRedirectionType::APFS
    }

    #[cfg(not(target_os = "macos"))]
    fn uses_checkout_path_as_target(
        &self,
        _instance: &EdenFsInstance,
        _checkout: &EdenFsCheckout,
    ) -> bool {
        false
    }

    #[cfg(target_os = "macos")]
    fn repo_path_is_symlink(&self, checkout: &EdenFsCheckout) -> bool {
        std::fs::symlink_metadata(self.expand_repo_path(checkout))
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    }

    fn _dmg_file_name(&self, target: &Path) -> PathBuf {
        target.join("image.dmg.sparseimage")
    }

    #[cfg(target_os = "linux")]
    async fn _bind_mount_linux(
        &self,
        instance: &EdenFsInstance,
        checkout_path: &Path,
        target: &Path,
    ) -> Result<()> {
        let daemon = instance.daemon();
        let abs_mount_path_in_repo = checkout_path.join(&self.repo_path);
        if abs_mount_path_in_repo.exists() {
            // To deal with the case where someone has manually unmounted
            // a bind mount and left the privhelper confused about the
            // list of bind mounts, we first speculatively try asking the
            // eden daemon to unmount it first, ignoring any error that
            // might raise.
            daemon
                .remove_bind_mount(checkout_path, &self.repo_path)
                .await
                .ok();
        }
        // Ensure that the client directory exists before we try to mount over it
        std::fs::create_dir_all(target)
            .from_err()
            .with_context(|| format!("Failed to create directory {}", target.display()))?;
        std::fs::create_dir_all(&abs_mount_path_in_repo)
            .from_err()
            .with_context(|| {
                format!(
                    "Failed to create directory {}",
                    abs_mount_path_in_repo.display()
                )
            })?;
        daemon
            .add_bind_mount(checkout_path, &self.repo_path, target)
            .await
            .with_context(|| {
                format!(
                    "add_bind_mount thrift call failed for target '{}' in checkout '{}'",
                    target.display(),
                    checkout_path.display()
                )
            })?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    /// Attempt to use an APFS volume for a bind redirection.
    /// The heavy lifting is part of the APFS_HELPER utility found
    /// in `eden/scm/exec/eden_apfs_mount_helper/`
    fn _bind_mount_darwin_apfs(&self, checkout_path: &Path) -> Result<()> {
        let mount_path = checkout_path.join(&self.repo_path);
        std::fs::create_dir_all(&mount_path)
            .from_err()
            .with_context(|| format!("Failed to create directory {}", &mount_path.display()))?;
        let args = &["mount", &mount_path.to_string_lossy()];
        let output = Command::new(APFS_HELPER)
            .envs(trace_env("apfs_helper"))
            .args(args)
            .output()
            .from_err()
            .with_context(|| {
                format!(
                    "Failed to execute command `{} {}`",
                    APFS_HELPER,
                    shlex::try_join(args.iter().copied()).unwrap(), // Unwrap OK, we know the args are valid
                )
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(EdenFsError::Other(anyhow!(
                "failed to add bind mount for mount {}. stderr: {}\n stdout: {}",
                checkout_path.display(),
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            )))
        }
    }

    #[cfg(target_os = "macos")]
    fn _bind_mount_darwin_dmg(&self, checkout_path: &Path, target: &Path) -> Result<()> {
        // Since we don't have bind mounts, we set up a disk image file
        // and mount that instead.
        let image_file_path = self._dmg_file_name(target);
        let target_stat = disk_usage(target)
            .from_err()
            .with_context(|| format!("Failed to stat target {}", target.display()))?;

        // Specify the size in kb because the disk utilities have weird
        // defaults if the units are unspecified, and `b` doesn't mean
        // bytes!
        let total_kib = target_stat.total() / 1024;
        let mount_path = checkout_path.join(self.repo_path());

        // We need to convert paths -> strings for the hdiutil commands
        let image_file_name = image_file_path.to_string_lossy();
        let mount_name = mount_path.to_string_lossy();

        if !image_file_path.exists() {
            let image_file_dir = image_file_path.parent().with_context(|| {
                format!(
                    "image file {} must exist in some parent directory",
                    &image_file_path.display()
                )
            })?;
            if !image_file_dir.exists() {
                std::fs::create_dir_all(image_file_dir)
                    .from_err()
                    .with_context(|| {
                        format!("Failed to create directory {}", &image_file_dir.display())
                    })?;
            }
            let args = &[
                "create",
                "-size",
                &format!("{}k", total_kib),
                "-type",
                "SPARSE",
                "-fs",
                "HFS+",
                "-volname",
                &format!("'EdenFS redirection for {}'", &mount_name),
                &image_file_name,
            ];
            let create_output = Command::new("hdiutil")
                .envs(trace_env("hdiutil"))
                .args(args)
                .output()
                .from_err()
                .with_context(|| {
                    format!(
                        "Failed to execute command `hdiutil {}`",
                        shlex::try_join(args.iter().copied()).unwrap(), // Unwrap OK, we know the args are valid
                    )
                })?;
            if !create_output.status.success() {
                return Err(EdenFsError::Other(anyhow!(
                    "failed to create dmg volume {} for mount {}. stderr: {}\n stdout: {}",
                    &image_file_name,
                    &mount_name,
                    String::from_utf8_lossy(&create_output.stderr),
                    String::from_utf8_lossy(&create_output.stdout)
                )));
            }
        }
        let args = &[
            "attach",
            &image_file_name,
            "-nobrowse",
            "-mountpoint",
            &mount_name,
        ];
        let mount_path = mount_path.parent().with_context(|| {
            format!(
                "mount path {} must exist in some parent directory",
                &mount_path.display()
            )
        })?;
        if !mount_path.exists() {
            std::fs::create_dir_all(mount_path)
                .from_err()
                .with_context(|| format!("Failed to create directory {}", &mount_path.display()))?;
        }
        let attach_output = Command::new("hdiutil")
            .envs(trace_env("hdiutil"))
            .args(args)
            .output()
            .from_err()
            .with_context(|| {
                format!(
                    "Failed to execute command `hdiutil {}`",
                    shlex::try_join(args.iter().copied()).unwrap(), // Unwrap OK, we know the args are valid
                )
            })?;
        if !attach_output.status.success() {
            return Err(EdenFsError::Other(anyhow!(
                "failed to attach dmg volume {} for mount {}. stderr: {}\n stdout: {}",
                &image_file_name,
                &mount_name,
                String::from_utf8_lossy(&attach_output.stderr),
                String::from_utf8_lossy(&attach_output.stdout)
            )));
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn _bind_mount_darwin(
        &self,
        instance: &EdenFsInstance,
        checkout_path: &Path,
        target: &Path,
    ) -> Result<()> {
        // We default to APFS since DMG redirections are experimental at this point
        if Self::determine_bind_redirection_type(instance) == DarwinBindRedirectionType::SYMLINK {
            self._apply_symlink(checkout_path, target, false)
        } else if Self::determine_bind_redirection_type(instance) == DarwinBindRedirectionType::DMG
        {
            self._bind_mount_darwin_dmg(checkout_path, target)
        } else {
            self._bind_mount_darwin_apfs(checkout_path)
        }
    }

    #[cfg(target_os = "windows")]
    fn _bind_mount_windows(&self, checkout_path: &Path, target: &Path, force: bool) -> Result<()> {
        self._apply_symlink(checkout_path, target, force)
    }

    #[cfg(target_os = "linux")]
    async fn _bind_mount(
        &self,
        instance: &EdenFsInstance,
        checkout: &Path,
        target: &Path,
        _force: bool,
    ) -> Result<()> {
        self._bind_mount_linux(instance, checkout, target).await
    }

    #[cfg(target_os = "macos")]
    async fn _bind_mount(
        &self,
        instance: &EdenFsInstance,
        checkout: &Path,
        target: &Path,
        _force: bool,
    ) -> Result<()> {
        self._bind_mount_darwin(instance, checkout, target)
    }

    #[cfg(target_os = "windows")]
    async fn _bind_mount(
        &self,
        _instance: &EdenFsInstance,
        checkout: &Path,
        target: &Path,
        force: bool,
    ) -> Result<()> {
        self._bind_mount_windows(checkout, target, force)
    }

    #[cfg(all(not(unix), not(windows)))]
    async fn _bind_mount(&self, checkout: &Path, target: &Path) -> Result<()> {
        Err(EdenFsError::Other(anyhow!(
            "Could not complete bind mount: unsupported platform"
        )))
    }

    #[cfg(target_os = "linux")]
    async fn _bind_unmount_linux(
        &self,
        instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
    ) -> Result<()> {
        instance
            .daemon()
            .remove_bind_mount(&checkout.path(), &self.repo_path)
            .await
            .with_context(|| {
                format!(
                    "remove_bind_mount thrift call failed for '{}' in checkout '{}'",
                    self.repo_path.display(),
                    checkout.path().display()
                )
            })?;
        Ok(())
    }

    pub fn expand_repo_path(&self, checkout: &EdenFsCheckout) -> PathBuf {
        checkout.path().join(&self.repo_path)
    }

    #[cfg(target_os = "macos")]
    fn _bind_unmount_darwin(&self, checkout: &EdenFsCheckout) -> Result<()> {
        let mount_path = checkout.path().join(&self.repo_path);
        // Only reached for paths that are real mounts: remove_existing unlinks
        // symlink-backed redirections based on disposition analysis first.
        //
        // We use unmount instead of eject here since eject has caused issues
        // by unmounting unrelated apfs volumes in the past. See S325232.
        let args = &["unmount", "force", &mount_path.to_string_lossy()];
        let output = Command::new("diskutil")
            .envs(trace_env("diskutil"))
            .args(args)
            .output()
            .from_err()
            .with_context(|| {
                format!(
                    "Failed to execute command `diskutil {}`",
                    shlex::try_join(args.iter().copied()).unwrap(), // Unwrap OK, we know the args are valid
                )
            })?;
        if !output.status.success() {
            return Err(EdenFsError::Other(anyhow!(format!(
                "failed to remove bind mount. stderr: {}\n stdout: {}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            ))));
        }
        Ok(())
    }

    #[cfg(target_os = "windows")]
    fn _bind_unmount_windows(&self, checkout: &EdenFsCheckout) -> Result<()> {
        let repo_path = self.expand_repo_path(checkout);
        remove_symlink(&repo_path)
            .with_context(|| format!("Failed to remove symlink {}", repo_path.display()))?;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    async fn _bind_unmount(
        &self,
        _instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
    ) -> Result<()> {
        self._bind_unmount_windows(checkout)
    }

    #[cfg(target_os = "macos")]
    async fn _bind_unmount(
        &self,
        _instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
    ) -> Result<()> {
        self._bind_unmount_darwin(checkout)
    }

    #[cfg(target_os = "linux")]
    async fn _bind_unmount(
        &self,
        instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
    ) -> Result<()> {
        self._bind_unmount_linux(instance, checkout).await
    }

    /// Attempts to create a symlink at checkout_path/self.repo_path that points to target.
    /// This will fail if checkout_path/self.repo_path already exists
    #[allow(unused)] // Force is only used in Windows
    fn _apply_symlink(&self, checkout_path: &Path, target: &Path, force: bool) -> Result<()> {
        let symlink_path = checkout_path.join(&self.repo_path);

        // If .parent() resolves to None or parent().exists() == true, we skip directory creation
        if !symlink_path.parent().is_none_or(|parent| parent.exists()) {
            symlink_path.parent().map(std::fs::create_dir_all);
        }

        #[cfg(not(windows))]
        std::os::unix::fs::symlink(target, &symlink_path)
            .from_err()
            .with_context(|| {
                format!(
                    "Failed to create symlink {} with target {}",
                    symlink_path.display(),
                    target.display()
                )
            })?;

        #[cfg(windows)]
        {
            // Creating a symlink on Windows is non-atomic, and thus when EdenFS
            // gets the notification about a file being created and then goes on
            // testing what's on disk, it may either find a symlink, or a directory.
            //
            // This is bad for EdenFS for a number of reason. The main one being
            // that EdenFS will attempt to recursively add all the childrens of
            // that directory to the inode hierarchy. If the symlinks points to
            // a very large directory, this can be extremely slow, leading to a
            // very poor user experience.
            //
            // Since these symlinks are created for redirections, we can expect
            // the above to be true.
            //
            // To fix this in a generic way is hard to impossible. One of the
            // approach would be to hack in the PrjfsDispatcherImpl.cpp and
            // sleep a bit when we detect a directory, to make sure that we
            // retest it if this was a symlink. This wouldn't work if the system
            // is overloaded, and it would add a small delay to update/status
            // operation due to these waiting on all pending notifications to be
            // handled.
            //
            // Instead, we chose here to handle it in a local way by forcing the
            // redirection to be created atomically. We first create the symlink
            // in the parent directory of the repository, and then move it
            // inside, which is atomic.
            let repo_and_symlink_path = checkout_path.join(&self.repo_path);
            if let Some(temp_symlink_path) = checkout_path
                .parent()
                .map(|co_parent| co_parent.join(zzencode(&repo_and_symlink_path.to_string_lossy())))
            {
                // These temp files should be created by EdenFS only, let's just remove it if it's there.
                // `is_dir()` will be true for symlink pointing to a directory
                if temp_symlink_path.exists() && temp_symlink_path.is_dir() {
                    // In Windows, symlink pointing to a dir can only be deleted using `remove_dir()`
                    // Refer: https://stackoverflow.com/a/76407261
                    std::fs::remove_dir(&temp_symlink_path)
                        .from_err()
                        .with_context(|| {
                            format!(
                                "Failed to remove temp redirection symlink file {}",
                                temp_symlink_path.display()
                            )
                        })?;
                }
                std::os::windows::fs::symlink_dir(target, &temp_symlink_path)
                    .from_err()
                    .with_context(|| {
                        format!(
                            "Failed to create symlink {} with target {}",
                            &temp_symlink_path.display(),
                            target.display()
                        )
                    })?;
                std::fs::rename(&temp_symlink_path, &symlink_path)
                    .from_err()
                    .with_context(|| {
                        format!(
                            "Failed to rename symlink {} to {}",
                            &temp_symlink_path.display(),
                            &symlink_path.display()
                        )
                    })?;
            } else {
                return Err(EdenFsError::Other(anyhow!(
                    "failed to create symlink for {}",
                    self.repo_path.display()
                )));
            }
        }
        Ok(())
    }

    fn _is_deletable_path(&self, instance: &EdenFsInstance, path: &Path) -> bool {
        let deletable_paths = instance.get_config().map_or_else(
            |_| Vec::new(),
            |config| config.redirections.redirect_fixup_deletable_paths,
        );

        let is_deletable_path = deletable_paths.contains(&path.display().to_string());
        if is_deletable_path {
            println!(
                "`{}` is a path that should only have auto-generated content hence should be safe to delete it to recover your redirections.
If this path should not be deleted automatically, please reach out to 'EdenFS Windows Users' (https://fb.workplace.com/groups/edenfswindows) to correct this.",
                path.display()
            );
        }

        is_deletable_path
    }

    fn _handle_non_empty_dir(
        &self,
        instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
        force_remove: bool,
        cli_name: &str,
    ) -> Result<RepoPathDisposition> {
        if force_remove || self._is_deletable_path(instance, &self.repo_path) {
            println!(
                "Redirection path found to be a non-empty directory. Attempting to remove this directory and its content."
            );
            match forcefully_remove_dir_all(&self.expand_repo_path(checkout)) {
                Ok(_) => Ok(RepoPathDisposition::DoesNotExist),
                Err(e) => {
                    println!("System error occurred while removing directory: {e}");
                    Err(EdenFsError::Other(anyhow!(
                        "Failed to delete a non-empty directory (full path `{}`).
This happens mostly when some of its files are in use by another process.
To detect and kill such processes, follow https://fburl.com/edenfs-redirection-non-empty-directory.",
                        self.expand_repo_path(checkout).display()
                    )))
                }
            }
        } else {
            Err(EdenFsError::Other(anyhow!(
                "A non-empty directory (full path `{}`) found. Either-
- Try again after reviewing and manually deleting the directory, or
- Run `eden redirect {} --force` with relevant params (if any) to attempt inline deletion of the directory if none of its files are in use.",
                self.expand_repo_path(checkout).display(),
                cli_name
            )))
        }
    }

    fn _handle_file_repo_path(
        &self,
        instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
        force_remove: bool,
        cli_name: &str,
    ) -> Result<RepoPathDisposition> {
        if force_remove || self._is_deletable_path(instance, &self.repo_path) {
            println!("Redirection path found to be a file. Attempting to remove this file.");
            match remove_file(&self.expand_repo_path(checkout)) {
                Ok(_) => Ok(RepoPathDisposition::DoesNotExist),
                Err(e) => {
                    println!("System error occurred while removing file: {e}");
                    Err(EdenFsError::Other(anyhow!(
                        "Failed to delete the file (full path `{}`).
This happens mostly when the file is being used by another process.
To detect and kill such processes, follow https://fburl.com/edenfs-redirection-non-empty-directory.",
                        self.expand_repo_path(checkout).display()
                    )))
                }
            }
        } else {
            Err(EdenFsError::Other(anyhow!(
                "Redirection path found to be a file (full path `{}`). Either-
- Try again after reviewing and manually deleting the file, or
- Run `eden redirect {} --force` with relevant params (if any) to attempt inline deletion of the file if it is not in use by another process.",
                self.expand_repo_path(checkout).display(),
                cli_name
            )))
        }
    }

    #[async_recursion]
    pub async fn remove_existing(
        &self,
        instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
        fail_if_bind_mount: bool,
        force_remove: bool,
        cli_name: &str,
    ) -> Result<RepoPathDisposition> {
        let repo_path = self.expand_repo_path(checkout);
        let disposition = RepoPathDisposition::analyze(&repo_path)
            .with_context(|| format!("Failed to analyze path {}", repo_path.display()))?;
        if disposition == RepoPathDisposition::DoesNotExist {
            return Ok(disposition);
        }

        if disposition == RepoPathDisposition::IsSymlink {
            remove_symlink(&repo_path)
                .with_context(|| format!("Failed to remove symlink {}", repo_path.display()))?;
            return Ok(RepoPathDisposition::DoesNotExist);
        }

        if disposition == RepoPathDisposition::IsBindMount {
            if fail_if_bind_mount {
                return Err(EdenFsError::Other(anyhow!(
                    "Failed to remove bind mount {}",
                    repo_path.display()
                )));
            }
            self._bind_unmount(instance, checkout)
                .await
                .with_context(|| {
                    format!("Failed to unmount bind mount {}", self.repo_path.display())
                })?;

            // Now that it is unmounted, re-assess and ideally
            // remove the empty directory that was the mount point
            // To avoid infinite recursion, tell the next call to fail if
            // the disposition is still a bind mount
            return self
                .remove_existing(instance, checkout, true, force_remove, cli_name)
                .await;
        }

        if disposition == RepoPathDisposition::IsEmptyDir {
            match std::fs::remove_dir(repo_path) {
                Ok(_) => return Ok(RepoPathDisposition::DoesNotExist),
                Err(_) => return Ok(disposition),
            }
        }

        if self.redir_type == RedirectionType::Symlink
            || (self.redir_type == RedirectionType::Bind && cfg!(windows))
        {
            if disposition == RepoPathDisposition::IsNonEmptyDir {
                return self._handle_non_empty_dir(instance, checkout, force_remove, cli_name);
            }

            if disposition == RepoPathDisposition::IsFile {
                return self._handle_file_repo_path(instance, checkout, force_remove, cli_name);
            }
        }

        Ok(disposition)
    }

    /// Detach this redirection before deleting its backing target.
    pub async fn unmount_for_backing_cleanup(
        &self,
        instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
    ) -> Result<()> {
        let repo_path = self.expand_repo_path(checkout);
        #[cfg(target_os = "linux")]
        {
            // A leftover bind on an inactive checkout can share its parent's st_dev.
            // Covered mount entries also prevent deletion until they are detached.
            unmount_and_verify(
                &repo_path,
                || {
                    let path = repo_path.clone();
                    async move {
                        tokio::task::spawn_blocking(move || is_mount_point(&path))
                            .await
                            .from_err()?
                    }
                },
                self._bind_unmount(instance, checkout),
            )
            .await?;
        }
        // Leftover content at the repo path is not a mount, so it can't keep the backing in use.
        let disposition = RepoPathDisposition::analyze(&repo_path)
            .with_context(|| format!("Failed to analyze path {}", repo_path.display()))?;
        if matches!(
            disposition,
            RepoPathDisposition::IsNonEmptyDir | RepoPathDisposition::IsFile
        ) {
            return Ok(());
        }
        self.remove_existing(instance, checkout, false, false, "fixup")
            .await?;
        Ok(())
    }

    /// Request a bind unmount without inspecting mounts or removing checkout paths.
    /// The caller must verify the batch with fresh mount state before deleting backing targets.
    #[cfg(target_os = "linux")]
    pub async fn detach_for_backing_cleanup(
        &self,
        instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
    ) -> Result<()> {
        self._bind_unmount(instance, checkout).await
    }

    pub async fn apply(
        &self,
        instance: &EdenFsInstance,
        checkout: &EdenFsCheckout,
        force: bool,
        cli_name: &str,
    ) -> Result<()> {
        let disposition = match self
            .remove_existing(instance, checkout, false, force, cli_name)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                return Err(EdenFsError::Other(anyhow!(
                    "Failed to remove existing redirection `{}`.\nReason- {}",
                    self.repo_path.display(),
                    e
                )));
            }
        };

        if disposition == RepoPathDisposition::IsFile {
            return Err(EdenFsError::Other(anyhow!(
                "Cannot redirect {} because it is a file",
                self.repo_path.display()
            )));
        }

        if self.redir_type == RedirectionType::Bind {
            let target = self.ensure_target_abspath(instance, checkout)?;
            match target {
                Some(t) => {
                    self._bind_mount(instance, &checkout.path(), &t, force)
                        .await
                }
                None => Err(EdenFsError::Other(anyhow!(
                    "failed to expand target abspath for checkout {}",
                    checkout.path().display()
                ))),
            }
        } else if self.redir_type == RedirectionType::Symlink {
            let target = self
                .ensure_target_abspath(instance, checkout)
                .with_context(|| {
                    format!(
                        "Failed to expand abspath for target {} in checkout {}",
                        self.target
                            .as_ref()
                            .unwrap_or(&PathBuf::from("DoesNotExist"))
                            .display(),
                        checkout.path().display()
                    )
                })?;
            match target {
                Some(t) => self._apply_symlink(&checkout.path(), &t, force),
                None => Err(EdenFsError::Other(anyhow!(
                    "failed to expand target abspath for checkout {}",
                    checkout.path().display()
                ))),
            }
        } else {
            Err(EdenFsError::Other(anyhow!(
                "Unsupported redirection type {}",
                self.redir_type
            )))
        }
    }
}

/// Detect the most common form of a bind mount in the repo;
/// its parent directory will have a different device number than
/// the mount point itself.  This won't detect something funky like
/// bind mounting part of the repo to a different part.
pub(crate) fn is_bind_mount(path: PathBuf) -> Result<bool> {
    let parent = path.parent();
    if let Some(parent_path) = parent {
        let path_metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
        .from_err()
        .with_context(|| format!("Failed to get symlink metadata for path {}", path.display()))?;
        let parent_metadata = match std::fs::symlink_metadata(parent_path) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
        .from_err()
        .with_context(|| {
            format!(
                "Failed to get symlink metadata for path {}",
                parent_path.display()
            )
        })?;

        match (path_metadata, parent_metadata) {
            (Some(m1), Some(m2)) => Ok(m1.eden_dev() != m2.eden_dev()),
            _ => Ok(false),
        }
    } else {
        Ok(false)
    }
}

pub fn is_empty_dir(path: &Path) -> Result<bool> {
    let mut dir_iter = path
        .read_dir()
        .with_context(|| anyhow!("failed to read directory {}", path.display()))?;
    // read_dir returns a directory iter that skips . and ..
    // Therefore, if .next() -> None, we know the dir is empty
    Ok(dir_iter.next().is_none())
}

#[derive(Deserialize)]
struct RedirectionsConfigInner {
    #[serde(flatten, deserialize_with = "deserialize_redirections")]
    redirections: BTreeMap<PathBuf, RedirectionType>,
}

#[derive(Deserialize)]
struct RedirectionsConfig {
    #[serde(rename = "redirections")]
    inner: RedirectionsConfigInner,
}

pub(crate) fn deserialize_redirections<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<PathBuf, RedirectionType>, D::Error>
where
    D: Deserializer<'de>,
{
    let unvalidated_map: BTreeMap<String, Value> = BTreeMap::deserialize(deserializer)?;
    let mut map = BTreeMap::new();
    for (key, value) in unvalidated_map {
        if let Some(s) = value.as_str() {
            map.insert(
                PathBuf::from(
                    // Convert path separator to backslash on Windows
                    if cfg!(windows) {
                        key.replace("\\\\", "\\").replace('/', "\\")
                    } else {
                        key
                    },
                ),
                RedirectionType::from_str(s).map_err(serde::de::Error::custom)?,
            );
        } else {
            return Err(serde::de::Error::custom(format!(
                "Unsupported redirection value type {value}. Must be string."
            )));
        }
    }

    Ok(map)
}

/// Returns the explicitly configured redirection configuration.
/// This does not take into account how things are currently mounted;
/// use `get_effective_redirections` for that purpose.
pub fn get_configured_redirections(
    checkout: &EdenFsCheckout,
) -> Result<BTreeMap<PathBuf, Redirection>> {
    let mut redirs = BTreeMap::new();

    // Repo-specified settings have the lowest level of precedence
    let repo_redirection_config_file_name = checkout.path().join(".eden-redirections");
    if let Ok(contents) = std::fs::read(repo_redirection_config_file_name) {
        let s = String::from_utf8(contents).from_err()?;
        let config: RedirectionsConfig = toml::from_str(&s)
            .from_err()
            .with_context(|| format!("Failed to create RedirectionsConfig from str '{s}'"))?;
        for (repo_path, redir_type) in config.inner.redirections {
            redirs.insert(
                repo_path.clone(),
                Redirection {
                    repo_path,
                    redir_type,
                    target: None,
                    source: REPO_SOURCE.to_string(),
                    state: RedirectionState::MatchesConfiguration,
                },
            );
        }
    }

    // User-specific things have the highest precedence
    if let Some(user_redirs) = &checkout.redirections {
        for (repo_path, redir_type) in user_redirs {
            let target = checkout
                .redirection_targets
                .as_ref()
                .and_then(|targets| targets.get(repo_path));
            redirs.insert(
                repo_path.clone(),
                Redirection {
                    repo_path: repo_path.clone(),
                    redir_type: *redir_type,
                    target: target.cloned(),
                    source: USER_REDIRECTION_SOURCE.to_string(),
                    state: RedirectionState::MatchesConfiguration,
                },
            );
        }
    }

    Ok(redirs)
}

fn is_symlink_correct(
    instance: &EdenFsInstance,
    redir: &Redirection,
    checkout: &EdenFsCheckout,
) -> Result<bool> {
    if let Some(expected_target) = redir
        .expand_target_abspath(instance, checkout)
        .with_context(|| {
            format!(
                "Failed to expand abspath for target {} in checkout {}",
                redir
                    .target
                    .as_ref()
                    .unwrap_or(&PathBuf::from("DoesNotExist"))
                    .display(),
                checkout.path().display()
            )
        })?
    {
        let expected_target = std::fs::canonicalize(&expected_target)
            .from_err()
            .with_context(|| {
                format!("Failed to canonicalize path {}", expected_target.display())
            })?;
        let symlink_path = checkout.path().join(&redir.repo_path);
        let target_path = std::fs::read_link(&symlink_path).with_context(|| {
            format!("Failed to read link for symlink {}", symlink_path.display())
        })?;
        let target = std::fs::canonicalize(&target_path)
            .from_err()
            .with_context(|| format!("Failed to canonicalize path {}", target_path.display()))?;
        Ok(target == expected_target)
    } else {
        Ok(false)
    }
}

// Returns the complete set of effective redirections for a given mount path,
// and expands redirection target paths. Useful for listing redirections.
pub fn get_effective_redirs_for_mount(
    instance: &EdenFsInstance,
    mount: PathBuf,
) -> Result<BTreeMap<PathBuf, Redirection>> {
    let checkout = find_checkout(instance, &mount)?;
    let mut redirections = get_effective_redirections(instance, &checkout).with_context(|| {
        anyhow!(
            "Unable to retrieve redirections for checkout '{}'",
            mount.display()
        )
    })?;

    redirections
        .values_mut()
        .try_for_each(|redir| -> Result<()> {
            redir.target = redir
                .expand_target_abspath(instance, &checkout)
                .with_context(|| {
                    format!(
                        "Failed to expand target abspath for redirection: {}",
                        redir.repo_path.display()
                    )
                })?;
            Ok(())
        })
        .with_context(|| anyhow!("failed to expand redirection target path"))?;

    Ok(redirections)
}

fn scratch_cleanup_action(
    scratch_root: &Path,
    target: PathBuf,
) -> Result<Option<RedirectionBackingCleanupAction>> {
    let relative_target = target.strip_prefix(scratch_root).with_context(|| {
        format!(
            "scratch target {} is outside scratch root {}",
            target.display(),
            scratch_root.display()
        )
    })?;
    let relative_target = ValidatedRepoPath::try_from(relative_target)?;
    let root_parent = scratch_root.parent().filter(|_| scratch_root.is_absolute());
    let (Some(root_parent), Some(root_name)) = (root_parent, scratch_root.file_name()) else {
        return Err(EdenFsError::ConfigurationError(format!(
            "invalid scratch root {}",
            scratch_root.display()
        )));
    };

    let relative_target = Path::new(root_name).join(relative_target.as_ref());
    let name = PathBuf::from(
        relative_target
            .file_name()
            .expect("validated target has a name"),
    );
    let open_parents = || -> std::io::Result<(NoFollowRoot, NoFollowRoot)> {
        // Only the configured scratch prefix may traverse aliases. Keep the
        // target's real parent open across checkout removal.
        let scratch_root_parent = NoFollowRoot::new(root_parent)?;
        let parent = scratch_root_parent.open_root(
            relative_target
                .parent()
                .expect("target is below scratch root"),
        )?;
        if !parent.symlink_metadata(Some(&name))?.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "scratch target must be a directory, not a symlink or file",
            ));
        }
        Ok((scratch_root_parent, parent))
    };
    let (scratch_root_parent, parent) = match open_parents() {
        Ok(parents) => parents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(EdenFsError::Other(anyhow!(
                "failed to open scratch target {} without following symlinks: {error}",
                target.display()
            )));
        }
    };
    Ok(Some(RedirectionBackingCleanupAction {
        target,
        action: BackingCleanupAction::ScratchDirectory {
            scratch_root_parent,
            scratch_root_name: PathBuf::from(root_name),
            parent,
            name,
        },
    }))
}

/// Observe a batch without retaining mount state across unmount operations.
#[cfg(target_os = "linux")]
pub fn redirection_mount_status(paths: &[PathBuf]) -> Result<Vec<bool>> {
    let mut mounts = MountTableSnapshot::default();
    paths
        .iter()
        .map(|path| mounts.is_mount_point(path))
        .collect()
}

/// Remove an unmounted redirection's symlink or empty directory, leaving other content alone.
/// Call only after checking fresh mount state. This does not unmount anything.
#[cfg(target_os = "linux")]
pub fn finish_backing_cleanup_unmount(path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).from_err(),
    };
    if metadata.is_symlink() {
        remove_symlink(path).from_err()?;
    } else if metadata.is_dir() {
        // rmdir also rejects a mount that appeared since verification, including a
        // same-filesystem bind. Never recurse through checkout-side content here.
        match std::fs::remove_dir(path) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => return Err(error).from_err(),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn unmount_and_verify<Probe>(
    repo_path: &Path,
    mut is_mounted: impl FnMut() -> Probe,
    unmount: impl Future<Output = Result<()>>,
) -> Result<()>
where
    Probe: Future<Output = Result<bool>>,
{
    if is_mounted().await? {
        unmount.await?;
        if is_mounted().await? {
            return Err(EdenFsError::Other(anyhow!(
                "redirection {} is still mounted after unmount",
                repo_path.display()
            )));
        }
    }
    Ok(())
}

/// Plans backing cleanup while sharing a lazy mount-table snapshot across calls.
/// Drop the planner before changing mounts; unmount verification needs fresh state.
pub struct RedirectionBackingCleanupPlanner<'a> {
    checkout_path: &'a Path,
    scratch_root: Option<PathBuf>,
    #[cfg(target_os = "linux")]
    mounts: MountTableSnapshot,
}

impl<'a> RedirectionBackingCleanupPlanner<'a> {
    /// Create a planner for one checkout's planning batch without reading mounts.
    pub fn new(checkout_path: &'a Path) -> Self {
        Self {
            checkout_path,
            scratch_root: None,
            #[cfg(target_os = "linux")]
            mounts: MountTableSnapshot::default(),
        }
    }

    fn resolve_scratch_root(
        &mut self,
        resolve: impl FnOnce(&Path) -> Result<PathBuf>,
    ) -> Result<PathBuf> {
        if let Some(root) = &self.scratch_root {
            return Ok(root.clone());
        }
        let root = resolve(self.checkout_path)?;
        self.scratch_root = Some(root.clone());
        Ok(root)
    }

    fn resolve_scratch_cleanup_target(
        &mut self,
        repo_path: &ValidatedRepoPath,
        recorded_target: Option<&Path>,
        resolved_target: &Path,
    ) -> Result<PathBuf> {
        let checkout_path = self.checkout_path;
        if !resolved_target.is_absolute()
            || resolved_target
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(EdenFsError::ConfigurationError(format!(
                "resolved scratch target {} must be an absolute canonical path",
                resolved_target.display()
            )));
        }
        let Some(recorded_target) = recorded_target.filter(|target| !target.as_os_str().is_empty())
        else {
            let checkout_target = checkout_path.join(repo_path.as_ref());
            match std::fs::symlink_metadata(&checkout_target) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let target = std::fs::read_link(&checkout_target).from_err()?;
                    let target = checkout_target
                        .parent()
                        .expect("validated repo path has a parent")
                        .join(target);
                    return self.resolve_scratch_cleanup_target(
                        repo_path,
                        Some(&target),
                        resolved_target,
                    );
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).from_err(),
            }
            #[cfg(target_os = "linux")]
            if self.mounts.is_mount_point(&checkout_target)? {
                let installed = std::fs::metadata(&checkout_target).from_err()?;
                let resolved = std::fs::metadata(resolved_target).from_err().with_context(|| {
                    format!(
                        "cannot validate installed redirection {} against resolved scratch target {}",
                        checkout_target.display(),
                        resolved_target.display()
                    )
                })?;
                if (installed.st_dev(), installed.st_ino())
                    != (resolved.st_dev(), resolved.st_ino())
                {
                    return Err(EdenFsError::ConfigurationError(format!(
                        "installed redirection {} does not match resolved managed scratch target {}",
                        checkout_target.display(),
                        resolved_target.display()
                    )));
                }
            }
            return Ok(resolved_target.to_path_buf());
        };
        if !recorded_target.is_absolute()
            || recorded_target
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(EdenFsError::ConfigurationError(format!(
                "recorded redirection target {} must be an absolute canonical path",
                recorded_target.display()
            )));
        }

        let checkout_target = checkout_path.join(repo_path.as_ref());
        if recorded_target == checkout_target {
            return Ok(resolved_target.to_path_buf());
        }
        let canonical_checkout = std::fs::canonicalize(checkout_path)
            .from_err()
            .with_context(|| {
                format!(
                    "failed to canonicalize checkout path {}",
                    checkout_path.display()
                )
            })?;
        match std::fs::canonicalize(recorded_target) {
            Ok(canonical_target)
                if canonical_target == canonical_checkout.join(repo_path.as_ref()) =>
            {
                return Ok(resolved_target.to_path_buf());
            }
            Ok(canonical_recorded_target) => match std::fs::canonicalize(resolved_target) {
                Ok(canonical_resolved_target)
                    if canonical_recorded_target == canonical_resolved_target =>
                {
                    return Ok(resolved_target.to_path_buf());
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(EdenFsError::Other(anyhow!(
                        "failed to canonicalize resolved scratch target {}: {error}",
                        resolved_target.display()
                    )));
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if recorded_target == resolved_target
                    || std::fs::symlink_metadata(resolved_target)
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    return Ok(resolved_target.to_path_buf());
                }
            }
            Err(error) => {
                return Err(EdenFsError::Other(anyhow!(
                    "failed to canonicalize recorded redirection target {}: {error}",
                    recorded_target.display()
                )));
            }
        }

        Err(EdenFsError::ConfigurationError(format!(
            "recorded redirection target {} does not match resolved managed scratch target {}",
            recorded_target.display(),
            resolved_target.display()
        )))
    }
}

// Also compiled under cfg(test) so the path validation is exercised on every
// platform, not just macOS.
#[cfg(any(test, target_os = "macos"))]
fn resolve_apfs_cleanup_target(
    checkout_path: &Path,
    repo_path: &ValidatedRepoPath,
) -> Result<PathBuf> {
    let canonical_checkout = std::fs::canonicalize(checkout_path)
        .from_err()
        .with_context(|| {
            format!(
                "failed to canonicalize checkout path {}",
                checkout_path.display()
            )
        })?;
    let expected_target = canonical_checkout.join(repo_path.as_ref());
    if std::fs::symlink_metadata(&expected_target)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Ok(expected_target);
    }
    match std::fs::canonicalize(&expected_target) {
        Ok(canonical_target) if canonical_target == expected_target => Ok(canonical_target),
        Ok(canonical_target) => Err(EdenFsError::ConfigurationError(format!(
            "APFS redirection path {} resolves to {} instead of its canonical checkout-relative location",
            expected_target.display(),
            canonical_target.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(expected_target),
        Err(error) => Err(EdenFsError::Other(anyhow!(
            "failed to canonicalize APFS redirection path {}: {error}",
            expected_target.display()
        ))),
    }
}

#[cfg(target_os = "macos")]
fn resolve_apfs_cleanup_action(
    checkout_path: &Path,
    repo_path: &ValidatedRepoPath,
) -> Result<Option<RedirectionBackingCleanupAction>> {
    let target = resolve_apfs_cleanup_target(checkout_path, repo_path)?;
    let volume_name = eden_apfs::encode_mount_point_as_volume_name(&target);
    let volume = eden_apfs::ApfsUtil::global()
        .resolve_managed_volume(&volume_name)
        .with_context(|| {
            format!(
                "failed to resolve APFS volume for redirection {}",
                repo_path.as_ref().display()
            )
        })?;
    Ok(volume.map(|volume| RedirectionBackingCleanupAction {
        target,
        action: BackingCleanupAction::ApfsVolume(volume),
    }))
}

/// Resolve and validate cleanup actions for one batch of redirection backing targets.
/// Use [`RedirectionBackingCleanupPlanner`] to share mount lookups across calls
/// in the same planning batch.
pub fn plan_redirection_backing_cleanup(
    checkout_path: &Path,
    redirections: &BTreeMap<PathBuf, Redirection>,
) -> Result<Vec<RedirectionBackingCleanupAction>> {
    RedirectionBackingCleanupPlanner::new(checkout_path).plan(redirections)
}

impl RedirectionBackingCleanupPlanner<'_> {
    /// Resolve and validate cleanup actions for all redirection backing targets.
    pub fn plan(
        &mut self,
        redirections: &BTreeMap<PathBuf, Redirection>,
    ) -> Result<Vec<RedirectionBackingCleanupAction>> {
        let checkout_path = self.checkout_path;
        if !cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows"
        )) {
            return Err(EdenFsError::Other(anyhow!(
                "redirection backing cleanup is not supported on {}",
                std::env::consts::OS
            )));
        }
        // Unknown redirections are unconfigured bind mounts with no managed backing;
        // checkout removal still unmounts them.
        let mut redirections = redirections
            .values()
            .filter(|redirection| redirection.redir_type != RedirectionType::Unknown)
            .peekable();
        if redirections.peek().is_none() {
            return Ok(Vec::new());
        }
        let scratch_root =
            self.resolve_scratch_root(|path| Redirection::resolve_scratch_path(path, None, true))?;
        redirections.try_fold(Vec::new(), |mut actions, redirection| {
            let repo_path = ValidatedRepoPath::try_from(redirection.repo_path.as_path())
                .with_context(|| {
                    format!(
                        "failed to validate cleanup path for redirection {}",
                        redirection.repo_path.display()
                    )
                })?;
            #[cfg(target_os = "macos")]
            if let Some(action) = resolve_apfs_cleanup_action(checkout_path, &repo_path)? {
                actions.push(action);
            }

            // Creation passes the configured key to mkscratch, whose flat layout keeps `.`
            // components, so `./buck-out` and `buck-out` have different backing directories.
            let resolved_target =
                Redirection::resolve_scratch_dir(checkout_path, &redirection.repo_path, true)?;
            let target = self.resolve_scratch_cleanup_target(
                &repo_path,
                redirection.target.as_deref(),
                &resolved_target,
            )?;
            if let Some(action) = scratch_cleanup_action(&scratch_root, target)? {
                actions.push(action);
            }

            Ok(actions)
        })
    }
}

/// Computes the complete set of redirections that are currently in effect.
/// This is based on the explicitly configured settings but also factors in
/// effective configuration by reading the mount table.
pub fn get_effective_redirections(
    instance: &EdenFsInstance,
    checkout: &EdenFsCheckout,
) -> Result<BTreeMap<PathBuf, Redirection>> {
    let mut redirs = BTreeMap::new();
    let path_prefix = checkout.path();
    let mount_table = read_mount_table().context("Failed to read mount table")?;
    for mount_info in mount_table {
        let mount_point = mount_info.mount_point();
        if let Ok(rel_path) = mount_point.strip_prefix(&path_prefix) {
            // The is_bind_mount test may appear to be redundant but it is
            // possible for mounts to layer such that we have:
            //
            // /my/repo    <-- fuse at the top of the vfs
            // /my/repo/buck-out
            // /my/repo    <-- earlier generation fuse at bottom
            //
            // The buck-out bind mount in the middle is visible in the
            // mount table but is not visible via the VFS because there
            // is a different /my/repo mounted over the top.
            //
            // We test whether we can see a mount point at that location
            // before recording it in the effective redirection list so
            // that we don't falsely believe that the bind mount is up.
            let is_mnt_path_a_bind_mount =
                is_bind_mount(mount_info.mount_point()).with_context(|| {
                    format!(
                        "Failed to check if mount point '{}' is a bind mount",
                        mount_info.mount_point().display()
                    )
                })?;
            if path_prefix != mount_point && is_mnt_path_a_bind_mount {
                redirs.insert(
                    rel_path.to_path_buf(),
                    Redirection {
                        repo_path: rel_path.to_path_buf(),
                        redir_type: RedirectionType::Unknown,
                        target: None,
                        source: "mount".to_string(),
                        state: RedirectionState::UnknownMount,
                    },
                );
            }
        }
    }

    let configured_redirections = get_configured_redirections(checkout).with_context(|| {
        format!(
            "Failed to get configured redirections for checkout {}",
            checkout.path().display()
        )
    })?;

    #[cfg(target_os = "macos")]
    let bind_redirection_uses_symlink = configured_redirections
        .values()
        .any(|redir| redir.redir_type == RedirectionType::Bind)
        && Redirection::determine_bind_redirection_type(instance)
            == DarwinBindRedirectionType::SYMLINK;
    #[cfg(target_os = "windows")]
    let bind_redirection_uses_symlink = true;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let bind_redirection_uses_symlink = false;

    for (rel_path, mut redir) in configured_redirections {
        let is_in_mount_table = redirs.contains_key(&rel_path);
        #[cfg(target_os = "macos")]
        let bind_redirection_is_symlink =
            redir.redir_type == RedirectionType::Bind && redir.repo_path_is_symlink(checkout);
        #[cfg(not(target_os = "macos"))]
        let bind_redirection_is_symlink = false;
        let uses_symlink = redirection_uses_symlink(
            redir.redir_type,
            bind_redirection_uses_symlink,
            bind_redirection_is_symlink,
        );
        if is_in_mount_table {
            // The configured redirection entries take precedence over the mount table entries.
            // We overwrite them in the `redirs` map.
            //
            // A symlink-backed redirection should never appear in the mount table; if
            // one does, we don't know what is mounted there. Mount-backed binds found
            // in the table are assumed to be mounted correctly.
            if uses_symlink {
                redir.state = RedirectionState::UnknownMount;
            }
        } else if redir.redir_type == RedirectionType::Bind && !uses_symlink {
            redir.state = RedirectionState::NotMounted;
        } else if uses_symlink {
            if let Ok(is_correct) = is_symlink_correct(instance, &redir, checkout) {
                if !is_correct {
                    redir.state = RedirectionState::SymlinkIncorrect;
                }
            } else {
                // We're considering a variety of errors that might
                // manifest around trying to read the symlink as meaning
                // that the symlink is effectively missing, even if it
                // isn't literally missing.  eg: EPERM means we can't
                // resolve it, so it is effectively no good.
                redir.state = RedirectionState::SymlinkMissing
            }
        }
        redirs.insert(rel_path, redir);
    }

    Ok(redirs)
}

fn redirection_uses_symlink(
    redirection_type: RedirectionType,
    bind_redirection_uses_symlink: bool,
    bind_redirection_is_symlink: bool,
) -> bool {
    redirection_type == RedirectionType::Symlink
        || (redirection_type == RedirectionType::Bind
            && (bind_redirection_uses_symlink || bind_redirection_is_symlink))
}

pub mod scratch {
    use std::collections::BTreeSet;
    use std::collections::VecDeque;
    use std::ffi::OsStr;
    use std::fs;
    use std::fs::DirEntry;
    use std::io::ErrorKind;
    use std::path::Path;
    use std::path::PathBuf;

    use anyhow::Context;
    use anyhow::Result;
    use anyhow::anyhow;
    use edenfs_utils::metadata::MetadataExt;
    use rayon::prelude::*;

    use super::Redirection;

    pub fn usage_for_dir(
        path: &Path,
        device_id: Option<u64>,
    ) -> std::io::Result<(u64, Vec<PathBuf>)> {
        let device_id = match device_id {
            Some(device_id) => device_id,
            None => match fs::metadata(path) {
                Ok(metadata) => metadata.eden_dev(),
                Err(e) if ignored_io_error(&e) => return Ok((0, vec![path.to_path_buf()])),
                Err(e) => return Err(e),
            },
        };

        // Collect directory entries and process them in parallel
        let entries: Vec<DirEntry> = match fs::read_dir(path) {
            Ok(read_dir) => read_dir.filter_map(|e| e.ok()).collect(),
            Err(e) if ignored_io_error(&e) => return Ok((0, vec![path.to_path_buf()])),
            Err(e) => return Err(e),
        };

        let results: Vec<(u64, Vec<PathBuf>)> = entries
            .par_iter()
            .map(|entry| usage_for_dir_entry(entry, device_id))
            .collect();

        let mut total_size = 0u64;
        let mut failed_files = Vec::new();
        for (size, mut failed) in results {
            total_size += size;
            failed_files.append(&mut failed);
        }

        Ok((total_size, failed_files))
    }

    fn usage_for_dir_entry(entry: &DirEntry, parent_device_id: u64) -> (u64, Vec<PathBuf>) {
        let path = entry.path();
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    // Don't recurse onto different filesystems
                    if cfg!(windows) || metadata.eden_dev() == parent_device_id {
                        match usage_for_dir(&path, Some(parent_device_id)) {
                            Ok((size, failed)) => (size, failed),
                            Err(_) => (0, vec![path]),
                        }
                    } else {
                        (0, vec![])
                    }
                } else {
                    (metadata.eden_file_size(), vec![])
                }
            }
            Err(e) if ignored_io_error(&e) => (0, vec![path]),
            Err(_) => (0, vec![path]),
        }
    }

    fn ignored_io_error(error: &std::io::Error) -> bool {
        error.kind() == std::io::ErrorKind::NotFound
            || error.kind() == std::io::ErrorKind::PermissionDenied
    }

    /// Find all the directories under `redirection_path` that aren't present in
    /// `existing_redirections`.
    fn recursively_check_orphaned_mirrored_redirections(
        redirection_path: PathBuf,
        existing_redirections: &BTreeSet<PathBuf>,
    ) -> std::io::Result<Vec<PathBuf>> {
        let mut to_walk = VecDeque::new();
        to_walk.push_back(redirection_path);

        let mut orphaned = Vec::new();
        while let Some(current) = to_walk.pop_front() {
            // A range is required here to distinguish 3 cases:
            //  0) Is that path an existing redirection
            //  1) Is there an existing redirection in a subdirectory?
            //  2) Is this an orphaned redirection?
            let num_existing_redirections = existing_redirections
                // Logarithmically filter all the paths whose prefix is `current`
                .range(std::ops::RangeFrom {
                    start: current.clone(),
                })
                // And then filter the remaining paths whose prefix do not start with `current`.
                .take_while(|p| p.starts_with(&current))
                .count();
            match num_existing_redirections {
                0 => orphaned.push(current),
                1 if existing_redirections.contains(&current) => continue,
                _ => {
                    if current.is_dir() {
                        for current_subdir in fs::read_dir(current)? {
                            to_walk.push_back(current_subdir?.path());
                        }
                    }
                }
            }
        }

        Ok(orphaned)
    }

    fn get_orphaned_redirection_targets_impl(
        scratch_path: PathBuf,
        scratch_subdir: PathBuf,
        existing_redirections: &BTreeSet<PathBuf>,
    ) -> Result<Vec<PathBuf>> {
        // Scratch directories can either be flat, ie: a directory like foo/bar will be encoded as
        // fooZbar, or mirrored, where no encoding is performed. Let's test how mkscratch encoded the
        // directory and compare it against the EdenFS scratch namespace to test if mkscratch is
        // configured to be flat or mirrored.
        let is_scratch_mirrored = scratch_path.ends_with(&scratch_subdir);
        let (scratch_root, prefix) = if is_scratch_mirrored {
            (
                scratch_path
                    .ancestors()
                    .nth(scratch_subdir.components().count() + 1)
                    .unwrap(),
                scratch_subdir,
            )
        } else {
            (
                // We want to get the root of the scratch directory, which is 2 level up from the path
                // mkscratch gave us: first the path in the repository, and second the repository path.
                scratch_path.parent().unwrap().parent().unwrap(),
                PathBuf::from(scratch_path.file_name().unwrap().to_os_string()),
            )
        };

        // mkscratch only prints the path, so the scratch root may not exist yet.
        let scratch_root_entries = match fs::read_dir(scratch_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to read scratch root {}", scratch_root.display())
                });
            }
        };
        let mut orphaned_redirections = Vec::new();
        if is_scratch_mirrored {
            for dirent in scratch_root_entries {
                let dirent_path = dirent?.path();
                let redirection_path = dirent_path.join(&prefix);
                if redirection_path.exists() {
                    // The directory exist, now we need to check if there is an unknown redirection.
                    orphaned_redirections.extend(recursively_check_orphaned_mirrored_redirections(
                        redirection_path,
                        existing_redirections,
                    )?);
                }
            }
        } else {
            for dirent in scratch_root_entries {
                let dirent_path = dirent?.path();
                if !dirent_path.is_dir() {
                    continue;
                }

                for subdir in fs::read_dir(dirent_path)? {
                    let path = subdir?.path();
                    if !existing_redirections.contains(&path)
                        && path
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(&prefix.to_string_lossy().into_owned())
                    {
                        orphaned_redirections.push(path);
                    }
                }
            }
        }

        Ok(orphaned_redirections)
    }

    pub fn get_orphaned_redirection_targets(
        existing_redirections: &BTreeSet<PathBuf>,
    ) -> Result<Vec<PathBuf>> {
        get_orphaned_redirection_targets_with_mkscratch(
            &Redirection::mkscratch_bin(),
            existing_redirections,
        )
    }

    fn get_orphaned_redirection_targets_with_mkscratch(
        mkscratch: &Path,
        existing_redirections: &BTreeSet<PathBuf>,
    ) -> Result<Vec<PathBuf>> {
        let scratch_subdir = Redirection::scratch_subdir();
        let scratch_subdir_str = scratch_subdir.to_string_lossy();
        let home_dir = match dirs::home_dir() {
            Some(dir) => dir,
            None => return Ok(vec![]),
        };
        let home_dir_str = home_dir.to_string_lossy();

        let mkscratch_args = [
            "--no-create",
            "path",
            &*home_dir_str,
            "--subdir",
            &*scratch_subdir_str,
        ]
        .map(OsStr::new);
        let mkscratch_res = Redirection::run_mkscratch(mkscratch, &mkscratch_args);

        let scratch_path = match mkscratch_res {
            Ok(output) if output.status.success() => {
                Redirection::parse_mkscratch_stdout(&output.stdout)?
            }
            Ok(output) => {
                return Err(anyhow!(
                    "mkscratch exited with {:?} while resolving the scratch root: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Err(error) => return Err(error).context("failed to run mkscratch"),
        };

        get_orphaned_redirection_targets_impl(scratch_path, scratch_subdir, existing_redirections)
    }

    #[cfg(test)]
    mod tests {
        use std::fs::create_dir;
        use std::fs::create_dir_all;
        use std::path::Path;

        use tempfile::TempDir;

        use super::*;

        fn create_and_add(
            path: impl AsRef<Path>,
            existing_redirections: &mut BTreeSet<PathBuf>,
        ) -> Result<()> {
            create_dir_all(path.as_ref())?;
            existing_redirections.insert(path.as_ref().to_path_buf());
            Ok(())
        }

        #[cfg(unix)]
        #[test]
        fn test_get_orphaned_redirection_targets_propagates_mkscratch_failure() -> Result<()> {
            use std::os::unix::fs::PermissionsExt;

            let tempdir = TempDir::new()?;
            let fake_mkscratch = tempdir.path().join("mkscratch");
            std::fs::write(
                &fake_mkscratch,
                "#!/bin/sh\necho 'scratch config broken' >&2\nexit 1\n",
            )?;
            std::fs::set_permissions(&fake_mkscratch, std::fs::Permissions::from_mode(0o755))?;

            let error =
                get_orphaned_redirection_targets_with_mkscratch(&fake_mkscratch, &BTreeSet::new())
                    .expect_err(
                        "a failing mkscratch must surface an error, not an empty orphan list",
                    );
            assert!(
                error.to_string().contains("scratch config broken"),
                "error should carry mkscratch's stderr: {error:#}"
            );
            Ok(())
        }

        #[test]
        fn test_get_orphaned_redirection_targets_missing_scratch_root() -> Result<()> {
            let tempdir = TempDir::new()?;
            let scratch_path = tempdir.path().join("missing/repository/fooZbar");

            let res = get_orphaned_redirection_targets_impl(
                scratch_path,
                Path::new("foo/bar").to_path_buf(),
                &BTreeSet::new(),
            )?;
            assert!(
                res.is_empty(),
                "a scratch root that doesn't exist has no orphans"
            );
            Ok(())
        }

        #[test]
        fn test_recursive_check_orphaned_mirrored_redirections() -> Result<()> {
            let tempdir = TempDir::new()?;
            let path = tempdir.path();
            let mut existing_redirections = BTreeSet::new();

            // Single known directory
            create_and_add(path.join("A"), &mut existing_redirections)?;

            // Directory with an orphaned directory inside
            create_and_add(path.join("B/1"), &mut existing_redirections)?;
            create_and_add(path.join("B/2"), &mut existing_redirections)?;
            create_dir(path.join("B/3"))?;

            // Single orphaned directory
            create_dir(path.join("C"))?;

            // Single orphaned with several subdirectories
            create_dir_all(path.join("D/1"))?;
            create_dir_all(path.join("D/2"))?;
            create_dir_all(path.join("D/3"))?;

            // Orphaned redirection with an existing redirection as a sibling
            create_dir_all(path.join("E/1/2"))?;
            create_and_add(path.join("E/1/3"), &mut existing_redirections)?;

            let res = recursively_check_orphaned_mirrored_redirections(
                path.to_path_buf(),
                &existing_redirections,
            )?;
            assert!(!res.contains(&path.join("A")));

            assert!(!res.contains(&path.join("B")));
            assert!(!res.contains(&path.join("B/1")));
            assert!(!res.contains(&path.join("B/2")));
            assert!(res.contains(&path.join("B/3")));

            assert!(res.contains(&path.join("C")));

            assert!(res.contains(&path.join("D")));

            eprintln!("{res:?}");
            assert!(res.contains(&path.join("E/1/2")));
            Ok(())
        }

        #[test]
        fn test_get_orphaned_redirection_targets_mirrored() -> Result<()> {
            let tempdir = TempDir::new()?;
            let path = tempdir.path();
            let scratch_subdir = Path::new("foo/bar");
            let mut existing_redirections = BTreeSet::new();

            let scratch_path = path.join("repository").join(scratch_subdir);

            // Single known directory
            create_and_add(
                path.join("repo1").join(scratch_subdir).join("A"),
                &mut existing_redirections,
            )?;

            // Directory with an orphaned directory inside
            create_and_add(
                path.join("repo2").join(scratch_subdir).join("B/1"),
                &mut existing_redirections,
            )?;
            create_and_add(
                path.join("repo2").join(scratch_subdir).join("B/2"),
                &mut existing_redirections,
            )?;
            create_dir_all(path.join("repo2").join(scratch_subdir).join("B/3"))?;

            // Single orphaned directory
            create_dir_all(path.join("repo3").join(scratch_subdir).join("C"))?;

            // Single orphaned with several subdirectories
            create_dir_all(path.join("repo4").join(scratch_subdir).join("D/1"))?;
            create_dir_all(path.join("repo4").join(scratch_subdir).join("D/2"))?;
            create_dir_all(path.join("repo4").join(scratch_subdir).join("D/3"))?;

            let res = get_orphaned_redirection_targets_impl(
                scratch_path,
                scratch_subdir.to_path_buf(),
                &existing_redirections,
            )?;
            assert!(!res.contains(&path.join("repo1").join(scratch_subdir).join("A")));

            assert!(!res.contains(&path.join("repo2").join(scratch_subdir).join("B")));
            assert!(!res.contains(&path.join("repo2").join(scratch_subdir).join("B/1")));
            assert!(!res.contains(&path.join("repo2").join(scratch_subdir).join("B/2")));
            assert!(res.contains(&path.join("repo2").join(scratch_subdir).join("B/3")));

            assert!(res.contains(&path.join("repo3").join(scratch_subdir)));

            assert!(res.contains(&path.join("repo4").join(scratch_subdir)));

            Ok(())
        }

        #[test]
        fn test_get_orphaned_redirection_targets_flat() -> Result<()> {
            let tempdir = TempDir::new()?;
            let path = tempdir.path();
            let scratch_subdir = Path::new("fooZbar");
            let mut existing_redirections = BTreeSet::new();

            let scratch_path = path.join("repository").join(scratch_subdir);

            // Single known directory
            let repo1_a_path =
                path.join("repo1")
                    .join(format!("{}Z{}", scratch_subdir.display(), "A"));
            create_and_add(&repo1_a_path, &mut existing_redirections)?;

            // Directory with an orphaned directory inside
            let repo2_b1_path =
                path.join("repo2")
                    .join(format!("{}Z{}Z{}", scratch_subdir.display(), "B", "1"));
            let repo2_b2_path =
                path.join("repo2")
                    .join(format!("{}Z{}Z{}", scratch_subdir.display(), "B", "2"));
            let repo2_b3_path =
                path.join("repo2")
                    .join(format!("{}Z{}Z{}", scratch_subdir.display(), "B", "3"));
            create_and_add(&repo2_b1_path, &mut existing_redirections)?;
            create_and_add(&repo2_b2_path, &mut existing_redirections)?;
            create_dir_all(&repo2_b3_path)?;

            // Single orphaned directory
            let repo3_c_path =
                path.join("repo3")
                    .join(format!("{}Z{}", scratch_subdir.display(), "C"));
            create_dir_all(&repo3_c_path)?;

            let res = get_orphaned_redirection_targets_impl(
                scratch_path,
                Path::new("foo/bar").to_path_buf(),
                &existing_redirections,
            )?;
            assert!(!res.contains(&repo1_a_path));

            assert!(!res.contains(&repo2_b1_path));
            assert!(!res.contains(&repo2_b2_path));
            assert!(res.contains(&repo2_b3_path));

            assert!(res.contains(&repo3_c_path));

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::ffi::OsStr;
    #[cfg(unix)]
    use std::fs::File;
    #[cfg(unix)]
    use std::io;
    #[cfg(unix)]
    use std::io::ErrorKind;
    #[cfg(unix)]
    use std::iter::once;
    #[cfg(unix)]
    use std::os::fd::OwnedFd;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::process::Command;
    #[cfg(unix)]
    use std::process::ExitStatus;
    #[cfg(unix)]
    use std::process::Stdio;

    #[cfg(unix)]
    use edenfs_error::EdenFsError;
    #[cfg(target_os = "windows")]
    use mkscratch::zzencode;
    use rand::distr::Alphanumeric;
    use rand::distr::SampleString;
    use serde_test::Token;
    use serde_test::assert_ser_tokens;
    use tempfile::tempdir;

    #[cfg(unix)]
    use crate::redirect::MKSCRATCH_SUCCESS_MARKER;
    #[cfg(unix)]
    use crate::redirect::MKSCRATCH_WRAPPER;
    #[cfg(unix)]
    use crate::redirect::MkscratchExitStatus;
    use crate::redirect::REPO_SOURCE;
    use crate::redirect::Redirection;
    use crate::redirect::RedirectionBackingCleanupPlanner;
    use crate::redirect::RedirectionState;
    use crate::redirect::RedirectionType;
    use crate::redirect::RepoPathDisposition;
    use crate::redirect::SCRATCH_README;
    use crate::redirect::ValidatedRepoPath;
    use crate::redirect::plan_redirection_backing_cleanup;
    use crate::redirect::redirection_uses_symlink;
    use crate::redirect::resolve_apfs_cleanup_target;
    use crate::redirect::scratch_cleanup_action;

    #[test]
    fn test_bind_redirection_with_symlink_backing_uses_symlink_state() {
        assert!(
            redirection_uses_symlink(RedirectionType::Symlink, false, false),
            "explicit symlink redirections should use symlink state detection"
        );
        assert!(
            redirection_uses_symlink(RedirectionType::Bind, true, false),
            "bind redirections should use symlink state detection when configured"
        );
        assert!(
            redirection_uses_symlink(RedirectionType::Bind, false, true),
            "bind redirections should use symlink state detection when already backed by a symlink"
        );
        assert!(
            !redirection_uses_symlink(RedirectionType::Bind, false, false),
            "mount-backed bind redirections should use mount state detection"
        );
    }

    #[test]
    fn test_parse_mkscratch_stdout_preserves_success_path_behavior() {
        #[cfg(unix)]
        {
            assert_eq!(
                Redirection::parse_mkscratch_stdout(b"relative/scratch\n")
                    .expect("mkscratch path should parse"),
                PathBuf::from("relative/scratch"),
            );
            assert_eq!(
                Redirection::parse_mkscratch_stdout(b"relative/scratch\r\n")
                    .expect("CRLF-terminated mkscratch path should parse"),
                PathBuf::from("relative/scratch\r"),
            );
            assert_eq!(
                Redirection::parse_mkscratch_stdout(b"relative/\xff\n")
                    .expect("non-UTF-8 mkscratch path should parse")
                    .as_os_str()
                    .as_bytes(),
                b"relative/\xff",
            );
        }

        #[cfg(windows)]
        {
            assert_eq!(
                Redirection::parse_mkscratch_stdout(b"relative\\scratch\r\n")
                    .expect("mkscratch path should parse"),
                PathBuf::from(r"relative\scratch"),
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_mkscratch_recovery_requires_success_marker() {
        let success = Redirection::classify_mkscratch_recovery(
            None,
            b"/tmp/scratch\n".to_vec(),
            [b"warning".as_slice(), MKSCRATCH_SUCCESS_MARKER].concat(),
        )
        .expect("marked output should prove success");
        assert!(matches!(
            success.status,
            MkscratchExitStatus::RecoveredAfterReap
        ));
        assert_eq!(success.stdout, b"/tmp/scratch\n");
        assert_eq!(success.stderr, b"warning");

        assert_eq!(
            Redirection::parse_mkscratch_stdout(&success.stdout)
                .expect("successful output should contain a path"),
            PathBuf::from("/tmp/scratch"),
        );

        assert!(
            Redirection::classify_mkscratch_recovery(
                None,
                b"/tmp/scratch\n".to_vec(),
                b"warning".to_vec(),
            )
            .is_err(),
            "unmarked output must not turn an unknown exit status into success",
        );

        for raw in [7 << 8, libc::SIGTERM] {
            assert!(
                Redirection::classify_mkscratch_recovery(
                    Some(ExitStatus::from_raw(raw)),
                    b"/tmp/scratch\n".to_vec(),
                    [b"warning".as_slice(), MKSCRATCH_SUCCESS_MARKER].concat(),
                )
                .is_err(),
                "a marker must not hide a known failure",
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_mkscratch_recovery_resumes_interrupted_reads_and_waits() {
        let (marker_start, marker_end) =
            MKSCRATCH_SUCCESS_MARKER.split_at(MKSCRATCH_SUCCESS_MARKER.len() / 2);
        let mut reads = [
            (
                b"/tmp/".to_vec(),
                b"war".to_vec(),
                Err(ErrorKind::Interrupted.into()),
            ),
            (
                b"scratch\n".to_vec(),
                [b"ning".as_slice(), marker_start].concat(),
                Err(ErrorKind::Interrupted.into()),
            ),
            (Vec::new(), marker_end.to_vec(), Ok(())),
        ]
        .into_iter();
        let mut waits = [
            Err(ErrorKind::Interrupted.into()),
            Err(ErrorKind::Interrupted.into()),
            Err(io::Error::from_raw_os_error(libc::ECHILD)),
        ]
        .into_iter();

        let output = Redirection::finish_mkscratch_recovery(
            |output| {
                let (stdout, stderr, result) = reads.next().expect("must stop reading at EOF");
                output[0].extend(stdout);
                output[1].extend(stderr);
                result
            },
            || waits.next().expect("must stop waiting after completion"),
        )
        .expect("interrupted I/O must preserve the complete output and marker");

        assert!(matches!(
            output.status,
            MkscratchExitStatus::RecoveredAfterReap
        ));
        assert_eq!(output.stdout, b"/tmp/scratch\n");
        assert_eq!(output.stderr, b"warning");
    }

    #[cfg(unix)]
    #[test]
    fn test_mkscratch_recovery_preserves_read_errors() {
        let mut reads = once(Err(ErrorKind::PermissionDenied.into()));
        let error = Redirection::finish_mkscratch_recovery(
            |output| {
                output[0].extend_from_slice(b"/tmp/scratch\n");
                output[1].extend_from_slice(MKSCRATCH_SUCCESS_MARKER);
                reads.next().expect("must not retry a non-interrupted read")
            },
            || panic!("must propagate a read error before collecting exit status"),
        )
        .expect_err("even marked output must not hide an I/O failure");

        let EdenFsError::Other(error) = error else {
            panic!("expected the original I/O error");
        };
        assert_eq!(
            error
                .downcast_ref::<io::Error>()
                .expect("must preserve the I/O error")
                .kind(),
            ErrorKind::PermissionDenied,
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_mkscratch_recovery_preserves_wait_errors() {
        let mut reads = once(Ok(()));
        let mut waits = once(Err(ErrorKind::PermissionDenied.into()));
        let error = Redirection::finish_mkscratch_recovery(
            |output| {
                output[0].extend_from_slice(b"/tmp/scratch\n");
                output[1].extend_from_slice(MKSCRATCH_SUCCESS_MARKER);
                reads.next().expect("must not reread output while waiting")
            },
            || waits.next().expect("must not retry a non-interrupted wait"),
        )
        .expect_err("even marked output must not hide a wait failure");

        let EdenFsError::Other(error) = error else {
            panic!("expected the original I/O error");
        };
        assert_eq!(
            error
                .downcast_ref::<io::Error>()
                .expect("must preserve the I/O error")
                .kind(),
            ErrorKind::PermissionDenied,
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_mkscratch_recovery_wrapper_reports_command_result() {
        let success = Redirection::retry_mkscratch_after_reap(
            Path::new("/bin/sh"),
            &["-c", "printf '/tmp/scratch\\n'; printf warning >&2"].map(OsStr::new),
        )
        .expect("successful retry should be recognized");
        assert!(matches!(
            success.status,
            MkscratchExitStatus::RecoveredAfterReap
        ));
        assert_eq!(success.stdout, b"/tmp/scratch\n");
        assert_eq!(success.stderr, b"warning");

        assert!(
            Redirection::retry_mkscratch_after_reap(
                Path::new("/bin/sh"),
                &["-c", "printf failure >&2; exit 7"].map(OsStr::new),
            )
            .is_err(),
            "a failed retry must remain a failure",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_mkscratch_recovery_avoids_atfork_callbacks() {
        const CHILD_MODE: &str = "EDEN_MKSCRATCH_ATFORK_TEST";
        const READY: &[u8] = b"mkscratch atfork observer ready\n";
        const CALLED: &[u8] = b"mkscratch atfork observer called\n";
        const DONE: &[u8] = b"mkscratch atfork observer done\n";

        fn write_marker(marker: &[u8]) {
            // SAFETY: The static bytes remain valid, write is async-signal-safe,
            // and failure exits without unwinding across the callback's C ABI.
            unsafe {
                if libc::write(libc::STDERR_FILENO, marker.as_ptr().cast(), marker.len())
                    != marker.len() as isize
                {
                    libc::_exit(101);
                }
            }
        }

        extern "C" fn observe_fork() {
            write_marker(CALLED);
        }

        if let Ok(mode) = std::env::var(CHILD_MODE) {
            // SAFETY: Only this isolated subprocess registers the static,
            // async-signal-safe callback and changes its own alarm.
            unsafe {
                libc::alarm(30);
                assert_eq!(libc::pthread_atfork(None, None, Some(observe_fork)), 0);
            }
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let worker = std::thread::spawn(move || wait.recv());
            write_marker(READY);
            match mode.as_str() {
                "control" => {
                    // SAFETY: The child only calls async-signal-safe _exit;
                    // the parent waits for this exact child, retrying EINTR.
                    unsafe {
                        let pid = libc::fork();
                        assert!(pid >= 0);
                        if pid == 0 {
                            libc::_exit(0);
                        }
                        let mut status = 0;
                        loop {
                            let waited = libc::waitpid(pid, &mut status, 0);
                            if waited == -1
                                && std::io::Error::last_os_error().kind() == ErrorKind::Interrupted
                            {
                                continue;
                            }
                            assert_eq!(waited, pid);
                            assert_eq!(status, 0);
                            break;
                        }
                    }
                }
                "recovery" => {
                    let output = Redirection::retry_mkscratch_after_reap(
                        Path::new("/bin/sh"),
                        &["-c", "printf '/tmp/scratch\\n'; printf warning >&2"].map(OsStr::new),
                    )
                    .expect("recovery should retain the command output");
                    assert!(output.status.success());
                    assert_eq!(output.stdout, b"/tmp/scratch\n");
                    assert_eq!(output.stderr, b"warning");
                }
                _ => panic!("unexpected atfork test mode: {mode}"),
            }
            drop(release);
            assert!(worker.join().unwrap().is_err());
            write_marker(DONE);
            std::process::exit(0);
        }

        for mode in ["control", "recovery"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "redirect::tests::test_mkscratch_recovery_avoids_atfork_callbacks",
                    "--nocapture",
                ])
                .env(CHILD_MODE, mode)
                .output()
                .unwrap();
            assert!(output.status.success(), "{mode}: {output:?}");
            for marker in [READY, DONE] {
                assert!(
                    output.stderr.windows(marker.len()).any(|w| w == marker),
                    "{mode}: missing observer checkpoint: {output:?}",
                );
            }
            let called = output.stderr.windows(CALLED.len()).any(|w| w == CALLED);
            assert_eq!(called, mode == "control", "{mode}: {output:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_mkscratch_recovery_preserves_output_after_external_reap() {
        for (script, succeeds) in [
            ("printf '/tmp/scratch\\n'; printf warning >&2", true),
            (
                "printf '/tmp/scratch\\n'; printf warning >&2; exit 7",
                false,
            ),
        ] {
            let mut child = Command::new("/bin/sh")
                .args(["-c", MKSCRATCH_WRAPPER, "--", "/bin/sh", "-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut status = 0;
            loop {
                // SAFETY: status is writable and waitpid targets only the
                // child owned by this test, simulating the daemon's reaper.
                let waited = unsafe { libc::waitpid(child.id() as libc::pid_t, &mut status, 0) };
                if waited == -1 && io::Error::last_os_error().kind() == ErrorKind::Interrupted {
                    continue;
                }
                assert_eq!(waited, child.id() as libc::pid_t);
                break;
            }
            assert_eq!(ExitStatus::from_raw(status).success(), succeeds);
            assert_eq!(child.wait().unwrap_err().raw_os_error(), Some(libc::ECHILD));
            let mut pipes = [
                child
                    .stdout
                    .take()
                    .map(|pipe| File::from(OwnedFd::from(pipe))),
                child
                    .stderr
                    .take()
                    .map(|pipe| File::from(OwnedFd::from(pipe))),
            ];
            let output = Redirection::finish_mkscratch_recovery(
                |output| Redirection::read_mkscratch_output(&mut pipes, output),
                || child.wait(),
            );
            if succeeds {
                let output = output.expect("ECHILD must not discard the captured success marker");
                assert!(output.status.success());
                assert_eq!(output.stdout, b"/tmp/scratch\n");
                assert_eq!(output.stderr, b"warning");
            } else {
                let error = output.expect_err("ECHILD without a marker must not imply success");
                assert!(error.to_string().contains("without a success marker"));
            }
            assert!(pipes.iter().all(Option::is_none));
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_mkscratch_recovery_drains_both_output_pipes() {
        let output = Redirection::retry_mkscratch_after_reap(
            Path::new("/bin/sh"),
            &[
                "-c",
                "i=0; while [ \"$i\" -lt 16384 ]; do printf 'stdout\\n'; printf 'stderr\\n' >&2; i=$((i+1)); done",
            ].map(OsStr::new),
        )
        .expect("both output pipes must be drained without deadlocking");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"stdout\n".repeat(16384));
        assert_eq!(output.stderr, b"stderr\n".repeat(16384));
    }

    #[test]
    fn test_apply_symlink() {
        // The symlink creation will fail if we try to create a symlink where there's an existing
        // file. So let's try to prevent collisions by making the filename random.
        // TODO(@Cuev): Is there a better way to do this?
        let rand_file = format!(
            "test_path_{}",
            Alphanumeric.sample_string(&mut rand::rng(), 16)
        );
        let redir = Redirection {
            repo_path: PathBuf::from(rand_file),
            redir_type: RedirectionType::Symlink,
            target: None,
            source: REPO_SOURCE.into(),
            state: RedirectionState::UnknownMount,
        };
        let fake_checkout = tempdir().expect("failed to create fake checkout");
        let fake_checkout_path = fake_checkout.path();

        let symlink_path = fake_checkout_path.join(redir.repo_path());
        redir
            ._apply_symlink(fake_checkout_path, &symlink_path, false)
            .expect("Failed to create symlink");
        assert!(symlink_path.is_symlink())
    }

    #[cfg(windows)]
    #[test]
    fn test_apply_symlink_with_existing_temp_symlink() {
        // Arrange
        // The symlink creation will fail if we try to create a symlink where there's an existing
        // file. So let's try to prevent collisions by making unique working directory.
        let rand_path = format!(
            "test_path_{}",
            Alphanumeric.sample_string(&mut rand::rng(), 16)
        );
        let redir = Redirection {
            repo_path: PathBuf::from(rand_path),
            redir_type: RedirectionType::Symlink,
            target: None,
            source: REPO_SOURCE.into(),
            state: RedirectionState::UnknownMount,
        };
        let fake_checkout = tempdir().expect("failed to create fake checkout");
        let fake_checkout_path = fake_checkout.path();
        let symlink_path = fake_checkout_path.join(redir.repo_path());
        let fake_target_path = fake_checkout_path.join("target");
        let temp_symlink_path = fake_checkout_path
            .parent()
            .unwrap()
            .join(zzencode(&symlink_path.to_string_lossy()));
        std::fs::create_dir_all(&fake_target_path).expect("failed to create target directory");
        // Create a temp symlink file so that we can test deletion of such file succeeds and we recreate it.
        std::os::windows::fs::symlink_dir(&fake_target_path, &temp_symlink_path)
            .expect("failed to create temp symlink");

        // Act
        redir
            ._apply_symlink(fake_checkout_path, &symlink_path, false)
            .expect("Failed to create symlink");

        // Assert
        assert!(symlink_path.is_symlink())
    }

    /// returns true if we succeeded in removing the existing file/dir
    fn try_remove(path: &Path) -> bool {
        if path.is_file() {
            std::fs::remove_file(path).ok();
        } else if path.is_dir() {
            std::fs::remove_dir_all(path).ok();
        }
        !path.exists()
    }

    fn check_empty_dir_and_non_empty_dir_and_file(dir_path: &Path) {
        std::fs::create_dir_all(dir_path).ok();
        assert_eq!(
            RepoPathDisposition::analyze(dir_path).expect("failed to analyze RepoPathDisposition"),
            RepoPathDisposition::IsEmptyDir
        );
        let test_file = dir_path.join("test_file");
        std::fs::File::create(&test_file).ok();
        assert_eq!(
            RepoPathDisposition::analyze(&test_file)
                .expect("failed to analyze RepoPathDisposition"),
            RepoPathDisposition::IsFile
        );
        assert_eq!(
            RepoPathDisposition::analyze(dir_path).expect("failed to analyze RepoPathDisposition"),
            RepoPathDisposition::IsNonEmptyDir
        );
    }

    /// We will test DNE, Empty dir, Non-empty dir, file, and symlink dispositions. We skip bind
    /// mounts since those are tough to test
    #[test]
    fn test_analyze() {
        // test non-existent path
        let dne_dir = tempdir().expect("couldn't create temporary directory for testing");
        let dne_path = dne_dir
            .path()
            .join(Alphanumeric.sample_string(&mut rand::rng(), 16));
        #[allow(clippy::if_same_then_else)]
        if !dne_path.exists() {
            assert_eq!(
                RepoPathDisposition::analyze(&dne_path)
                    .expect("failed to analyze RepoPathDisposition"),
                RepoPathDisposition::DoesNotExist
            );
        // we were unlucky enough to somehow collide with an existing file. Let's try removing
        // it. If that fails, just skip this case
        } else if try_remove(&dne_path) {
            assert_eq!(
                RepoPathDisposition::analyze(&dne_path)
                    .expect("failed to analyze RepoPathDisposition"),
                RepoPathDisposition::DoesNotExist
            );
        }

        // empty dir, non-empty dir, and file
        let tmp_dir = tempdir().expect("couldn't create temp directory for testing");
        let dir_path = tmp_dir
            .path()
            .join(Alphanumeric.sample_string(&mut rand::rng(), 16));
        #[allow(clippy::if_same_then_else)]
        if !dir_path.exists() {
            check_empty_dir_and_non_empty_dir_and_file(&dir_path);
        } else if try_remove(&dir_path) {
            check_empty_dir_and_non_empty_dir_and_file(&dir_path);
        }

        // symlink
        let symlink_dir = tempdir().expect("couldn't create temp directory for testing");
        let symlink_path = symlink_dir
            .path()
            .join(Alphanumeric.sample_string(&mut rand::rng(), 16));
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&dir_path, &symlink_path).ok();
        #[cfg(not(windows))]
        std::os::unix::fs::symlink(&dir_path, &symlink_path).ok();

        if std::fs::symlink_metadata(&symlink_path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            // we actually created a symlink, so we can test if disposition detects the symlink
            assert_eq!(
                RepoPathDisposition::analyze(&symlink_path)
                    .expect("failed to analyze RepoPathDisposition"),
                RepoPathDisposition::IsSymlink
            );
        }
        // if we failed to make the symlink, skip this test case
    }

    /// The format of JSON-serialized redirections is relied upon by callers of
    /// `redirect --json`, so we should try not to break them.
    #[test]
    fn test_serialize_redirection() {
        assert_ser_tokens(
            &Redirection {
                repo_path: "/mnt/foo".into(),
                redir_type: RedirectionType::Bind,
                source: "test".to_string(),
                state: RedirectionState::UnknownMount,
                target: None,
            },
            &[
                Token::Struct {
                    name: "Redirection",
                    len: 5,
                },
                Token::Str("repo_path"),
                Token::Str("/mnt/foo"),
                Token::Str("type"),
                Token::UnitVariant {
                    name: "RedirectionType",
                    variant: "bind",
                },
                Token::Str("source"),
                Token::Str("test"),
                Token::Str("state"),
                Token::UnitVariant {
                    name: "RedirectionState",
                    variant: "unknown-mount",
                },
                Token::Str("target"),
                Token::None,
                Token::StructEnd,
            ],
        );

        assert_ser_tokens(
            &Redirection {
                repo_path: "/mnt/foo".into(),
                redir_type: RedirectionType::Bind,
                source: "test".to_string(),
                state: RedirectionState::UnknownMount,
                target: Some("/mnt/target".into()),
            },
            &[
                Token::Struct {
                    name: "Redirection",
                    len: 5,
                },
                Token::Str("repo_path"),
                Token::Str("/mnt/foo"),
                Token::Str("type"),
                Token::UnitVariant {
                    name: "RedirectionType",
                    variant: "bind",
                },
                Token::Str("source"),
                Token::Str("test"),
                Token::Str("state"),
                Token::UnitVariant {
                    name: "RedirectionState",
                    variant: "unknown-mount",
                },
                Token::Str("target"),
                Token::Some,
                Token::Str("/mnt/target"),
                Token::StructEnd,
            ],
        );
    }

    #[test]
    fn cleanup_reuses_scratch_root_and_retries_failed_resolution() {
        let checkout = Path::new("checkout");
        let mut planner = RedirectionBackingCleanupPlanner::new(checkout);
        let attempts = std::cell::Cell::new(0);
        let resolve = |path: &Path| {
            assert_eq!(path, checkout);
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                return Err(edenfs_error::EdenFsError::Other(anyhow::anyhow!(
                    "scratch resolution failed"
                )));
            }
            Ok(PathBuf::from("scratch"))
        };

        assert!(planner.resolve_scratch_root(resolve).is_err());
        for _ in 0..3 {
            assert_eq!(
                planner.resolve_scratch_root(resolve).unwrap(),
                Path::new("scratch")
            );
        }
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn cleanup_accepts_matching_recorded_scratch_target() {
        let temp_dir = tempdir().expect("temporary directory should be created");
        let checkout = temp_dir.path().join("checkout");
        std::fs::create_dir(&checkout).expect("checkout directory should be created");
        let repo_path = ValidatedRepoPath::try_from(Path::new("generated/output"))
            .expect("repo-relative path should be valid");
        let resolved_target = temp_dir.path().join("scratch/generated-output");
        std::fs::create_dir_all(&resolved_target).expect("scratch directory should be created");

        assert_eq!(
            RedirectionBackingCleanupPlanner::new(&checkout)
                .resolve_scratch_cleanup_target(
                    &repo_path,
                    Some(&resolved_target),
                    &resolved_target,
                )
                .expect("recorded scratch target should be valid"),
            resolved_target
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn backing_cleanup_requires_mount_to_disappear() {
        let checkout = tempdir().unwrap();
        let repo_path = checkout.path().join("buck-out");
        std::fs::create_dir(&repo_path).unwrap();
        std::fs::write(repo_path.join("contents"), "backing data").unwrap();
        assert_eq!(
            RepoPathDisposition::analyze(&repo_path).unwrap(),
            RepoPathDisposition::IsNonEmptyDir
        );
        for detach in [false, true] {
            let mounted = std::cell::Cell::new(true);
            let called = std::cell::Cell::new(false);
            let result =
                super::unmount_and_verify(&repo_path, || async { Ok(mounted.get()) }, async {
                    called.set(true);
                    if detach {
                        mounted.set(false);
                    }
                    Ok(())
                })
                .await;
            assert!(
                called.get(),
                "mount-table detection must request unmount even when st_dev matches"
            );
            if detach {
                result.expect("confirmed unmount permits backing cleanup");
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("still mounted after unmount")
                );
            }
            assert_eq!(
                std::fs::read_to_string(repo_path.join("contents")).unwrap(),
                "backing data"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_validates_unrecorded_symlink_target() {
        let temp_dir = tempdir().unwrap();
        let checkout = temp_dir.path().join("checkout");
        let installed = temp_dir.path().join("scratch-before");
        let resolved = temp_dir.path().join("scratch-after");
        std::fs::create_dir(&checkout).unwrap();
        std::fs::create_dir(&installed).unwrap();
        let repo_path = ValidatedRepoPath::try_from(Path::new("buck-out")).unwrap();
        let link = checkout.join(repo_path.as_ref());
        std::os::unix::fs::symlink(&installed, &link).unwrap();

        for recorded in [None, Some(Path::new(""))] {
            let error = RedirectionBackingCleanupPlanner::new(&checkout)
                .resolve_scratch_cleanup_target(&repo_path, recorded, &resolved)
                .expect_err("a changed scratch template must not abandon the installed backing");
            assert!(
                error
                    .to_string()
                    .contains("does not match resolved managed scratch target")
            );
        }
        assert_eq!(std::fs::read_link(&link).unwrap(), installed);
        assert!(installed.exists());
        assert!(!resolved.exists());
        assert_eq!(
            RedirectionBackingCleanupPlanner::new(&checkout)
                .resolve_scratch_cleanup_target(&repo_path, None, &installed)
                .unwrap(),
            installed
        );
        std::fs::remove_dir(&installed).unwrap();
        assert_eq!(
            RedirectionBackingCleanupPlanner::new(&checkout)
                .resolve_scratch_cleanup_target(&repo_path, None, &installed)
                .unwrap(),
            installed,
            "a matching dangling symlink needs no backing cleanup"
        );
    }

    #[test]
    fn cleanup_rejects_unrelated_recorded_scratch_target() {
        let temp_dir = tempdir().expect("temporary directory should be created");
        let checkout = temp_dir.path().join("checkout");
        std::fs::create_dir(&checkout).expect("checkout directory should be created");
        let repo_path = ValidatedRepoPath::try_from(Path::new("generated/output"))
            .expect("repo-relative path should be valid");
        let resolved_target = temp_dir.path().join("scratch/generated-output");
        let unrelated_target = temp_dir.path().join("unrelated-data");
        std::fs::create_dir(&unrelated_target).expect("unrelated target should be created");

        let error = RedirectionBackingCleanupPlanner::new(&checkout)
            .resolve_scratch_cleanup_target(&repo_path, Some(&unrelated_target), &resolved_target)
            .expect_err("unrelated recorded target should be rejected");

        assert!(
            error
                .to_string()
                .contains("does not match resolved managed scratch target"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn cleanup_skips_unknown_redirections() {
        let temp_dir = tempdir().expect("temporary directory should be created");
        let redirections = BTreeMap::from([(
            PathBuf::from("stray-bind"),
            Redirection {
                repo_path: PathBuf::from("stray-bind"),
                redir_type: RedirectionType::Unknown,
                target: None,
                source: "mount".to_string(),
                state: RedirectionState::UnknownMount,
            },
        )]);

        assert!(
            plan_redirection_backing_cleanup(temp_dir.path(), &redirections)
                .expect("unknown redirections have no managed backing to plan")
                .is_empty()
        );
    }

    #[test]
    fn cleanup_does_not_treat_checkout_target_as_scratch() {
        let temp_dir = tempdir().expect("temporary directory should be created");
        let checkout = temp_dir.path().join("checkout");
        std::fs::create_dir(&checkout).expect("checkout directory should be created");
        let repo_path = ValidatedRepoPath::try_from(Path::new("generated/output"))
            .expect("repo-relative path should be valid");
        let checkout_target = checkout.join(repo_path.as_ref());
        let resolved_target = temp_dir.path().join("scratch/generated-output");

        assert_eq!(
            RedirectionBackingCleanupPlanner::new(&checkout)
                .resolve_scratch_cleanup_target(
                    &repo_path,
                    Some(&checkout_target),
                    &resolved_target,
                )
                .expect("checkout-relative APFS target should be valid"),
            resolved_target
        );
    }

    #[test]
    fn cleanup_paths_must_be_canonical_and_repo_relative() {
        for path in ["", ".", "../outside", "nested/../../outside", "/outside"] {
            let error = ValidatedRepoPath::try_from(Path::new(path))
                .expect_err("unsafe cleanup path should be rejected");
            assert!(
                error.to_string().contains("canonical path relative"),
                "unexpected error for {path}: {error:#}"
            );
        }

        let path = ValidatedRepoPath::try_from(Path::new("generated/output"))
            .expect("nested repo-relative path should be valid");
        assert_eq!(path.as_ref(), Path::new("generated/output"));

        let path = ValidatedRepoPath::try_from(Path::new("./buck-out"))
            .expect("dot-prefixed configured path should be valid");
        assert_eq!(path.as_ref(), Path::new("buck-out"));
    }

    #[cfg(unix)]
    #[test]
    fn apfs_cleanup_target_rejects_symlink_traversal() {
        let checkout = tempdir().expect("temporary checkout should be created");
        let outside = tempdir().expect("external directory should be created");
        std::fs::create_dir(outside.path().join("buck-out"))
            .expect("external target should be created");
        std::os::unix::fs::symlink(outside.path(), checkout.path().join("generated"))
            .expect("parent symlink should be created");
        let repo_path = ValidatedRepoPath::try_from(Path::new("generated/buck-out"))
            .expect("repo-relative path should be valid");

        let error = resolve_apfs_cleanup_target(checkout.path(), &repo_path)
            .expect_err("symlink traversal should be rejected");
        assert!(
            error
                .to_string()
                .contains("instead of its canonical checkout-relative location"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn scratch_cleanup_action_deletes_only_resolved_target() {
        let temp_dir = tempdir().expect("temporary directory should be created");
        let target = temp_dir.path().join("backing");
        let other_tool = temp_dir.path().join(".pyre");
        std::fs::create_dir(&target).expect("backing directory should be created");
        std::fs::write(target.join("artifact"), "data").expect("backing file should be created");
        std::fs::create_dir(&other_tool).expect("another tool's directory should be created");
        let action = scratch_cleanup_action(temp_dir.path(), target.clone())
            .expect("scratch target should be safe")
            .expect("scratch target should exist");

        action.execute().expect("scratch cleanup should succeed");

        assert!(
            !target.exists(),
            "resolved backing target should be deleted"
        );
        assert!(
            other_tool.exists(),
            "cleanup must not delete a scratch root that holds other files"
        );
    }

    #[test]
    fn scratch_cleanup_deletes_scratch_root_after_its_last_target() {
        let temp_dir = tempdir().expect("temporary directory should be created");
        let scratch = temp_dir.path().join("scratch");
        let first = scratch.join("first");
        let second = scratch.join("second");
        std::fs::create_dir_all(&first).expect("first backing directory should be created");
        std::fs::create_dir(&second).expect("second backing directory should be created");
        std::fs::write(scratch.join(SCRATCH_README), "mkscratch")
            .expect("README should be created");
        let first = scratch_cleanup_action(&scratch, first)
            .expect("scratch target should be safe")
            .expect("scratch target should exist");
        let second = scratch_cleanup_action(&scratch, second)
            .expect("scratch target should be safe")
            .expect("scratch target should exist");

        first.execute().expect("scratch cleanup should succeed");
        assert!(
            scratch.join(SCRATCH_README).exists(),
            "the scratch root stays while another target is in it"
        );
        second.execute().expect("scratch cleanup should succeed");
        assert!(
            !scratch.exists(),
            "a scratch root left with only mkscratch's README should be deleted"
        );
    }

    #[test]
    fn scratch_cleanup_deletes_mirrored_scratch_directories() {
        // mkscratch's mirror style, which Windows uses, nests a redirection's backing under
        // `edenfs/redirections` instead of flattening its name.
        let temp_dir = tempdir().expect("temporary directory should be created");
        let scratch = temp_dir.path().join("scratch");
        let target = scratch.join("edenfs").join("redirections").join("buck-out");
        std::fs::create_dir_all(&target).expect("backing directory should be created");
        std::fs::write(scratch.join(SCRATCH_README), "mkscratch")
            .expect("README should be created");
        let action = scratch_cleanup_action(&scratch, target.clone())
            .expect("scratch target should be safe")
            .expect("scratch target should exist");

        action.execute().expect("scratch cleanup should succeed");

        assert!(!target.exists(), "the backing target should be deleted");
        // FIXME: the empty `edenfs/redirections` and the scratch root should be deleted too,
        // as they are with the flat layout.
        assert!(
            scratch.join("edenfs").join("redirections").is_dir(),
            "the empty mirrored directories are left behind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scratch_contents_cleanup_keeps_target_and_does_not_follow_symlinks() {
        let temp_dir = tempdir().expect("temporary directory should be created");
        let target = temp_dir.path().join("backing");
        let outside = temp_dir.path().join("unrelated");
        std::fs::create_dir_all(target.join("nested")).expect("backing tree should be created");
        std::fs::write(target.join("nested/artifact"), "data")
            .expect("backing file should be created");
        std::fs::write(target.join("artifact"), "data").expect("backing file should be created");
        std::fs::create_dir(&outside).expect("unrelated directory should be created");
        std::fs::write(outside.join("artifact"), "keep").expect("unrelated file should be created");
        std::os::unix::fs::symlink(&outside, target.join("link"))
            .expect("symlink should be created");
        let action = scratch_cleanup_action(temp_dir.path(), target.clone())
            .expect("scratch target should be safe")
            .expect("scratch target should exist");

        action
            .execute_contents()
            .expect("contents cleanup should succeed");

        assert!(target.is_dir(), "a possibly mounted target must be kept");
        assert_eq!(
            std::fs::read_dir(&target)
                .expect("target should be readable")
                .count(),
            0,
            "every entry in the target should be deleted"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("artifact"))
                .expect("data behind a symlink must survive"),
            "keep"
        );
    }

    #[test]
    fn scratch_cleanup_deletes_read_only_files() {
        let temp_dir = tempdir().expect("temporary directory should be created");
        let target = temp_dir.path().join("target");
        let contents = temp_dir.path().join("contents");
        for backing in [&target, &contents] {
            std::fs::create_dir_all(backing.join("nested"))
                .expect("backing tree should be created");
            for file in [backing.join("artifact"), backing.join("nested/artifact")] {
                std::fs::write(&file, "data").expect("backing file should be created");
                let mut permissions = std::fs::metadata(&file)
                    .expect("backing file metadata should be readable")
                    .permissions();
                permissions.set_readonly(true);
                std::fs::set_permissions(&file, permissions)
                    .expect("backing file should become read-only");
            }
        }

        scratch_cleanup_action(temp_dir.path(), target.clone())
            .expect("scratch target should be safe")
            .expect("scratch target should exist")
            .execute()
            .expect("scratch cleanup should delete read-only files");
        scratch_cleanup_action(temp_dir.path(), contents.clone())
            .expect("scratch target should be safe")
            .expect("scratch target should exist")
            .execute_contents()
            .expect("contents cleanup should delete read-only files");

        assert!(
            !target.exists(),
            "resolved backing target should be deleted"
        );
        assert_eq!(
            std::fs::read_dir(&contents)
                .expect("target should be readable")
                .count(),
            0,
            "every entry in the target should be deleted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scratch_cleanup_rejects_symlinked_root_ancestor_and_target() {
        for link_path in ["scratch", "scratch/generated", "scratch/generated/output"] {
            let temp_dir = tempdir().expect("temporary directory should be created");
            let outside = temp_dir.path().join("unrelated");
            let suffix = Path::new("scratch/generated/output")
                .strip_prefix(link_path)
                .expect("link is an ancestor of the target");
            let external_target = outside.join(suffix);
            std::fs::create_dir_all(&external_target)
                .expect("unrelated directory should be created");
            let marker = external_target.join("artifact");
            std::fs::write(&marker, "keep").expect("unrelated file should be created");
            let link = temp_dir.path().join(link_path);
            std::fs::create_dir_all(link.parent().expect("link has a parent"))
                .expect("link parent should be created");
            std::os::unix::fs::symlink(&outside, &link).expect("symlink should be created");

            let error = scratch_cleanup_action(
                &temp_dir.path().join("scratch"),
                temp_dir.path().join("scratch/generated/output"),
            )
            .expect_err("symlink traversal should be rejected during planning");
            assert!(error.to_string().contains("without following symlinks"));
            assert_eq!(
                std::fs::read_to_string(&marker).expect("unrelated data must survive"),
                "keep"
            );
        }
    }
}
