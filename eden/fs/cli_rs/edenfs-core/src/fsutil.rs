/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs;
#[cfg(target_os = "windows")]
use std::os::windows::fs::FileTypeExt;
use std::path::Path;
#[cfg(target_os = "windows")]
use std::path::PathBuf;
use std::process;
use std::process::Command;

use edenfs_utils::trace_env;
use regex::Regex;
use sysinfo::Pid;
use sysinfo::System;

#[derive(Debug, Clone, PartialEq)]
pub struct FileHandleEntry {
    pub process_name: String,
    pub process_id: String,
    pub resource_type: String,
    pub path: String,
    pub kill_order: u32,
}

#[derive(Debug)]
pub enum HandleErrors {
    FormatError,
}

#[derive(Debug)]
pub struct HandleError {
    error: HandleErrors,
}

impl fmt::Display for HandleError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self.error {
            HandleErrors::FormatError => write!(f, "Handle output format error"),
        }
    }
}

impl Error for HandleError {}

#[allow(dead_code)]
fn get_kill_order(process_name: &str) -> u32 {
    // Returns the kill order of a process. We use this to sort later and kill the process in an order that minimizes inconvenience.
    // The processes listed are the ones known to almost always cause problems, so we kill them before the others.
    match process_name {
        "Hubbub.exe" => 0,
        "dotnet.exe" => 1,
        _ => 2,
    }
}

#[allow(dead_code)]
fn parse_handler_output(output: &str) -> Result<Vec<FileHandleEntry>, HandleError> {
    let re =
        Regex::new(r"^\s*(?<process_name>.+?)\s*pid: (?<pid>[0-9]+)\s*type: (?<resource_type>[^ ]*)\s*([^ ]*)\s*(.*?): (?<path>.*)").unwrap();
    let mut ret = Vec::<FileHandleEntry>::new();
    let lines: Vec<_> = output.lines().collect();
    for line in &lines {
        if let Some(caps) = re.captures(line) {
            if caps.len() == 7 {
                let new_elem = FileHandleEntry {
                    process_name: caps["process_name"].to_string(),
                    process_id: caps["pid"].to_string(),
                    resource_type: caps["resource_type"].to_string(),
                    path: caps["path"].trim().to_string(), // Trim because handle ends some lines with \r\r\n and lines() only removes \r\n
                    kill_order: get_kill_order(&caps["process_name"]),
                };
                ret.push(new_elem);
            }
        }
    }
    // We expect _some lines_ (such as the header or maybe a footer) to be present (and ignored by the Regex), but if we got more than a few
    // lines but no matches, it's possible the format changed.
    // Below, 6 is just a reasonable number - if we got more than 6 lines without data, it's probably a format change, and we should fail.
    if lines.len() > 6 && ret.is_empty() {
        return Err(HandleError {
            error: HandleErrors::FormatError,
        });
    }
    Ok(ret)
}

pub fn handle_command(mount: &Path) -> Command {
    let mut command = Command::new("handle.exe");
    command.envs(trace_env("handle.exe")).args([
        "/accepteula",
        "-nobanner",
        mount.to_str().unwrap(),
    ]);
    command
}

#[allow(dead_code)]
pub fn find_resource_locks(mount: &Path) -> Result<Vec<FileHandleEntry>, HandleError> {
    let output = handle_command(mount)
        .output()
        .expect("failed to execute handle.exe");
    let output = String::from_utf8(output.stdout).expect("Failed to decode handle.exe output");
    let mut entries = parse_handler_output(&output)?;
    entries.sort_by_key(|e| e.kill_order);
    Ok(entries)
}

#[allow(dead_code)]
pub fn get_process_tree() -> HashSet<u32> {
    let mut pid = process::id();
    let s = System::new_all();
    let mut res = HashSet::new();

    while let Some(process) = s.process(Pid::from_u32(pid)) {
        if !res.insert(process.pid().as_u32()) {
            // Prevent loops in case a previous parent PID was reused.
            break;
        }
        let Some(parent_proc) = process.parent() else {
            break;
        };
        pid = parent_proc.as_u32();
    }
    res
}

#[cfg(target_os = "windows")]
fn release_files_in_dir(_dir: &Path) -> bool {
    // TODO: Implement release using handle.exe
    false
}

#[cfg(not(target_os = "windows"))]
fn release_files_in_dir(_dir: &Path) -> bool {
    false
}

pub fn forcefully_remove_dir_all(directory: &Path) -> std::io::Result<()> {
    let mut retries = 0;
    loop {
        if !directory.try_exists()? {
            // Path doesn't exist, either as a result of a previous work or it never did, so we're done.
            return Ok(());
        }
        let res = fs::remove_dir_all(directory);
        if res.is_ok() {
            // Successfully removed the directory and its contents, so we're done.
            return Ok(());
        }
        if retries >= 3 {
            #[cfg(target_os = "windows")]
            let res = res.or_else(|err| {
                tracing::info!(
                    "deleting {} entry by entry after: {err}",
                    directory.display()
                );
                remove_dir_all_win32(directory)
            });
            return res;
        }
        release_files_in_dir(directory);
        retries += 1;
    }
}

/// `fs::remove_dir_all` fails on unmounted ProjectedFS placeholders (error 369), but
/// path-based Win32 deletion works. Like Python's Windows checkout cleanup using
/// `shutil.rmtree(..., onerror=collect_errors)`, continue past failures and report their paths.
#[cfg(target_os = "windows")]
fn remove_dir_all_win32(directory: &Path) -> std::io::Result<()> {
    let failures = remove_tree_win32(directory);
    let Some((_, first_error)) = failures.first() else {
        return Ok(());
    };
    let failed_paths = failures
        .iter()
        .map(|(path, error)| format!("{}: {error}", path.display()))
        .collect::<Vec<_>>()
        .join("; ");
    Err(std::io::Error::new(
        first_error.kind(),
        format!("failed to delete {failed_paths}"),
    ))
}

/// Returns the entries it couldn't delete, with their errors.
#[cfg(target_os = "windows")]
fn remove_tree_win32(directory: &Path) -> Vec<(PathBuf, std::io::Error)> {
    // List the whole directory before deleting from it, as `shutil.rmtree` does.
    let entries = match fs::read_dir(directory)
        .and_then(|entries| entries.collect::<std::io::Result<Vec<_>>>())
    {
        Ok(entries) => entries,
        Err(error) => return vec![(directory.to_path_buf(), error)],
    };
    let mut failures = Vec::new();
    for entry in entries {
        let path = entry.path();
        let result = match entry.file_type() {
            Ok(file_type) if file_type.is_symlink_dir() => fs::remove_dir(&path),
            Ok(file_type) if file_type.is_dir() => {
                failures.extend(remove_tree_win32(&path));
                continue;
            }
            Ok(_) => fs::remove_file(&path),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            failures.push((path, error));
        }
    }
    // Only an emptied directory can be deleted, and what is left in it is reported already.
    if failures.is_empty()
        && let Err(error) = fs::remove_dir(directory)
    {
        failures.push((directory.to_path_buf(), error));
    }
    failures
}

pub fn remove_file(path: &Path) -> std::io::Result<()> {
    if !path.try_exists()? {
        // Path doesn't exist, either as a result of a previous work or it never did, so we're done.
        return Ok(());
    }
    fs::remove_file(path)
}

#[test]
fn test_parse_handler_output_ignores_rubbish() {
    let output = "sdjkslfhslkfhsdlfkhs";
    let actual = parse_handler_output(output).unwrap();
    assert_eq!(actual.len(), 0);
}

#[test]
fn test_parse_handler_output_no_matching_handles_found() {
    let output = "Nthandle v4.22 - Handle viewer\
    Copyright (C) 1997-2019 Mark Russinovich\
    Sysinternals - www.sysinternals.com\
    \
    No matching handles found.\
";
    let actual = parse_handler_output(output).unwrap();
    assert_eq!(actual.len(), 0);
}

#[test]
fn test_parse_handler_output_parses_valid() {
    let output = r"\
    VS Code @ FB.exe   pid: 19044  type: File           34C: C:\open\fbsource2\
    Hubbub.exe         pid: 24856  type: File            40: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\
    ";
    let actual = parse_handler_output(output).unwrap();
    let expected = [
        FileHandleEntry {
            process_name: "VS Code @ FB.exe".to_string(),
            process_id: "19044".to_string(),
            resource_type: "File".to_string(),
            path: r"C:\open\fbsource2\".to_string(),
            kill_order: 2,
        },
        FileHandleEntry {
            process_name: "Hubbub.exe".to_string(),
            process_id: "24856".to_string(),
            resource_type: "File".to_string(),
            path: r"C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\".to_string(),
            kill_order: 0,
        },
    ];

    assert_eq!(actual, expected);
}

