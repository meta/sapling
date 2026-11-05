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

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::prelude::*;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use anyhow::bail;
use anyhow::format_err;
use clap::Arg;
use clap::Command;
use mkscratch::hashencode;
use mkscratch::zzencode;
#[cfg(unix)]
use nix::unistd::Uid;
#[cfg(unix)]
use nix::unistd::User;
#[cfg(unix)]
use nix::unistd::chown;
#[cfg(unix)]
use nix::unistd::geteuid;
use serde::Deserialize;

/// Configuration for scratch space style. This decides whether the directory
/// structure is kept exactly as provided subdir or not.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ScratchStyle {
    /// With flat scratch style, the sub-directories are created one-level under
    /// the repository namespace with serialized names.
    #[default]
    Flat,

    /// With mirror scratch style, the sub-directories mirror the directory
    /// hierarchy of the subdir.
    Mirror,
}

/// The configuration is intentionally very minimal, and explicitly
/// not made accessible via command line options; the intent is that
/// placement of the scratch space is the policy of the owner of the
/// environment rather than a decision made by the tool that needs
/// the scratch space.
/// Configuration is loaded by parsing the following files as TOML
/// and overlaying the values from the later files over the the
/// current parsed state:
/// * The file /etc/scratch.toml
/// * The file ~/.scratch.toml
/// * The file identified by the $SCRATCH_CONFIG_PATH environmental
///   variable.
///
/// Example configuration file might look like:
///
/// ```
/// template = "/data/users/$REPO_OWNER_USER/scratch"
/// overrides = {"/data/users/wez/fbsource": "/dev/shm/scratch"}
/// ```
#[derive(Debug, Deserialize, Default)]
struct Config {
    /// An optional "template" path.  Template paths are subject to
    /// two simple substitution transformations; $HOME is expanded
    /// to the home directory of the current user and $USER is
    /// expanded to the user name of the current user.  This allows
    /// definition of a simple placement policy without explicitly
    /// specifying the value for every user.
    /// If left unspecified, the default is `/data/users/<owner>/scratch`
    /// when that directory exists, otherwise `$HOME/.scratch`, except that
    /// root acting on another user's repository uses that user's
    /// `~/.scratch`.
    template: Option<String>,

    /// The list of overridden settings
    #[serde(default)]
    overrides: HashMap<String, String>,

    /// Scratch style. See [`ScratchStyle`]
    style: Option<ScratchStyle>,
}

/// Returns the home directory of the user as a string.
fn home_dir() -> Result<String> {
    let home = dirs::home_dir().ok_or_else(|| format_err!("unable to resolve HOME dir"))?;
    home.into_os_string()
        .into_string()
        .map_err(|home| format_err!("HOME dir {home:?} was not representable as UTF-8"))
}

#[cfg(unix)]
fn lookup_home_dir_for_user(user: &str) -> Result<String> {
    let pw = User::from_name(user)
        .map_err(|err| format_err!("getpwnam({user}) failed: {err}"))?
        .ok_or_else(|| format_err!("getpwnam({user}) found no user"))?;
    pw.dir
        .into_os_string()
        .into_string()
        .map_err(|home| format_err!("home dir {home:?} of {user} was not representable as UTF-8"))
}

/// This is technically wrong for windows, but is at least
/// wrong in a backwards compatible way
#[cfg(windows)]
fn lookup_home_dir_for_user(_user: &str) -> Result<String> {
    home_dir()
}

impl Config {
    /// Attempt to load a Config instance from the specified path.
    /// If path does not exist, None is returned.
    fn load_file<P: AsRef<Path>>(path: P) -> Result<Option<Self>> {
        let path = path.as_ref();
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(err) => match err.kind() {
                std::io::ErrorKind::NotFound => return Ok(None),
                _ => bail!(err),
            },
        };
        let mut s = String::new();
        file.read_to_string(&mut s)?;

