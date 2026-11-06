/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! EdenFS utils

use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use anyhow::anyhow;
use edenfs_error::EdenFsError;
use edenfs_error::Result;
use edenfs_error::ResultExt;
use sysinfo::ProcessesToUpdate;
use tracing::trace;

pub mod hostname;
pub mod humantime;
pub mod metadata;
pub mod varint;

#[cfg(windows)]
pub mod winargv;

#[cfg(fbcode_build)]
pub use art_cli_lite_rs::outgoing_env as trace_env;

#[cfg(not(fbcode_build))]
pub fn trace_env(_command: &str) -> Vec<(String, String)> {
    Vec::new()
}

/// Converts bytes to a PathBuf.
///
/// This function attempts to convert a byte slice to a PathBuf by interpreting the bytes
/// as a UTF-8 encoded string. It's commonly used when working with paths received from
/// external sources.
///
/// # Arguments
/// * `bytes` - The byte slice to convert to a path
///
/// # Returns
/// * `Result<PathBuf>` - A PathBuf on success, or an error if the bytes contain invalid UTF-8
///
///
/// # Example
/// ```
/// use std::path::PathBuf;
///
/// use edenfs_utils::path_from_bytes;
///
/// let valid_bytes = b"/path/to/file";
/// let path = path_from_bytes(valid_bytes).unwrap();
/// assert_eq!(path, PathBuf::from("/path/to/file"));
/// ```
pub fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(std::str::from_utf8(bytes).from_err()?))
}

/// Infallible helper that converts bytes to a PathBuf using lossy conversion.
///
/// This function will replace invalid UTF-8 sequences with a replacement character.
/// It's intended to be used when converting paths that are known to be valid UTF-8, such as
/// when EdenFS returns path data via its thrift endpoints.
///
/// # Arguments
/// * `bytes` - The byte slice to convert to a path
///
/// # Returns
/// A PathBuf containing the converted string, with any invalid UTF-8 sequences replaced
pub fn path_from_bytes_lossy(bytes: &[u8]) -> PathBuf {
    String::from_utf8_lossy(bytes).to_string().into()
}

/// Converts a PathBuf to a byte vector.
///
/// This function takes a PathBuf and attempts to convert it to a UTF-8 string,
/// then to a byte vector. It will fail if the path contains non-UTF-8 characters.
///
/// # Arguments
/// * `path` - The PathBuf to convert
///
/// # Returns
/// * `Result<Vec<u8>>` - A byte vector representation of the path on success,
///   or an EdenFsError if the path contains invalid UTF-8
pub fn bytes_from_path(path: PathBuf) -> Result<Vec<u8>> {
    Ok(path
        .into_os_string()
        .into_string()
        .map_err(|e| EdenFsError::Other(anyhow!("invalid checkout path {e:?}")))?
        .as_bytes()
        .to_vec())
}

// As path_from_bytes, but returns a &Path
pub fn path_ref_from_bytes(bytes: &[u8]) -> Result<&Path> {
    Ok(Path::new(std::str::from_utf8(bytes).from_err()?))
}

const ENV_KEYS_TO_REMOVE: &[&str] = &[
    "DYLD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "PAR_LAUNCH_TIMESTAMP",
];
// some processes like hg and arc are sensitive about their environments, we
// clear variables that might make problems for their dynamic linking.
// note buck is even more sensitive see buck.run_buck_command
//
// Clean out par related environment so that we don't cause problems
// for our child process
pub fn get_environment_suitable_for_subprocess() -> Vec<(OsString, OsString)> {
    std::env::vars()
        .filter_map(|(k, v)| {
            if ENV_KEYS_TO_REMOVE.contains(&k.as_str())
                || k.starts_with("FB_PAR")
                || k.starts_with("PYTHON")
            {
                None
            } else {
                Some((k.into(), v.into()))
            }
        })
        .collect()
}

pub fn get_buck_command() -> String {
    "buck2".to_string()
}