#[test]
fn test_parse_handler_output_hubhub_running() {
    //# This is a real output from handle.exe when Hubhub is running.
    let output = r"\Nthandle v4.22 - Handle viewer
    Copyright (C) 1997-2019 Mark Russinovich
    Sysinternals - www.sysinternals.com

    dotnet.exe         pid: 11744  type: File           214: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.CoreLib.dll
    dotnet.exe         pid: 11744  type: File           240: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\MSBuild.dll
    dotnet.exe         pid: 11744  type: File           244: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.dll
    dotnet.exe         pid: 11744  type: File           25C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Framework.dll
    dotnet.exe         pid: 11744  type: File           264: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Console.dll
    dotnet.exe         pid: 11744  type: File           268: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.dll
    dotnet.exe         pid: 11744  type: File           26C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.dll
    dotnet.exe         pid: 11744  type: File           270: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.dll
    dotnet.exe         pid: 11744  type: File           27C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.Extensions.dll
    dotnet.exe         pid: 11744  type: File           280: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.RegularExpressions.dll
    dotnet.exe         pid: 11744  type: File           284: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Concurrent.dll
    dotnet.exe         pid: 11744  type: File           288: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Thread.dll
    dotnet.exe         pid: 11744  type: File           28C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.dll
    dotnet.exe         pid: 11744  type: File           290: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.CodePages.dll
    dotnet.exe         pid: 11744  type: File           298: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.AccessControl.dll
    dotnet.exe         pid: 11744  type: File           29C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Tracing.dll
    dotnet.exe         pid: 11744  type: File           2A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.InteropServices.dll
    dotnet.exe         pid: 11744  type: File           2A4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Memory.dll
    dotnet.exe         pid: 11744  type: File           2AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Primitives.dll
    dotnet.exe         pid: 11744  type: File           2BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Process.dll
    dotnet.exe         pid: 11744  type: File           2C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.TraceSource.dll
    dotnet.exe         pid: 11744  type: File           2C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.Primitives.dll
    dotnet.exe         pid: 11744  type: File           2E8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Principal.Windows.dll
    dotnet.exe         pid: 11744  type: File           308: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.Pipes.dll
    dotnet.exe         pid: 11744  type: File           318: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Claims.dll
    dotnet.exe         pid: 11744  type: File           328: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Overlapped.dll
    dotnet.exe         pid: 11744  type: File           398: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.FileVersionInfo.dll
    dotnet.exe         pid: 11744  type: File           3CC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.NET.StringTools.dll
    dotnet.exe         pid: 11744  type: File           410: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.dll
    dotnet.exe         pid: 11744  type: File           490: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Loader.dll
    dotnet.exe         pid: 11744  type: File           498: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Tasks.Core.dll
    dotnet.exe         pid: 11744  type: File           49C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Immutable.dll
    dotnet.exe         pid: 11744  type: File           4B0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Dataflow.dll
    dotnet.exe         pid: 11744  type: File           4BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.ReaderWriter.dll
    dotnet.exe         pid: 11744  type: File           4C0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Uri.dll
    dotnet.exe         pid: 11744  type: File           4C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Registry.dll
    dotnet.exe         pid: 11744  type: File           4C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Utilities.Core.dll
    dotnet.exe         pid: 11744  type: File           4D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\netstandard.dll
    dotnet.exe         pid: 11744  type: File           4E8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encodings.Web.dll
    dotnet.exe         pid: 11744  type: File           4F4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Parallel.dll
    dotnet.exe         pid: 11744  type: File           500: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.Linq.dll
    dotnet.exe         pid: 11744  type: File           548: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.Lightweight.dll
    dotnet.exe         pid: 11744  type: File           550: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.ILGeneration.dll
    dotnet.exe         pid: 11744  type: File           554: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Primitives.dll
    dotnet.exe         pid: 11744  type: File           558: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Frameworks.dll
    dotnet.exe         pid: 11744  type: File           560: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.ThreadPool.dll
    dotnet.exe         pid: 11744  type: File           574: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Serialization.Primitives.dll
    dotnet.exe         pid: 11744  type: File           578: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Serialization.Formatters.dll
    dotnet.exe         pid: 11744  type: File           590: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100
    dotnet.exe         pid: 11744  type: File           5AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.MemoryMappedFiles.dll
    dotnet.exe         pid: 11744  type: File           5C0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Metadata.dll
    dotnet.exe         pid: 11744  type: File           5C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.XDocument.dll
    dotnet.exe         pid: 11744  type: File           5D4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.NonGeneric.dll
    dotnet.exe         pid: 11744  type: File           5D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ObjectModel.dll
    dotnet.exe         pid: 11744  type: File           5EC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.dll
    dotnet.exe         pid: 11744  type: File           604: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Intrinsics.dll
    dotnet.exe         pid: 11744  type: File           60C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Extensions.DependencyModel.dll
    dotnet.exe         pid: 11744  type: File           620: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Configuration.dll
    dotnet.exe         pid: 11744  type: File           624: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.LibraryModel.dll
    dotnet.exe         pid: 11744  type: File           62C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.FileSystem.dll
    dotnet.exe         pid: 11744  type: File           630: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Numerics.Vectors.dll
    dotnet.exe         pid: 11744  type: File           638: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Packaging.dll
    dotnet.exe         pid: 11744  type: File           644: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Newtonsoft.Json.dll
    dotnet.exe         pid: 11744  type: File           648: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.ProjectModel.dll
    dotnet.exe         pid: 11744  type: File           650: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.Expressions.dll
    dotnet.exe         pid: 11744  type: File           654: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.TypeConverter.dll
    dotnet.exe         pid: 11744  type: File           658: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Common.dll
    dotnet.exe         pid: 11744  type: File           65C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Versioning.dll
    dotnet.exe         pid: 11744  type: File           660: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Numerics.dll
    dotnet.exe         pid: 11744  type: File           6B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Json.dll
    dotnet.exe         pid: 11744  type: File           6DC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Algorithms.dll
    dotnet.exe         pid: 11744  type: File           6E0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Primitives.dll
    dotnet.exe         pid: 11744  type: File           764: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Resources.Writer.dll
    dotnet.exe         pid: 11744  type: File           774: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Specialized.dll
    dotnet.exe         pid: 11744  type: File           79C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\System.Resources.Extensions.dll
    dotnet.exe         pid: 11744  type: File           7B4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.NET.HostModel.dll
    dotnet.exe         pid: 11744  type: File           7C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Data.Common.dll
    dotnet.exe         pid: 11744  type: File           A5C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.dll
    dotnet.exe         pid: 11744  type: File           B1C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\System.CodeDom.dll
    dotnet.exe         pid: 31992  type: File           214: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.CoreLib.dll
    dotnet.exe         pid: 31992  type: File           254: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\MSBuild.dll
    dotnet.exe         pid: 31992  type: File           258: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.dll
    dotnet.exe         pid: 31992  type: File           268: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Framework.dll
    dotnet.exe         pid: 31992  type: File           26C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Console.dll
    dotnet.exe         pid: 31992  type: File           274: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.dll
    dotnet.exe         pid: 31992  type: File           278: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.dll
    dotnet.exe         pid: 31992  type: File           27C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.dll
    dotnet.exe         pid: 31992  type: File           288: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.Extensions.dll
    dotnet.exe         pid: 31992  type: File           28C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Thread.dll
    dotnet.exe         pid: 31992  type: File           290: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.dll
    dotnet.exe         pid: 31992  type: File           294: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.CodePages.dll
    dotnet.exe         pid: 31992  type: File           298: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Tracing.dll
    dotnet.exe         pid: 31992  type: File           29C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.InteropServices.dll
    dotnet.exe         pid: 31992  type: File           2A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.RegularExpressions.dll
    dotnet.exe         pid: 31992  type: File           2A4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Concurrent.dll
    dotnet.exe         pid: 31992  type: File           2AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Memory.dll
    dotnet.exe         pid: 31992  type: File           2B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Primitives.dll
    dotnet.exe         pid: 31992  type: File           2BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Process.dll
    dotnet.exe         pid: 31992  type: File           2C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.TraceSource.dll
    dotnet.exe         pid: 31992  type: File           2CC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.Primitives.dll
    dotnet.exe         pid: 31992  type: File           30C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.Pipes.dll
    dotnet.exe         pid: 31992  type: File           310: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.AccessControl.dll
    dotnet.exe         pid: 31992  type: File           314: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Principal.Windows.dll
    dotnet.exe         pid: 31992  type: File           31C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Claims.dll
    dotnet.exe         pid: 31992  type: File           324: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Overlapped.dll
    dotnet.exe         pid: 31992  type: File           388: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.FileVersionInfo.dll
    dotnet.exe         pid: 31992  type: File           39C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.NET.StringTools.dll
    dotnet.exe         pid: 31992  type: File           48C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Loader.dll
    dotnet.exe         pid: 31992  type: File           490: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Dataflow.dll
    dotnet.exe         pid: 31992  type: File           494: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.Lightweight.dll
    dotnet.exe         pid: 31992  type: File           498: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Immutable.dll
    dotnet.exe         pid: 31992  type: File           49C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.dll
    dotnet.exe         pid: 31992  type: File           4C0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.ReaderWriter.dll
    dotnet.exe         pid: 31992  type: File           4C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Uri.dll
    dotnet.exe         pid: 31992  type: File           4C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Registry.dll
    dotnet.exe         pid: 31992  type: File           4EC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\netstandard.dll
    dotnet.exe         pid: 31992  type: File           504: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Utilities.Core.dll
    dotnet.exe         pid: 31992  type: File           508: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Parallel.dll
    dotnet.exe         pid: 31992  type: File           534: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.FileSystem.dll
    dotnet.exe         pid: 31992  type: File           538: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.ILGeneration.dll
    dotnet.exe         pid: 31992  type: File           53C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Primitives.dll
    dotnet.exe         pid: 31992  type: File           540: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Tasks.Core.dll
    dotnet.exe         pid: 31992  type: File           54C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Numerics.Vectors.dll
    dotnet.exe         pid: 31992  type: File           550: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.ThreadPool.dll
    dotnet.exe         pid: 31992  type: File           554: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encodings.Web.dll
    dotnet.exe         pid: 31992  type: File           568: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Frameworks.dll
    dotnet.exe         pid: 31992  type: File           56C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Metadata.dll
    dotnet.exe         pid: 31992  type: File           578: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.MemoryMappedFiles.dll
    dotnet.exe         pid: 31992  type: File           57C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.NonGeneric.dll
    dotnet.exe         pid: 31992  type: File           584: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.dll
    dotnet.exe         pid: 31992  type: File           594: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.XDocument.dll
    dotnet.exe         pid: 31992  type: File           598: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.Linq.dll
    dotnet.exe         pid: 31992  type: File           5A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.ProjectModel.dll
    dotnet.exe         pid: 31992  type: File           5C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.LibraryModel.dll
    dotnet.exe         pid: 31992  type: File           5C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Common.dll
    dotnet.exe         pid: 31992  type: File           5D0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ObjectModel.dll
    dotnet.exe         pid: 31992  type: File           5D4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Versioning.dll
    dotnet.exe         pid: 31992  type: File           5D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Newtonsoft.Json.dll
    dotnet.exe         pid: 31992  type: File           5DC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.Expressions.dll
    dotnet.exe         pid: 31992  type: File           5E0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.TypeConverter.dll
    dotnet.exe         pid: 31992  type: File           5E4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Packaging.dll
    dotnet.exe         pid: 31992  type: File           5E8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Numerics.dll
    dotnet.exe         pid: 31992  type: File           5F0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Serialization.Primitives.dll
    dotnet.exe         pid: 31992  type: File           5F8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Intrinsics.dll
    dotnet.exe         pid: 31992  type: File           5FC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Configuration.dll
    dotnet.exe         pid: 31992  type: File           63C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\System.CodeDom.dll
    dotnet.exe         pid: 31992  type: File           640: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Specialized.dll
    dotnet.exe         pid: 31992  type: File           654: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Algorithms.dll
    dotnet.exe         pid: 31992  type: File           658: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Primitives.dll
    dotnet.exe         pid: 31992  type: File           664: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Extensions.DependencyModel.dll
    dotnet.exe         pid: 31992  type: File           6B0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100
    dotnet.exe         pid: 31992  type: File           6BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Json.dll
    dotnet.exe         pid: 37264  type: File           21C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.CoreLib.dll
    dotnet.exe         pid: 37264  type: File           23C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\MSBuild.dll
    dotnet.exe         pid: 37264  type: File           24C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.dll
    dotnet.exe         pid: 37264  type: File           260: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Framework.dll
    dotnet.exe         pid: 37264  type: File           268: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Console.dll
    dotnet.exe         pid: 37264  type: File           26C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.dll
    dotnet.exe         pid: 37264  type: File           270: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.dll
    dotnet.exe         pid: 37264  type: File           274: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.dll
    dotnet.exe         pid: 37264  type: File           280: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.Extensions.dll
    dotnet.exe         pid: 37264  type: File           284: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.Primitives.dll
    dotnet.exe         pid: 37264  type: File           28C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Thread.dll
    dotnet.exe         pid: 37264  type: File           290: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.dll
    dotnet.exe         pid: 37264  type: File           294: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.CodePages.dll
    dotnet.exe         pid: 37264  type: File           298: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Memory.dll
    dotnet.exe         pid: 37264  type: File           29C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Tracing.dll
    dotnet.exe         pid: 37264  type: File           2A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.InteropServices.dll
    dotnet.exe         pid: 37264  type: File           2AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.RegularExpressions.dll
    dotnet.exe         pid: 37264  type: File           2B0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Concurrent.dll
    dotnet.exe         pid: 37264  type: File           2B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Primitives.dll
    dotnet.exe         pid: 37264  type: File           2C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Process.dll
    dotnet.exe         pid: 37264  type: File           2D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.TraceSource.dll
    dotnet.exe         pid: 37264  type: File           314: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.Pipes.dll
    dotnet.exe         pid: 37264  type: File           318: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.AccessControl.dll
    dotnet.exe         pid: 37264  type: File           31C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Principal.Windows.dll
    dotnet.exe         pid: 37264  type: File           320: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Claims.dll
    dotnet.exe         pid: 37264  type: File           32C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Overlapped.dll
    dotnet.exe         pid: 37264  type: File           398: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.FileVersionInfo.dll
    dotnet.exe         pid: 37264  type: File           39C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.NET.StringTools.dll
    dotnet.exe         pid: 37264  type: File           3B0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Metadata.dll
    dotnet.exe         pid: 37264  type: File           490: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Loader.dll
    dotnet.exe         pid: 37264  type: File           494: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Immutable.dll
    dotnet.exe         pid: 37264  type: File           4A4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.ReaderWriter.dll
    dotnet.exe         pid: 37264  type: File           4A8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Dataflow.dll
    dotnet.exe         pid: 37264  type: File           4AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.dll
    dotnet.exe         pid: 37264  type: File           4B0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.Linq.dll
    dotnet.exe         pid: 37264  type: File           4C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Uri.dll
    dotnet.exe         pid: 37264  type: File           4D4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Registry.dll
    dotnet.exe         pid: 37264  type: File           4E0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Parallel.dll
    dotnet.exe         pid: 37264  type: File           4EC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Algorithms.dll
    dotnet.exe         pid: 37264  type: File           508: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\netstandard.dll
    dotnet.exe         pid: 37264  type: File           50C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Tasks.Core.dll
    dotnet.exe         pid: 37264  type: File           510: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.ThreadPool.dll
    dotnet.exe         pid: 37264  type: File           514: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Utilities.Core.dll
    dotnet.exe         pid: 37264  type: File           518: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Primitives.dll
    dotnet.exe         pid: 37264  type: File           524: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Configuration.dll
    dotnet.exe         pid: 37264  type: File           578: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.Lightweight.dll
    dotnet.exe         pid: 37264  type: File           57C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.ILGeneration.dll
    dotnet.exe         pid: 37264  type: File           5B0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Frameworks.dll
    dotnet.exe         pid: 37264  type: File           5D4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.XDocument.dll
    dotnet.exe         pid: 37264  type: File           5D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.dll
    dotnet.exe         pid: 37264  type: File           5DC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.MemoryMappedFiles.dll
    dotnet.exe         pid: 37264  type: File           5E0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.NonGeneric.dll
    dotnet.exe         pid: 37264  type: File           5E4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Json.dll
    dotnet.exe         pid: 37264  type: File           5E8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Intrinsics.dll
    dotnet.exe         pid: 37264  type: File           600: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.FileSystem.dll
    dotnet.exe         pid: 37264  type: File           610: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.ProjectModel.dll
    dotnet.exe         pid: 37264  type: File           614: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.Expressions.dll
    dotnet.exe         pid: 37264  type: File           618: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Common.dll
    dotnet.exe         pid: 37264  type: File           628: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encodings.Web.dll
    dotnet.exe         pid: 37264  type: File           63C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.TypeConverter.dll
    dotnet.exe         pid: 37264  type: File           640: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ObjectModel.dll
    dotnet.exe         pid: 37264  type: File           644: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Serialization.Primitives.dll
    dotnet.exe         pid: 37264  type: File           648: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Newtonsoft.Json.dll
    dotnet.exe         pid: 37264  type: File           64C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Primitives.dll
    dotnet.exe         pid: 37264  type: File           650: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Numerics.dll
    dotnet.exe         pid: 37264  type: File           658: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Packaging.dll
    dotnet.exe         pid: 37264  type: File           65C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Numerics.Vectors.dll
    dotnet.exe         pid: 37264  type: File           664: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.LibraryModel.dll
    dotnet.exe         pid: 37264  type: File           668: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Versioning.dll
    dotnet.exe         pid: 37264  type: File           680: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Extensions.DependencyModel.dll
    dotnet.exe         pid: 37264  type: File           6D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100
    dotnet.exe         pid: 32024  type: File           214: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.CoreLib.dll
    dotnet.exe         pid: 32024  type: File           218: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.Extensions.dll
    dotnet.exe         pid: 32024  type: File           21C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Concurrent.dll
    dotnet.exe         pid: 32024  type: File           248: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.dll
    dotnet.exe         pid: 32024  type: File           250: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\MSBuild.dll
    dotnet.exe         pid: 32024  type: File           254: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.dll
    dotnet.exe         pid: 32024  type: File           268: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Console.dll
    dotnet.exe         pid: 32024  type: File           26C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Framework.dll
    dotnet.exe         pid: 32024  type: File           278: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.dll
    dotnet.exe         pid: 32024  type: File           27C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.dll
    dotnet.exe         pid: 32024  type: File           288: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Thread.dll
    dotnet.exe         pid: 32024  type: File           28C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.dll
    dotnet.exe         pid: 32024  type: File           290: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.CodePages.dll
    dotnet.exe         pid: 32024  type: File           298: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.InteropServices.dll
    dotnet.exe         pid: 32024  type: File           2A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Tracing.dll
    dotnet.exe         pid: 32024  type: File           2A4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.RegularExpressions.dll
    dotnet.exe         pid: 32024  type: File           2A8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Primitives.dll
    dotnet.exe         pid: 32024  type: File           2B0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Process.dll
    dotnet.exe         pid: 32024  type: File           2BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Memory.dll
    dotnet.exe         pid: 32024  type: File           2D4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.TraceSource.dll
    dotnet.exe         pid: 32024  type: File           2D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.Primitives.dll
    dotnet.exe         pid: 32024  type: File           2F8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.Pipes.dll
    dotnet.exe         pid: 32024  type: File           2FC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.AccessControl.dll
    dotnet.exe         pid: 32024  type: File           300: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Principal.Windows.dll
    dotnet.exe         pid: 32024  type: File           304: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Claims.dll
    dotnet.exe         pid: 32024  type: File           30C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Overlapped.dll
    dotnet.exe         pid: 32024  type: File           384: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.FileVersionInfo.dll
    dotnet.exe         pid: 32024  type: File           388: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.NET.StringTools.dll
    dotnet.exe         pid: 32024  type: File           408: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Immutable.dll
    dotnet.exe         pid: 32024  type: File           48C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Loader.dll
    dotnet.exe         pid: 32024  type: File           490: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Dataflow.dll
    dotnet.exe         pid: 32024  type: File           4AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.ReaderWriter.dll
    dotnet.exe         pid: 32024  type: File           4B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.dll
    dotnet.exe         pid: 32024  type: File           4C0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Uri.dll
    dotnet.exe         pid: 32024  type: File           4C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Registry.dll
    dotnet.exe         pid: 32024  type: File           4D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\netstandard.dll
    dotnet.exe         pid: 32024  type: File           4FC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Utilities.Core.dll
    dotnet.exe         pid: 32024  type: File           504: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Parallel.dll
    dotnet.exe         pid: 32024  type: File           52C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.Lightweight.dll
    dotnet.exe         pid: 32024  type: File           534: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.ILGeneration.dll
    dotnet.exe         pid: 32024  type: File           538: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Primitives.dll
    dotnet.exe         pid: 32024  type: File           53C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Tasks.Core.dll
    dotnet.exe         pid: 32024  type: File           540: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Metadata.dll
    dotnet.exe         pid: 32024  type: File           548: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.ThreadPool.dll
    dotnet.exe         pid: 32024  type: File           598: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.MemoryMappedFiles.dll
    dotnet.exe         pid: 32024  type: File           59C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Frameworks.dll
    dotnet.exe         pid: 32024  type: File           5A4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.NonGeneric.dll
    dotnet.exe         pid: 32024  type: File           5AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Versioning.dll
    dotnet.exe         pid: 32024  type: File           5B4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.dll
    dotnet.exe         pid: 32024  type: File           5B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.Linq.dll
    dotnet.exe         pid: 32024  type: File           5C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.XDocument.dll
    dotnet.exe         pid: 32024  type: File           5E4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.ProjectModel.dll
    dotnet.exe         pid: 32024  type: File           5E8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Common.dll
    dotnet.exe         pid: 32024  type: File           5EC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.FileSystem.dll
    dotnet.exe         pid: 32024  type: File           5F0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Newtonsoft.Json.dll
    dotnet.exe         pid: 32024  type: File           5F4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.Expressions.dll
    dotnet.exe         pid: 32024  type: File           5F8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.TypeConverter.dll
    dotnet.exe         pid: 32024  type: File           604: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ObjectModel.dll
    dotnet.exe         pid: 32024  type: File           608: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Numerics.dll
    dotnet.exe         pid: 32024  type: File           60C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.LibraryModel.dll
    dotnet.exe         pid: 32024  type: File           614: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Packaging.dll
    dotnet.exe         pid: 32024  type: File           618: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Serialization.Primitives.dll
    dotnet.exe         pid: 32024  type: File           624: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Configuration.dll
    dotnet.exe         pid: 32024  type: File           664: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Algorithms.dll
    dotnet.exe         pid: 32024  type: File           668: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Primitives.dll
    dotnet.exe         pid: 32024  type: File           680: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Extensions.DependencyModel.dll
    dotnet.exe         pid: 32024  type: File           688: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100
    dotnet.exe         pid: 32024  type: File           6B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Json.dll
    dotnet.exe         pid: 32024  type: File           6BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encodings.Web.dll
    dotnet.exe         pid: 32024  type: File           6C0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Intrinsics.dll
    dotnet.exe         pid: 32024  type: File           6C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Numerics.Vectors.dll
    dotnet.exe         pid: 34180  type: File           218: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.CoreLib.dll
    dotnet.exe         pid: 34180  type: File           240: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\MSBuild.dll
    dotnet.exe         pid: 34180  type: File           244: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.dll
    dotnet.exe         pid: 34180  type: File           248: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.InteropServices.dll
    dotnet.exe         pid: 34180  type: File           260: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Framework.dll
    dotnet.exe         pid: 34180  type: File           264: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Console.dll
    dotnet.exe         pid: 34180  type: File           268: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.dll
    dotnet.exe         pid: 34180  type: File           270: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.dll
    dotnet.exe         pid: 34180  type: File           274: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Thread.dll
    dotnet.exe         pid: 34180  type: File           278: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.CodePages.dll
    dotnet.exe         pid: 34180  type: File           27C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.dll
    dotnet.exe         pid: 34180  type: File           288: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.Extensions.dll
    dotnet.exe         pid: 34180  type: File           28C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.Primitives.dll
    dotnet.exe         pid: 34180  type: File           290: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Tracing.dll
    dotnet.exe         pid: 34180  type: File           294: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.dll
    dotnet.exe         pid: 34180  type: File           298: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Concurrent.dll
    dotnet.exe         pid: 34180  type: File           29C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.RegularExpressions.dll
    dotnet.exe         pid: 34180  type: File           2A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Primitives.dll
    dotnet.exe         pid: 34180  type: File           2AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Memory.dll
    dotnet.exe         pid: 34180  type: File           2B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Process.dll
    dotnet.exe         pid: 34180  type: File           2C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.TraceSource.dll
    dotnet.exe         pid: 34180  type: File           300: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.Pipes.dll
    dotnet.exe         pid: 34180  type: File           304: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Claims.dll
    dotnet.exe         pid: 34180  type: File           30C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.AccessControl.dll
    dotnet.exe         pid: 34180  type: File           314: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Principal.Windows.dll
    dotnet.exe         pid: 34180  type: File           318: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Overlapped.dll
    dotnet.exe         pid: 34180  type: File           390: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.FileVersionInfo.dll
    dotnet.exe         pid: 34180  type: File           3A4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.NET.StringTools.dll
    dotnet.exe         pid: 34180  type: File           408: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Loader.dll
    dotnet.exe         pid: 34180  type: File           44C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.ThreadPool.dll
    dotnet.exe         pid: 34180  type: File           498: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Immutable.dll
    dotnet.exe         pid: 34180  type: File           4A4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Dataflow.dll
    dotnet.exe         pid: 34180  type: File           4B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.ReaderWriter.dll
    dotnet.exe         pid: 34180  type: File           4C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.dll
    dotnet.exe         pid: 34180  type: File           4D0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Registry.dll
    dotnet.exe         pid: 34180  type: File           4D4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Uri.dll
    dotnet.exe         pid: 34180  type: File           4D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Parallel.dll
    dotnet.exe         pid: 34180  type: File           500: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Tasks.Core.dll
    dotnet.exe         pid: 34180  type: File           50C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Utilities.Core.dll
    dotnet.exe         pid: 34180  type: File           520: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Algorithms.dll
    dotnet.exe         pid: 34180  type: File           524: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\netstandard.dll
    dotnet.exe         pid: 34180  type: File           540: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Primitives.dll
    dotnet.exe         pid: 34180  type: File           544: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.ILGeneration.dll
    dotnet.exe         pid: 34180  type: File           54C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.Lightweight.dll
    dotnet.exe         pid: 34180  type: File           550: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Metadata.dll
    dotnet.exe         pid: 34180  type: File           55C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Primitives.dll
    dotnet.exe         pid: 34180  type: File           570: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Serialization.Primitives.dll
    dotnet.exe         pid: 34180  type: File           574: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Packaging.dll
    dotnet.exe         pid: 34180  type: File           58C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Numerics.dll
    dotnet.exe         pid: 34180  type: File           590: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Configuration.dll
    dotnet.exe         pid: 34180  type: File           5A8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Frameworks.dll
    dotnet.exe         pid: 34180  type: File           5AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ObjectModel.dll
    dotnet.exe         pid: 34180  type: File           5B0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.ProjectModel.dll
    dotnet.exe         pid: 34180  type: File           5B4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.MemoryMappedFiles.dll
    dotnet.exe         pid: 34180  type: File           5BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.dll
    dotnet.exe         pid: 34180  type: File           5C0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.FileSystem.dll
    dotnet.exe         pid: 34180  type: File           5C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Versioning.dll
    dotnet.exe         pid: 34180  type: File           5CC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Newtonsoft.Json.dll
    dotnet.exe         pid: 34180  type: File           5D0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.NonGeneric.dll
    dotnet.exe         pid: 34180  type: File           5D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.Expressions.dll
    dotnet.exe         pid: 34180  type: File           5E8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.XDocument.dll
    dotnet.exe         pid: 34180  type: File           5EC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.Linq.dll
    dotnet.exe         pid: 34180  type: File           5F8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.LibraryModel.dll
    dotnet.exe         pid: 34180  type: File           5FC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.TypeConverter.dll
    dotnet.exe         pid: 34180  type: File           600: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Common.dll
    dotnet.exe         pid: 34180  type: File           64C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Extensions.DependencyModel.dll
    dotnet.exe         pid: 34180  type: File           680: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100
    dotnet.exe         pid: 34180  type: File           688: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Json.dll
    dotnet.exe         pid: 34180  type: File           68C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encodings.Web.dll
    dotnet.exe         pid: 34180  type: File           690: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Intrinsics.dll
    dotnet.exe         pid: 34180  type: File           694: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Numerics.Vectors.dll
    dotnet.exe         pid: 34152  type: File            40: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.NonGeneric.dll
    dotnet.exe         pid: 34152  type: File            98: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\MSBuild.dll
    dotnet.exe         pid: 34152  type: File            A8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.dll
    dotnet.exe         pid: 34152  type: File           21C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.CoreLib.dll
    dotnet.exe         pid: 34152  type: File           250: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Thread.dll
    dotnet.exe         pid: 34152  type: File           264: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.dll
    dotnet.exe         pid: 34152  type: File           268: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.CodePages.dll
    dotnet.exe         pid: 34152  type: File           26C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Framework.dll
    dotnet.exe         pid: 34152  type: File           274: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Console.dll
    dotnet.exe         pid: 34152  type: File           278: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.dll
    dotnet.exe         pid: 34152  type: File           27C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.dll
    dotnet.exe         pid: 34152  type: File           280: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.dll
    dotnet.exe         pid: 34152  type: File           284: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.Extensions.dll
    dotnet.exe         pid: 34152  type: File           288: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Tracing.dll
    dotnet.exe         pid: 34152  type: File           28C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.RegularExpressions.dll
    dotnet.exe         pid: 34152  type: File           290: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.InteropServices.dll
    dotnet.exe         pid: 34152  type: File           294: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Concurrent.dll
    dotnet.exe         pid: 34152  type: File           29C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Memory.dll
    dotnet.exe         pid: 34152  type: File           2B0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Primitives.dll
    dotnet.exe         pid: 34152  type: File           2B4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Process.dll
    dotnet.exe         pid: 34152  type: File           2C0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.TraceSource.dll
    dotnet.exe         pid: 34152  type: File           2C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.Primitives.dll
    dotnet.exe         pid: 34152  type: File           300: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.Pipes.dll
    dotnet.exe         pid: 34152  type: File           304: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Principal.Windows.dll
    dotnet.exe         pid: 34152  type: File           308: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Claims.dll
    dotnet.exe         pid: 34152  type: File           30C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.AccessControl.dll
    dotnet.exe         pid: 34152  type: File           310: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Overlapped.dll
    dotnet.exe         pid: 34152  type: File           388: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.FileVersionInfo.dll
    dotnet.exe         pid: 34152  type: File           39C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.NET.StringTools.dll
    dotnet.exe         pid: 34152  type: File           3BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ObjectModel.dll
    dotnet.exe         pid: 34152  type: File           3D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Numerics.dll
    dotnet.exe         pid: 34152  type: File           3DC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100
    dotnet.exe         pid: 34152  type: File           454: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Tasks.Core.dll
    dotnet.exe         pid: 34152  type: File           4A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Dataflow.dll
    dotnet.exe         pid: 34152  type: File           4A8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Loader.dll
    dotnet.exe         pid: 34152  type: File           4AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Immutable.dll
    dotnet.exe         pid: 34152  type: File           4B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.ReaderWriter.dll
    dotnet.exe         pid: 34152  type: File           4BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Parallel.dll
    dotnet.exe         pid: 34152  type: File           4C0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Configuration.dll
    dotnet.exe         pid: 34152  type: File           4C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Versioning.dll
    dotnet.exe         pid: 34152  type: File           4E8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.dll
    dotnet.exe         pid: 34152  type: File           4EC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Uri.dll
    dotnet.exe         pid: 34152  type: File           4F0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Registry.dll
    dotnet.exe         pid: 34152  type: File           504: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Frameworks.dll
    dotnet.exe         pid: 34152  type: File           508: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.ThreadPool.dll
    dotnet.exe         pid: 34152  type: File           50C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.LibraryModel.dll
    dotnet.exe         pid: 34152  type: File           51C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\netstandard.dll
    dotnet.exe         pid: 34152  type: File           520: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.Lightweight.dll
    dotnet.exe         pid: 34152  type: File           524: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.ILGeneration.dll
    dotnet.exe         pid: 34152  type: File           528: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Utilities.Core.dll
    dotnet.exe         pid: 34152  type: File           534: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Primitives.dll
    dotnet.exe         pid: 34152  type: File           548: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Algorithms.dll
    dotnet.exe         pid: 34152  type: File           54C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Primitives.dll
    dotnet.exe         pid: 34152  type: File           57C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.MemoryMappedFiles.dll
    dotnet.exe         pid: 34152  type: File           580: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.dll
    dotnet.exe         pid: 34152  type: File           584: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Metadata.dll
    dotnet.exe         pid: 34152  type: File           59C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.XDocument.dll
    dotnet.exe         pid: 34152  type: File           5A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.Linq.dll
    dotnet.exe         pid: 34152  type: File           5FC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Packaging.dll
    dotnet.exe         pid: 34152  type: File           60C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.ProjectModel.dll
    dotnet.exe         pid: 34152  type: File           610: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Common.dll
    dotnet.exe         pid: 34152  type: File           614: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.FileSystem.dll
    dotnet.exe         pid: 34152  type: File           61C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Newtonsoft.Json.dll
    dotnet.exe         pid: 34152  type: File           620: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.Expressions.dll
    dotnet.exe         pid: 34152  type: File           624: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.TypeConverter.dll
    dotnet.exe         pid: 34152  type: File           62C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Serialization.Primitives.dll
    dotnet.exe         pid: 34152  type: File           658: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Intrinsics.dll
    dotnet.exe         pid: 34152  type: File           670: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Json.dll
    dotnet.exe         pid: 34152  type: File           674: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encodings.Web.dll
    dotnet.exe         pid: 34152  type: File           678: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Numerics.Vectors.dll
    dotnet.exe         pid: 34152  type: File           6B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Extensions.DependencyModel.dll
    dotnet.exe         pid: 34400  type: File           218: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.CoreLib.dll
    dotnet.exe         pid: 34400  type: File           22C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\MSBuild.dll
    dotnet.exe         pid: 34400  type: File           230: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.dll
    dotnet.exe         pid: 34400  type: File           264: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Console.dll
    dotnet.exe         pid: 34400  type: File           268: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Framework.dll
    dotnet.exe         pid: 34400  type: File           270: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.dll
    dotnet.exe         pid: 34400  type: File           274: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.Extensions.dll
    dotnet.exe         pid: 34400  type: File           278: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.dll
    dotnet.exe         pid: 34400  type: File           280: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.dll
    dotnet.exe         pid: 34400  type: File           290: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Primitives.dll
    dotnet.exe         pid: 34400  type: File           294: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Thread.dll
    dotnet.exe         pid: 34400  type: File           298: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.dll
    dotnet.exe         pid: 34400  type: File           29C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Tracing.dll
    dotnet.exe         pid: 34400  type: File           2A8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.CodePages.dll
    dotnet.exe         pid: 34400  type: File           2AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.InteropServices.dll
    dotnet.exe         pid: 34400  type: File           2B0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.RegularExpressions.dll
    dotnet.exe         pid: 34400  type: File           2B4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.Primitives.dll
    dotnet.exe         pid: 34400  type: File           2BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Concurrent.dll
    dotnet.exe         pid: 34400  type: File           2C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Memory.dll
    dotnet.exe         pid: 34400  type: File           2C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Process.dll
    dotnet.exe         pid: 34400  type: File           2D4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.TraceSource.dll
    dotnet.exe         pid: 34400  type: File           2F4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.Pipes.dll
    dotnet.exe         pid: 34400  type: File           2FC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Claims.dll
    dotnet.exe         pid: 34400  type: File           304: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.AccessControl.dll
    dotnet.exe         pid: 34400  type: File           31C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Principal.Windows.dll
    dotnet.exe         pid: 34400  type: File           328: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Overlapped.dll
    dotnet.exe         pid: 34400  type: File           394: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.FileVersionInfo.dll
    dotnet.exe         pid: 34400  type: File           3B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.NET.StringTools.dll
    dotnet.exe         pid: 34400  type: File           3E0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.Lightweight.dll
    dotnet.exe         pid: 34400  type: File           494: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Loader.dll
    dotnet.exe         pid: 34400  type: File           498: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Intrinsics.dll
    dotnet.exe         pid: 34400  type: File           4A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Immutable.dll
    dotnet.exe         pid: 34400  type: File           4B0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Registry.dll
    dotnet.exe         pid: 34400  type: File           4B4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Dataflow.dll
    dotnet.exe         pid: 34400  type: File           4BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.ReaderWriter.dll
    dotnet.exe         pid: 34400  type: File           4C0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.dll
    dotnet.exe         pid: 34400  type: File           4C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Uri.dll
    dotnet.exe         pid: 34400  type: File           4C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Parallel.dll
    dotnet.exe         pid: 34400  type: File           4E0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\netstandard.dll
    dotnet.exe         pid: 34400  type: File           4E4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Tasks.Core.dll
    dotnet.exe         pid: 34400  type: File           500: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Utilities.Core.dll
    dotnet.exe         pid: 34400  type: File           540: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Metadata.dll
    dotnet.exe         pid: 34400  type: File           544: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.ILGeneration.dll
    dotnet.exe         pid: 34400  type: File           548: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Primitives.dll
    dotnet.exe         pid: 34400  type: File           54C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.MemoryMappedFiles.dll
    dotnet.exe         pid: 34400  type: File           554: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.ThreadPool.dll
    dotnet.exe         pid: 34400  type: File           558: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Primitives.dll
    dotnet.exe         pid: 34400  type: File           560: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encodings.Web.dll
    dotnet.exe         pid: 34400  type: File           56C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.XDocument.dll
    dotnet.exe         pid: 34400  type: File           570: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Frameworks.dll
    dotnet.exe         pid: 34400  type: File           574: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.NonGeneric.dll
    dotnet.exe         pid: 34400  type: File           578: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.Linq.dll
    dotnet.exe         pid: 34400  type: File           58C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.dll
    dotnet.exe         pid: 34400  type: File           590: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.Expressions.dll
    dotnet.exe         pid: 34400  type: File           5A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Newtonsoft.Json.dll
    dotnet.exe         pid: 34400  type: File           5A4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Versioning.dll
    dotnet.exe         pid: 34400  type: File           5B4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.ProjectModel.dll
    dotnet.exe         pid: 34400  type: File           5C0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Common.dll
    dotnet.exe         pid: 34400  type: File           5C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.FileSystem.dll
    dotnet.exe         pid: 34400  type: File           5C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.TypeConverter.dll
    dotnet.exe         pid: 34400  type: File           5CC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ObjectModel.dll
    dotnet.exe         pid: 34400  type: File           5D0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Numerics.dll
    dotnet.exe         pid: 34400  type: File           5DC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Packaging.dll
    dotnet.exe         pid: 34400  type: File           5E0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Serialization.Primitives.dll
    dotnet.exe         pid: 34400  type: File           5E4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.LibraryModel.dll
    dotnet.exe         pid: 34400  type: File           5E8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Configuration.dll
    dotnet.exe         pid: 34400  type: File           630: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100
    dotnet.exe         pid: 34400  type: File           638: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Algorithms.dll
    dotnet.exe         pid: 34400  type: File           664: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Numerics.Vectors.dll
    dotnet.exe         pid: 34400  type: File           668: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Extensions.DependencyModel.dll
    dotnet.exe         pid: 34400  type: File           670: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Json.dll
    dotnet.exe         pid: 18120  type: File            40: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Frameworks.dll
    dotnet.exe         pid: 18120  type: File           214: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.CoreLib.dll
    dotnet.exe         pid: 18120  type: File           240: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.dll
    dotnet.exe         pid: 18120  type: File           248: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\MSBuild.dll
    dotnet.exe         pid: 18120  type: File           25C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Framework.dll
    dotnet.exe         pid: 18120  type: File           264: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Console.dll
    dotnet.exe         pid: 18120  type: File           268: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.dll
    dotnet.exe         pid: 18120  type: File           26C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.dll
    dotnet.exe         pid: 18120  type: File           270: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.dll
    dotnet.exe         pid: 18120  type: File           278: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.Extensions.dll
    dotnet.exe         pid: 18120  type: File           27C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.dll
    dotnet.exe         pid: 18120  type: File           280: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.CodePages.dll
    dotnet.exe         pid: 18120  type: File           284: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Thread.dll
    dotnet.exe         pid: 18120  type: File           288: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Tracing.dll
    dotnet.exe         pid: 18120  type: File           28C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.RegularExpressions.dll
    dotnet.exe         pid: 18120  type: File           290: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.InteropServices.dll
    dotnet.exe         pid: 18120  type: File           294: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Primitives.dll
    dotnet.exe         pid: 18120  type: File           298: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Concurrent.dll
    dotnet.exe         pid: 18120  type: File           29C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Process.dll
    dotnet.exe         pid: 18120  type: File           2A8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Memory.dll
    dotnet.exe         pid: 18120  type: File           2C0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.TraceSource.dll
    dotnet.exe         pid: 18120  type: File           2C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.Primitives.dll
    dotnet.exe         pid: 18120  type: File           300: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.Pipes.dll
    dotnet.exe         pid: 18120  type: File           30C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.AccessControl.dll
    dotnet.exe         pid: 18120  type: File           310: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Overlapped.dll
    dotnet.exe         pid: 18120  type: File           318: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Principal.Windows.dll
    dotnet.exe         pid: 18120  type: File           324: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Claims.dll
    dotnet.exe         pid: 18120  type: File           398: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.FileVersionInfo.dll
    dotnet.exe         pid: 18120  type: File           3A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.NET.StringTools.dll
    dotnet.exe         pid: 18120  type: File           3D4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Configuration.dll
    dotnet.exe         pid: 18120  type: File           478: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.FileSystem.dll
    dotnet.exe         pid: 18120  type: File           49C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.dll
    dotnet.exe         pid: 18120  type: File           4A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Loader.dll
    dotnet.exe         pid: 18120  type: File           4A4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Immutable.dll
    dotnet.exe         pid: 18120  type: File           4A8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.ReaderWriter.dll
    dotnet.exe         pid: 18120  type: File           4AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Dataflow.dll
    dotnet.exe         pid: 18120  type: File           4B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Registry.dll
    dotnet.exe         pid: 18120  type: File           4BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Uri.dll
    dotnet.exe         pid: 18120  type: File           4F8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.ThreadPool.dll
    dotnet.exe         pid: 18120  type: File           500: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.Lightweight.dll
    dotnet.exe         pid: 18120  type: File           504: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\netstandard.dll
    dotnet.exe         pid: 18120  type: File           508: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Primitives.dll
    dotnet.exe         pid: 18120  type: File           50C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Common.dll
    dotnet.exe         pid: 18120  type: File           518: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Utilities.Core.dll
    dotnet.exe         pid: 18120  type: File           524: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Parallel.dll
    dotnet.exe         pid: 18120  type: File           560: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.ILGeneration.dll
    dotnet.exe         pid: 18120  type: File           56C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Tasks.Core.dll
    dotnet.exe         pid: 18120  type: File           570: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.ProjectModel.dll
    dotnet.exe         pid: 18120  type: File           574: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Intrinsics.dll
    dotnet.exe         pid: 18120  type: File           5A4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Numerics.dll
    dotnet.exe         pid: 18120  type: File           5A8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Metadata.dll
    dotnet.exe         pid: 18120  type: File           5AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.MemoryMappedFiles.dll
    dotnet.exe         pid: 18120  type: File           5B0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.NonGeneric.dll
    dotnet.exe         pid: 18120  type: File           5B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.dll
    dotnet.exe         pid: 18120  type: File           5BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encodings.Web.dll
    dotnet.exe         pid: 18120  type: File           5C0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.XDocument.dll
    dotnet.exe         pid: 18120  type: File           5C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.Linq.dll
    dotnet.exe         pid: 18120  type: File           5CC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.TypeConverter.dll
    dotnet.exe         pid: 18120  type: File           5D4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Primitives.dll
    dotnet.exe         pid: 18120  type: File           5E0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Serialization.Primitives.dll
    dotnet.exe         pid: 18120  type: File           600: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.LibraryModel.dll
    dotnet.exe         pid: 18120  type: File           604: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Packaging.dll
    dotnet.exe         pid: 18120  type: File           614: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Algorithms.dll
    dotnet.exe         pid: 18120  type: File           618: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Versioning.dll
    dotnet.exe         pid: 18120  type: File           61C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Newtonsoft.Json.dll
    dotnet.exe         pid: 18120  type: File           620: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.Expressions.dll
    dotnet.exe         pid: 18120  type: File           624: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ObjectModel.dll
    dotnet.exe         pid: 18120  type: File           65C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Extensions.DependencyModel.dll
    dotnet.exe         pid: 18120  type: File           660: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Numerics.Vectors.dll
    dotnet.exe         pid: 18120  type: File           664: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Json.dll
    dotnet.exe         pid: 18120  type: File           66C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100
    dotnet.exe         pid: 42572  type: File            40: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Algorithms.dll
    dotnet.exe         pid: 42572  type: File           218: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.CoreLib.dll
    dotnet.exe         pid: 42572  type: File           228: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\netstandard.dll
    dotnet.exe         pid: 42572  type: File           238: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\MSBuild.dll
    dotnet.exe         pid: 42572  type: File           248: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.dll
    dotnet.exe         pid: 42572  type: File           25C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Framework.dll
    dotnet.exe         pid: 42572  type: File           264: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Console.dll
    dotnet.exe         pid: 42572  type: File           268: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.dll
    dotnet.exe         pid: 42572  type: File           26C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.dll
    dotnet.exe         pid: 42572  type: File           270: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.dll
    dotnet.exe         pid: 42572  type: File           27C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.Extensions.dll
    dotnet.exe         pid: 42572  type: File           280: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.Encoding.CodePages.dll
    dotnet.exe         pid: 42572  type: File           288: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Thread.dll
    dotnet.exe         pid: 42572  type: File           28C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.dll
    dotnet.exe         pid: 42572  type: File           294: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Tracing.dll
    dotnet.exe         pid: 42572  type: File           298: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.InteropServices.dll
    dotnet.exe         pid: 42572  type: File           29C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Text.RegularExpressions.dll
    dotnet.exe         pid: 42572  type: File           2A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Concurrent.dll
    dotnet.exe         pid: 42572  type: File           2A8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Memory.dll
    dotnet.exe         pid: 42572  type: File           2B4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Primitives.dll
    dotnet.exe         pid: 42572  type: File           2B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.Process.dll
    dotnet.exe         pid: 42572  type: File           2C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.TraceSource.dll
    dotnet.exe         pid: 42572  type: File           2C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.Primitives.dll
    dotnet.exe         pid: 42572  type: File           304: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.Pipes.dll
    dotnet.exe         pid: 42572  type: File           308: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.AccessControl.dll
    dotnet.exe         pid: 42572  type: File           314: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Principal.Windows.dll
    dotnet.exe         pid: 42572  type: File           318: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Claims.dll
    dotnet.exe         pid: 42572  type: File           324: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Overlapped.dll
    dotnet.exe         pid: 42572  type: File           39C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Diagnostics.FileVersionInfo.dll
    dotnet.exe         pid: 42572  type: File           3BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.NET.StringTools.dll
    dotnet.exe         pid: 42572  type: File           3E0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.ProjectModel.dll
    dotnet.exe         pid: 42572  type: File           488: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Loader.dll
    dotnet.exe         pid: 42572  type: File           494: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.Immutable.dll
    dotnet.exe         pid: 42572  type: File           498: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Dataflow.dll
    dotnet.exe         pid: 42572  type: File           4A8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.ReaderWriter.dll
    dotnet.exe         pid: 42572  type: File           4AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.dll
    dotnet.exe         pid: 42572  type: File           4B0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\Microsoft.Win32.Registry.dll
    dotnet.exe         pid: 42572  type: File           4B4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Uri.dll
    dotnet.exe         pid: 42572  type: File           4B8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.Tasks.Parallel.dll
    dotnet.exe         pid: 42572  type: File           4D4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Utilities.Core.dll
    dotnet.exe         pid: 42572  type: File           4D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.dll
    dotnet.exe         pid: 42572  type: File           4E4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Tasks.Core.dll
    dotnet.exe         pid: 42572  type: File           4E8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.Lightweight.dll
    dotnet.exe         pid: 42572  type: File           4EC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Emit.ILGeneration.dll
    dotnet.exe         pid: 42572  type: File           4F0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Primitives.dll
    dotnet.exe         pid: 42572  type: File           4F8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Xml.XDocument.dll
    dotnet.exe         pid: 42572  type: File           500: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.Xml.Linq.dll
    dotnet.exe         pid: 42572  type: File           50C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Security.Cryptography.Primitives.dll
    dotnet.exe         pid: 42572  type: File           560: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Reflection.Metadata.dll
    dotnet.exe         pid: 42572  type: File           590: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Versioning.dll
    dotnet.exe         pid: 42572  type: File           5A0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Serialization.Primitives.dll
    dotnet.exe         pid: 42572  type: File           5AC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Common.dll
    dotnet.exe         pid: 42572  type: File           5C0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.Numerics.dll
    dotnet.exe         pid: 42572  type: File           5C4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.MemoryMappedFiles.dll
    dotnet.exe         pid: 42572  type: File           5C8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.NonGeneric.dll
    dotnet.exe         pid: 42572  type: File           5CC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Frameworks.dll
    dotnet.exe         pid: 42572  type: File           5D0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Linq.Expressions.dll
    dotnet.exe         pid: 42572  type: File           5D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ObjectModel.dll
    dotnet.exe         pid: 42572  type: File           5DC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.IO.FileSystem.dll
    dotnet.exe         pid: 42572  type: File           5E0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.ComponentModel.TypeConverter.dll
    dotnet.exe         pid: 42572  type: File           5E4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Packaging.dll
    dotnet.exe         pid: 42572  type: File           5E8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Newtonsoft.Json.dll
    dotnet.exe         pid: 42572  type: File           5EC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.LibraryModel.dll
    dotnet.exe         pid: 42572  type: File           5F0: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\NuGet.Configuration.dll
    dotnet.exe         pid: 42572  type: File           5F4: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.ThreadPool.dll
    dotnet.exe         pid: 42572  type: File           6D8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100
    dotnet.exe         pid: 29748  type: File            40: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Roslyn\bincore
    dotnet.exe         pid: 29748  type: File           338: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Roslyn\bincore\VBCSCompiler.dll
    dotnet.exe         pid: 29748  type: File           380: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Roslyn\bincore\Microsoft.CodeAnalysis.dll
    dotnet.exe         pid: 29748  type: File           4BC: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Roslyn\bincore\Microsoft.CodeAnalysis.CSharp.dll
    dotnet.exe         pid: 29748  type: File           4E8: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Roslyn\bincore\Microsoft.CodeAnalysis.VisualBasic.dll
    Hubbub.exe         pid: 23460  type: File            40: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub
    Hubbub.exe         pid: 23460  type: File           278: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\Hubbub.dll
    Hubbub.exe         pid: 23460  type: File           2B4: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\Facebook.Xplat.Threading.dll
    Hubbub.exe         pid: 23460  type: File           2B8: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\socialvr-core.dll
    Hubbub.exe         pid: 23460  type: File           2C8: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\devtools.dll
    Hubbub.exe         pid: 23460  type: File           2D0: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\socialvr-packages.dll
    Hubbub.exe         pid: 23460  type: File           2D8: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\socialvr-analytics-events.dll
    Hubbub.exe         pid: 23460  type: File           504: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\Newtonsoft.Json.dll
    Hubbub.exe         pid: 23460  type: File           55C: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\UnityEngine.dll
    Hubbub.exe         pid: 23460  type: File           590: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\GraphClientProvider.dll
    Hubbub.exe         pid: 23460  type: File           718: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\System.Data.SQLite.dll
    Hubbub.exe         pid: 23460  type: File           7C0: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\GraphClient.dll
    Hubbub.exe         pid: 23460  type: File           820: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\BouncyCastle.Crypto.dll
    Hubbub.exe         pid: 23460  type: File           870: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\GraphQLClient.dll
    Hubbub.exe         pid: 23460  type: File           878: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\FBGraphQLTransports.dll
    Hubbub.exe         pid: 23460  type: File           AFC: C:\open\fbsource\.hg
    Hubbub.exe         pid: 23460  type: File           BB8: C:\open\fbsource\ovrsource-legacy\unity\socialvr\_tools\hubbub\Hubbub\bin\Debug\net6.0-windows\runtimes\win\lib\net6.0\System.ServiceProcess.ServiceController.dll
    adb.exe            pid: 8648   type: File            84: C:\open\fbsource\ovrsource-legacy
    ";
    let actual = parse_handler_output(output).unwrap();
    assert_eq!(
        actual
            .iter()
            .filter(|x| x.process_name == "dotnet.exe")
            .collect::<Vec<_>>()
            .len(),
        649
    );
    assert_eq!(
        actual
            .iter()
            .filter(|x| x.process_name == "Hubbub.exe")
            .collect::<Vec<_>>()
            .len(),
        17
    );
}