        toml::from_str(&s)
            .map(Some)
            .map_err(|e| format_err!("error while loading TOML from {}: {:?}", path.display(), e))
    }

    /// Merge the values from other into self.
    fn merge(&mut self, mut other: Self) {
        if let Some(template) = other.template.take() {
            self.template = Some(template);
        }

        self.overrides.extend(other.overrides);

        if let Some(style) = other.style.take() {
            self.style = Some(style);
        }
    }

    /// Compute the effective configuration by loading the configuration
    /// files in order and merging them together.  Missing files are OK,
    /// but any IO or parse errors cause the config resolution to stop and
    /// return the error.
    fn load() -> Result<Self> {
        let mut result = Self::default();

        let config_files = [
            #[cfg(unix)]
            Some("/etc/scratch.toml".into()),
            #[cfg(windows)]
            Some("C:/ProgramData/facebook/scratch.toml".into()),
            Some(format!("{}/.scratch.toml", home_dir()?)),
            std::env::var("SCRATCH_CONFIG_PATH").ok(),
        ];
        for path in config_files.iter().filter_map(Option::as_ref) {
            if let Some(o) = Self::load_file(path)? {
                result.merge(o);
            }
        }

        Ok(result)
    }

    /// Look up the template string for a given repo path.
    /// This is taken from a matching `overrides` entry first, if any,
    /// then the global `template` configuration, if any, finally
    /// falling back to a default under a home directory.
    /// We use `$HOME` rather than `/tmp` as it less prone to
    /// bad actors mounting a symlink attack.
    /// `owner_home_for_root` is the repo owner's home directory when root
    /// is acting on another user's repository, and `None` otherwise.
    fn template_for_path(
        &self,
        path: &str,
        owner: &str,
        owner_home_for_root: Option<&str>,
    ) -> Result<String> {
        // First, let's see if we have an override for this path
        if let Some(over) = self.overrides.get(path) {
            return Ok(over.clone());
        }
        match &self.template {
            Some(s) => Ok(s.clone()),
            None => {
                // This is a little bit of a hack; ideally we'd
                // configure this in chef, but don't have bandwidth
                // to prepare a recipe for this in time; will follow
                // up in T31633485.
                // If there is a /data/users/<owner> dir, then we
                // use that to hold the scratch dir.
                let local = format!("/data/users/{owner}");
                if let Ok(meta) = fs::metadata(&local) {
                    if meta.is_dir() {
                        return Ok(format!("{local}/scratch"));
                    }
                }
                // Root's own home is unreachable for the repo owner, and
                // EdenFS refuses redirection sources the owner cannot
                // traverse. Root can create the scratch space in the owner's
                // home and hand it over; skip homes that do not exist.
                if let Some(home) = owner_home_for_root {
                    if fs::metadata(home).is_ok_and(|meta| meta.is_dir()) {
                        return Ok(format!("{home}/.scratch"));
                    }
                }
                // Otherwise use their home dir
                Ok(format!("{}/.scratch", home_dir()?))
            }
        }
    }
}

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

/// Returns the current username, falling back to the literal
/// string `$USER` for env var expansion.
fn get_current_user() -> String {
    let env_name = if cfg!(windows) { "USERNAME" } else { "USER" };
    env::var(env_name).unwrap_or_else(|_| "$USER".into())
}

/// Given an absolute path, locate the repository root.
fn locate_repo_root(path: &Path) -> Result<Option<&Path>> {
    for p in path.ancestors() {
        if identity::sniff_dir(p)?.is_some() || p.join(".git").exists() {
            return Ok(Some(p));
        }
    }

    Ok(None)
}

/// Given a path, return the unix name of the owner of that path.
/// If we cannot stat the path, raise an error.
#[cfg(unix)]
fn get_file_owner(path: &Path) -> Result<String> {
    let meta = fs::metadata(path)
        .map_err(|e| format_err!("unable to get metadata for {}: {}", path.display(), e))?;
    let uid = meta.uid();
    let pw = User::from_uid(Uid::from_raw(uid))
        .map_err(|err| format_err!("getpwuid({uid}) failed: {err}"))?
        .ok_or_else(|| format_err!("getpwuid({uid}) found no user"))?;

    Ok(pw.name)
}

/// Returns `repo_owner_home` when root is acting on a repository owned by
/// another user, typically through sudo.
#[cfg(unix)]
fn repo_owner_home_for_root(path: &Path, repo_owner_home: &str) -> Result<Option<String>> {
    if !geteuid().is_root() {
        return Ok(None);
    }
    let meta = fs::metadata(path)
        .map_err(|e| format_err!("unable to get metadata for {}: {}", path.display(), e))?;
    Ok((meta.uid() != 0).then(|| repo_owner_home.to_owned()))
}