/// Buck is sensitive to many environment variables, so we need to set them up
/// properly before calling into buck
pub fn get_env_with_buck_version(path: &Path) -> Result<Vec<(OsString, OsString)>> {
    let mut env = get_environment_suitable_for_subprocess();

    // Using BUCKVERSION=last here to avoid triggering a download of a new
    // version of buck just to kill off buck.  This is specific to Facebook's
    // deployment of buck, and has no impact on the behavior of the opensource
    // buck executable.
    let buck_version = if !cfg!(windows) {
        Ok("last".to_string())
    } else {
        // On Windows, "last" doesn't work, fallback to reading the .buck-java11 file.
        let mut version_cmd = Command::new(get_buck_command());
        version_cmd.envs(trace_env("buck"));
        version_cmd.arg("--version-fast");
        let canonical_path = path.canonicalize().from_err()?;
        #[cfg(windows)]
        let canonical_path = strip_unc_prefix(canonical_path);
        let output = version_cmd
            .current_dir(canonical_path)
            .output()
            .from_err()?;
        if output.status.success() {
            Ok(std::str::from_utf8(&output.stdout)
                .from_err()?
                .trim_end()
                .to_string())
        } else {
            Err(EdenFsError::Other(anyhow!(
                "Failed to get buck version, stderr: {}, exit status: {:?}",
                String::from_utf8_lossy(&output.stderr),
                output.status,
            )))
        }
    }?;
    env.push((OsString::from("BUCKVERSION"), OsString::from(buck_version)));
    Ok(env)
}

pub fn get_executable(pid: sysinfo::Pid) -> Option<PathBuf> {
    let mut system = sysinfo::System::new().ok()?;

    if system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true) > 0 {
        if let Some(process) = system.process(pid) {
            let executable = process.exe();
            trace!(%pid, ?executable, "found process executable");

            #[cfg(unix)]
            {
                // We may get a path ends with (deleted) if the executable is deleted on UNIX.
                let path = executable?
                    .to_str()
                    .unwrap_or("")
                    .trim_end_matches(" (deleted)");
                return Some(path.into());
            }
            #[cfg(not(unix))]
            {
                return Some(executable?.into());
            }
        } else {
            trace!(%pid, "unable to find process");
        }
    } else {
        trace!("unable to load process information");
    }

    None
}

pub fn is_buckd_running_for_repo(path: &Path) -> bool {
    let mut status_cmd = Command::new(get_buck_command());
    status_cmd.envs(trace_env("buck"));
    status_cmd.arg("status");
    let canonical_path = path.canonicalize().unwrap_or_default();
    #[cfg(windows)]
    let canonical_path = strip_unc_prefix(canonical_path);
    match status_cmd.current_dir(canonical_path).output() {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                !stdout.contains("no buckd running")
            } else {
                false
            }
        }
        Err(e) => {
            eprintln!("Error running buck2 status: {e}");
            false
        }
    }
}

/// Buck is sensitive to many environment variables, so we need to set them up
/// properly before calling into buck. Use this function to guarantee environment
/// variables are set up correctly.
pub fn run_buck_command(buck_command: &mut Command, path: &Path) -> Result<Output> {
    let buck_envs = get_env_with_buck_version(path)?;
    buck_command
        .envs(buck_envs)
        .envs(trace_env("buck"))
        .current_dir(path)
        .output()
        .from_err()
}

pub fn stop_buckd_for_repo(path: &Path) -> Result<()> {
    if is_buckd_running_for_repo(path) {
        println!("Stopping buck2 in {}...", path.display());
        let mut kill_cmd = Command::new(get_buck_command());
        kill_cmd.arg("kill");
        let canonical_path = path.canonicalize().from_err()?;
        #[cfg(windows)]
        let canonical_path = strip_unc_prefix(canonical_path);
        let out = run_buck_command(&mut kill_cmd, &canonical_path)?;
        if out.status.success() {
            Ok(())
        } else {
            Err(EdenFsError::Other(anyhow!(
                "Failed to kill buck, stderr: {}, exit status: {:?}. Please try to run `buck2 kill` manually in {}.",
                String::from_utf8_lossy(&out.stderr),
                out.status,
                path.display(),
            )))
        }
    } else {
        Ok(())
    }
}