#[test]
fn test_parse_handler_output_detect_format_change() {
    //# This is a real output from handle.exe when Hubhub is running.
    let output = r"\Nthandle v4.22 - Handle viewer
    Copyright (C) 1997-2019 Mark Russinovich
    Sysinternals - www.sysinternals.com

    dotnet.exe         type: File           214: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Private.CoreLib.dll
    dotnet.exe         type: File           240: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\MSBuild.dll
    dotnet.exe         type: File           244: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Runtime.dll
    dotnet.exe         type: File           25C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\sdk\8.0.100\Microsoft.Build.Framework.dll
    dotnet.exe         type: File           264: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Console.dll
    dotnet.exe         type: File           268: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Threading.dll
    dotnet.exe         type: File           26C: C:\open\fbsource\arvr\projects\socialvr\third-party\dotnet\win-x64\shared\Microsoft.NETCore.App\8.0.0\System.Collections.dll
    ";
    let actual = parse_handler_output(output);
    assert!(actual.is_err());
}

#[cfg(target_os = "windows")]
#[test]
fn remove_dir_all_win32_deletes_junctions_without_following_them() {
    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let outside = temp_dir.path().join("outside");
    let tree = temp_dir.path().join("tree");
    fs::create_dir(&outside).expect("outside directory should be created");
    fs::write(outside.join("kept"), b"").expect("outside file should be created");
    fs::create_dir_all(tree.join("dir")).expect("tree should be created");
    fs::write(tree.join("dir").join("file"), b"").expect("tree file should be created");
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(tree.join("junction"))
        .arg(&outside)
        .status()
        .expect("mklink should run");
    assert!(status.success(), "mklink /J should create a junction");

    remove_dir_all_win32(&tree).expect("the tree should be deleted");

    assert!(!tree.exists(), "the tree should be deleted");
    assert!(
        outside.join("kept").exists(),
        "the junction's target should be kept"
    );
}