#[cfg(windows)]
fn repo_owner_home_for_root(_path: &Path, _repo_owner_home: &str) -> Result<Option<String>> {
    Ok(None)
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

/// This should return the owner of a path, but for now it just returns
/// the current user name on Windows systems.  This is probably correct
/// and good enough for the moment, and we can add support for the real
/// thing in a later diff.
#[cfg(windows)]
fn get_file_owner(_path: &Path) -> Result<String> {
    Ok(get_current_user())
}

/// Resolves the root directory to use as the scratch space for a given
/// repository path.  This is the function that performs expansion of
/// the $USER and $HOME placeholder tokens in the configured template.
fn scratch_root(config: &Config, path: &Path, encoder: &dyn Fn(&str) -> String) -> Result<PathBuf> {
    let path_str = path
        .to_str()
        .ok_or_else(|| format_err!("{path:?} cannot be converted to utf8"))?;
    let repo_owner = get_file_owner(path)?;
    let repo_owner_home = lookup_home_dir_for_user(&repo_owner)?;
    let owner_home_for_root = repo_owner_home_for_root(path, &repo_owner_home)?;
    let template =
        config.template_for_path(path_str, &repo_owner, owner_home_for_root.as_deref())?;

    let user = get_current_user();
    let home = home_dir()?;

    let mut root = PathBuf::from(
        template
            .replace("$REPO_OWNER_USER", &repo_owner)
            .replace("$REPO_OWNER_HOME", &repo_owner_home)
            .replace("$USER", &user)
            .replace("$HOME", &home),
    );

    root.push(encoder(path_str));
    Ok(root)
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

/// Validates curdir parameter. When using Mirror style scratch space, it is possible to escape the
/// scratch directory by passing in path with `..` component.
fn valid_curdir(p: &Path) -> bool {
    p.components().all(|c| c != Component::ParentDir)
}

#[test]
fn test_valid_curdir() {
    assert!(valid_curdir("a/b/c".as_ref()));
    assert!(valid_curdir("/a/b/c".as_ref()));
    assert!(valid_curdir("c:\\abc\\def".as_ref()));
    assert!(valid_curdir("./abc/./abc".as_ref()));
    assert!(!valid_curdir("../abc/../abc".as_ref()));
    assert!(!valid_curdir("abc/../abc".as_ref()));
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
    // Canonicalize the provided path.  If no path was provided, fall
    // back to the cwd.
    let path = match path {
        Some(path) => fs::canonicalize(path)
            .map_err(|e| format_err!("unable to canonicalize path: {path}: {e}"))?,
        None => env::current_dir()?,
    };

    // Resolve the path to the corresponding repo root.
    // If the path is not a repo then we use the provided path.
    let repo_root = locate_repo_root(&path)?.unwrap_or(&path);

    // Get the base scratch path for this repo
    let scratch_root = scratch_root(config, repo_root, encoder)?;
    let repo_owner = get_file_owner(repo_root)?;

    // If they asked for a subdir, compute it
    let mut result = scratch_root.clone();
    if let Some(subdir) = subdir {
        if watchable {
            result.push("watchable");
        }

        if let Some(ScratchStyle::Mirror) = config.style {
            if valid_curdir(subdir.as_ref()) {
                result.push(subdir);
            } else {
                bail!("subdir path contains parent component: {subdir:?}");
            }
        } else {
            result.push(encoder(subdir));
        }
    }

    if !no_create {
        readme_in_scratch_path(&scratch_root)?;
        let mut ancestors = result.ancestors().collect::<Vec<_>>();
        ancestors.reverse();
        for ancestor in ancestors.iter() {
            match fs::create_dir(ancestor) {
                Ok(()) => set_file_owner(ancestor, &repo_owner)?,
                Err(_) if ancestor.is_dir() => {}
                Err(e) => bail!(e),
            }
        }
        if watchable {
            create_watchmanconfig(config, &result, &repo_owner)?;
        }
    }

    println!("{}", result.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_template_uses_repo_owner_home_for_root() -> Result<()> {
        let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
        let repo = temp_dir.path().join("repo");
        let owner_home = temp_dir.path().join("owner-home");
        fs::create_dir(&repo).expect("repository directory should be created");
        fs::create_dir(&owner_home).expect("owner home should be created");
        let repo = repo.to_str().expect("repository path should be UTF-8");
        let owner_home = owner_home.to_str().expect("owner home should be UTF-8");
        let missing_home = temp_dir.path().join("missing-home");
        let missing_home = missing_home.to_str().expect("missing home should be UTF-8");
        // No `/data/users/<owner>` exists for this name.
        let owner = "mkscratch-test-owner";
        let config = Config::default();

        assert_eq!(
            config.template_for_path(repo, owner, Some(owner_home))?,
            format!("{owner_home}/.scratch")
        );
        assert_eq!(
            config.template_for_path(repo, owner, Some(missing_home))?,
            format!("{}/.scratch", home_dir()?)
        );
        assert_eq!(
            config.template_for_path(repo, owner, None)?,
            format!("{}/.scratch", home_dir()?)
        );
        Ok(())
    }

    #[test]
    fn no_create_does_not_write_to_existing_scratch_namespace() {
        let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
        let repo = temp_dir.path().join("repo");
        let scratch_base = temp_dir.path().join("scratch");
        fs::create_dir(&repo).expect("repository directory should be created");

        let config = Config {
            template: Some(scratch_base.display().to_string()),
            ..Config::default()
        };
        let scratch_namespace =
            scratch_root(&config, &repo, &zzencode).expect("scratch path should resolve");
        fs::create_dir_all(&scratch_namespace)
            .expect("existing scratch namespace should be created");

        path_command(
            &config,
            true,
            Some("buck-out"),
            true,
            repo.to_str(),
            &zzencode,
        )
        .expect("read-only scratch path resolution should succeed");

        assert!(
            !scratch_namespace.join("README.txt").exists(),
            "--no-create must not create README.txt"
        );
        assert_eq!(
            fs::read_dir(&scratch_namespace)
                .expect("scratch namespace should remain readable")
                .count(),
            0,
            "--no-create must not create directories or metadata"
        );
    }
}