#[cfg(windows)]
pub fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    path.to_string_lossy()
        .strip_prefix(r"\\?\")
        .map(From::from)
        .unwrap_or(path)
}

#[cfg(unix)]
/// on Unixy platforms, all symlinks are files and must be removed with std::fs::remove_file
pub fn remove_symlink(path: &Path) -> Result<()> {
    std::fs::remove_file(path).from_err()?;
    Ok(())
}

#[cfg(windows)]
/// on Windows, directory symlinks must be removed with std::fs::remove_dir instead.
pub fn remove_symlink(path: &Path) -> Result<()> {
    std::fs::remove_dir(path).from_err()?;
    Ok(())
}

#[cfg(not(any(windows, unix)))]
/// on other platforms, we don't know how to handle removing symlinks. Panic instead of guessing
pub fn remove_symlink(path: &Path) -> Result<()> {
    panic!("failed to remove symlink, unsupported platform");
}

#[cfg(windows)]
const PYTHON_CANDIDATES: &[&str] = &[
    r"c:\tools\fb-python\fb-python312",
    r"c:\tools\fb-python\fb-python310",
    r"c:\Python310",
];

// T214515756
#[cfg(windows)]
pub fn find_python() -> Option<PathBuf> {
    for candidate in PYTHON_CANDIDATES.iter() {
        let candidate = Path::new(candidate);
        let python = candidate.join("python.exe");

        if candidate.exists() && python.exists() {
            tracing::debug!("Found Python runtime at {}", python.display());
            return Some(python);
        }
    }
    None
}

#[cfg(windows)]
pub fn execute_par(par: PathBuf) -> anyhow::Result<Command> {
    let python = find_python().ok_or_else(|| {
        anyhow!(
            "Unable to find Python runtime. Paths tried:\n\n - {}",
            PYTHON_CANDIDATES.join("\n - ")
        )
    })?;
    let mut python = Command::new(python);
    python.arg(par);
    Ok(python)
}

/// Given a prefix and a list of paths, return a list of paths with the prefix prepended to each path.
///
/// If the prefix is None the paths are processed as-is.
/// All paths are post-processed with the provided function.
pub fn prefix_paths<F, T>(
    prefix: &Option<PathBuf>,
    paths: &Option<Vec<PathBuf>>,
    f: F,
) -> Option<Vec<T>>
where
    F: Fn(PathBuf) -> T,
{
    if let Some(prefix) = prefix {
        paths
            .as_ref()
            .map(|ps| ps.iter().map(|p| f(prefix.join(p))).collect::<Vec<_>>())
    } else {
        paths
            .as_ref()
            .map(|ps| ps.iter().map(|p| f(p.to_path_buf())).collect::<Vec<_>>())
    }
}

pub fn strip_prefix_from_bytes(prefix: &Option<PathBuf>, path: &[u8]) -> Vec<u8> {
    if let Some(prefix) = prefix {
        let path = Path::new(std::str::from_utf8(path).expect("Failed to convert path to string"));
        path.strip_prefix(prefix)
            .map_or(path, |stripped_path| stripped_path)
            .to_string_lossy()
            .to_string()
            .into_bytes()
    } else {
        path.to_vec()
    }
}

#[cfg(unix)]
pub fn mount_point_for_path(path: &Path) -> Option<PathBuf> {
    path.join(".eden").join("root").canonicalize().ok()
}

#[cfg(unix)]
pub fn is_active_eden_mount(path: &Path) -> bool {
    // For Linux and Mac, an active Eden mount should have a dir named ".eden" under the
    // repo root and there should be a symlink named "root" which points to the repo root

    match mount_point_for_path(path) {
        Some(resolved) => resolved == path,
        _ => false,
    }
}

#[cfg(windows)]
pub fn is_active_eden_mount(path: &Path) -> bool {
    // For Windows, an active EdenFS mount should have a dir named ".eden" under the
    // repo and there should be a file named "config" under the ".eden" dir
    let config_path = path.join(".eden").join("config");
    if !config_path.exists() {
        return false;
    }
    true
}