#[cfg(target_os = "windows")]
#[test]
fn forcefully_remove_dir_all_deletes_past_a_file_held_open_and_names_it() {
    use std::os::windows::fs::OpenOptionsExt;

    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let tree = temp_dir.path().join("tree");
    let held = tree.join("a").join("held");
    let read_only = tree.join("b").join("read-only");
    fs::create_dir_all(tree.join("a")).expect("tree should be created");
    fs::create_dir_all(tree.join("b")).expect("tree should be created");
    fs::write(&held, b"").expect("held file should be created");
    fs::write(&read_only, b"").expect("read-only file should be created");
    let mut permissions = fs::metadata(&read_only)
        .expect("read-only file should exist")
        .permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&read_only, permissions).expect("file should be made read-only");
    let _handle = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&held)
        .expect("held file should open");

    let error = forcefully_remove_dir_all(&tree).expect_err("a file held open can't be deleted");

    assert!(
        error.to_string().contains(&held.display().to_string()),
        "the error should name the held file: {error}"
    );
    assert!(
        !tree.join("b").exists(),
        "the entries after the held file, read-only ones too, should be deleted"
    );
}

#[test]
fn test_get_process_tree() {
    // We can't test with guaranteed data since it changes per run, but at least do a sanity check and confirm that our own PID and our parent's is on the list.
    let ancestors = get_process_tree();
    let my_pid = process::id();
    assert!(ancestors.contains(&my_pid));
    let s = System::new_all();
    let process = s.process(Pid::from_u32(my_pid)).unwrap();
    assert!(ancestors.contains(&process.parent().unwrap().as_u32()));
}
