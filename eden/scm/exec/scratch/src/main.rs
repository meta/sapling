/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! This is "scratch", a tool for locating and creating scratch space.
//! Why not just use the "mktemp" utility?  Scratch creates a persistent
//! and deterministic scratch location for a given input path.  This is
//! useful for holding build artifacts without having them clog up the
//! repository.  In addition, "scratch" is aware that sometimes we
//! may want to use watchman to watch a portion of the scratch space
//! and can arrange the directory structure to prevent over-watching.

use std::fs;
use std::io::prelude::*;
use std::path::Path;

use anyhow::Result;
use anyhow::bail;
#[cfg(unix)]
use anyhow::format_err;
use clap::Arg;
use clap::Command;
use mkscratch::Config;
use mkscratch::hashencode;
use mkscratch::zzencode;
#[cfg(unix)]
use nix::unistd::User;
#[cfg(unix)]
use nix::unistd::chown;
#[cfg(unix)]
use nix::unistd::geteuid;

fn run() -> Result<()> {
    let matches = Command::new("Scratch")
        .subcommand_required(true)
        .version("1.0")
        .author("Source Control <oncall+source_control@xmail.facebook.com")
        .arg(
            Arg::new("no-create")
                .long("no-create")
                .short('n')
                .action(clap::ArgAction::SetTrue)
                .help("Do not create files or directories"),
        )
        .subcommand(
            Command::new("path")
                .about("create and display the scratch path corresponding to the input path")
                .arg(
                    Arg::new("subdir")
                        .long("subdir")
                        .help("generate an isolated subdir based off this string")
                        .value_name("PATH"),
                )
                .arg(
                    Arg::new("hash")
                        .long("hash")
                        .action(clap::ArgAction::SetTrue)
                        .help("don't zzencode the path, and use a hash instead (for shorter but less intelligible paths)"),
                )
                .arg(
                    Arg::new("watchable")
                        .long("watchable")
                        .action(clap::ArgAction::SetTrue)
                        .help("the returned scratch space needs to be watchable by watchman"),
                )
                .arg(
                    Arg::new("REPO")
                        .help(
                            "Specifies the path to the repo. \
                             If omitted, infer the path from the current working directory",
                        )
                        .index(1),
                ),
        )
        .get_matches();

    let no_create = matches.get_flag("no-create");

    let config = Config::load()?;

    match matches.subcommand() {
        Some(("path", cmd)) => {
            let subdir = cmd.get_one::<String>("subdir").map(|s| s.as_str());
            let watchable = cmd.get_flag("watchable");
            let repo = cmd.get_one::<String>("REPO").map(|s| s.as_str());
            let encoder = if cmd.get_flag("hash") {
                &hashencode as _
            } else {
                &zzencode as _
            };
            path_command(&config, no_create, subdir, watchable, repo, encoder)
        }
        // subcommand_required(true) should mean that this is unpossible
        _ => unreachable!("wut?"),
    }
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(err) => {
            eprintln!("scratch failed: {err}");
            std::process::exit(1)
        }
    }
}

#[cfg(unix)]
fn set_file_owner(path: &Path, owner: &str) -> Result<()> {
    if !geteuid().is_root() {
        // Can't change the ownership, so stick with who we are
        return Ok(());
    }

    let pw = User::from_name(owner)
        .map_err(|err| format_err!("getpwnam({owner}) failed: {err}"))?
        .ok_or_else(|| format_err!("getpwnam({owner}) found no user"))?;

    if let Err(err) = chown(path, Some(pw.uid), Some(pw.gid)) {
        bail!(
            "Failed to chown({}, {} (uid={}, gid={})): {}",
            path.display(),
            owner,
            pw.uid,
            pw.gid,
            err
        );
    }

    Ok(())
}

/// This should alter the file ACLs on windows, but for now we're just
/// ignoring this, as we don't think the issue a practical problem.
#[cfg(windows)]
fn set_file_owner(_path: &Path, _owner: &str) -> Result<()> {
    Ok(())
}

/// A watchable path needs a .watchmanconfig file to define the boundary
/// of the watch and allow the watch of occur.
fn create_watchmanconfig(_config: &Config, path: &Path, repo_owner: &str) -> Result<()> {
    let filename = path.join(".watchmanconfig");
    let mut file = fs::File::create(&filename)?;
    // Write out an empty json object
    file.write_all("{}".as_bytes())?;
    set_file_owner(&filename, repo_owner)?;
    Ok(())
}

/// Checks if the scratch root path has a README.txt file.
/// If the scratch root path does not exist then successfully returns.
/// If the file exists then successfully returns.
/// If README.txt exists but it is not a file then returns an error.
/// If README.txt does not exist then attempts to create it and reports the status.
fn readme_in_scratch_path(scratch_root_path: &Path) -> Result<()> {
    if !scratch_root_path.exists() {
        return Ok(());
    }

    let readme_path = scratch_root_path.join("README.txt");

    if readme_path.exists() {
        return match readme_path.is_file() {
            true => Ok(()),
            false => Err(anyhow::anyhow!("README.txt exists but it is not a file.")),
        };
    }

    fs::File::create(readme_path)?.write_all(
        b"This directory is created to store build artifacts. \
        It is commonly used by Buck and other build systems. \
        It is okay to delete files from this directory \
        but it is recommended to clean with buck clean and similar commands.",
    )?;
    Ok(())
}

/// Performs the `path` command
fn path_command(
    config: &Config,
    no_create: bool,
    subdir: Option<&str>,
    watchable: bool,
    path: Option<&str>,
    encoder: &dyn Fn(&str) -> String,
) -> Result<()> {
    let scratch = config.resolve(
        path.map(Path::new),
        subdir.map(Path::new),
        watchable,
        encoder,
    )?;

    if !no_create {
        readme_in_scratch_path(&scratch.scratch_root)?;
        let mut ancestors = scratch.path.ancestors().collect::<Vec<_>>();
        ancestors.reverse();
        for ancestor in ancestors.iter() {
            match fs::create_dir(ancestor) {
                Ok(()) => set_file_owner(ancestor, &scratch.repo_owner)?,
                Err(_) if ancestor.is_dir() => {}
                Err(e) => bail!(e),
            }
        }
        if watchable {
            create_watchmanconfig(config, &scratch.path, &scratch.repo_owner)?;
        }
    }

    println!("{}", scratch.path.display());
    Ok(())
}
