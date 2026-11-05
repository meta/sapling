/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

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
#[cfg(unix)]
use nix::unistd::Uid;
#[cfg(unix)]
use nix::unistd::User;
#[cfg(unix)]
use nix::unistd::geteuid;
use serde::Deserialize;
use sha2::Sha256;
use sha2::digest::Digest;

/// Normalizes various path format on Windows. This function will convert
/// various Windows path format to full path form. Note this function does not
/// canonicalize the given path. So it does not collapse dots nor expand
/// relative paths.
/// Ref: https://googleprojectzero.blogspot.com/2016/02/the-definitive-guide-on-win32-to-nt.html
#[cfg(windows)]
fn normalize_windows_path(path: &str) -> String {
    let path = path.replace("/", r"\");
    if let Some(path) = path.strip_prefix(r"\??\UNC\") {
        // NT UNC path
        format!(r"\\{}", path)
    } else if let Some(path) = path.strip_prefix(r"\??\") {
        // NT path
        path.to_owned()
    } else if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        // Extend-length UNC path
        format!(r"\\{}", path)
    } else if let Some(path) = path.strip_prefix(r"\\?\") {
        // Extend-length path
        path.to_owned()
    } else {
        path
    }
}

/// Given a string representation of a path, encode it such that all
/// file/path special characters are replaced with non-special characters.
/// This has the effect of flattening a relative path fragment like
/// `foo/bar` into a single level path component like `fooZbar`.
/// Scratch uses this to give the appearance of hierarchy to clients
/// without having an actual hierarchy.  This is important on systems
/// such as Windows and macOS where the filesystem watchers are always
/// recursive.
/// The mapping is not and does not need to be reversible.
/// Why not just compute a SHA or MD5 hash?  It is nicer for the user
/// to have an idea of what the path is when they list the scratch container
/// path, which is something they'll likely end up doing when their disk
/// gets full, and they'll appreciate knowing which of these dirs have
/// value to them.
pub fn zzencode(path: &str) -> String {
    let mut result = String::with_capacity(path.len());

    // `std::fs::canonicalize` on Windows will normalize path into
    // extended-length format, which has a prefix `\\?\`. This function will
    // incorrect generate a path with the question mark which is an invalid
    // path.
    #[cfg(windows)]
    let path = &normalize_windows_path(path);

    for (i, b) in path.chars().enumerate() {
        if cfg!(unix) && i == 0 && b == '/' {
            // On unix, most paths begin with a slash, which
            // means that we'd use a Z prefix everything.
            // Let's just skip the first character.
            continue;
        }
        match b {
            '/' | '\\' => result.push('Z'),
            'Z' => result.push_str("_Z"),
            ':' => result.push('_'),
            _ => result.push(b),
        }
    }

    result
}

/// When the user needs a shorter path, we allow them to get a hash instead of zz-encoding.
pub fn hashencode(path: &str) -> String {
    #[cfg(windows)]
    let path = &normalize_windows_path(path);

    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());

    let hash = hasher.finalize();

    // Don't take the full hash, since the point is to be shorter.
    hex::encode(&hash[0..8])
}

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
/// ```toml
/// template = "/data/users/$REPO_OWNER_USER/scratch"
/// overrides = {"/data/users/wez/fbsource": "/data/users/wez/scratch-fbsource"}
/// ```
#[derive(Debug, Deserialize, Default)]
pub struct Config {
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

/// A scratch location, computed without creating anything.
pub struct ScratchPath {
    /// The scratch root of the input path's repository.
    pub scratch_root: PathBuf,
    /// `scratch_root` extended by the requested subdir, if any.
    pub path: PathBuf,
    /// The repository's owner, who should own anything created for it.
    pub repo_owner: String,
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
    pub fn load() -> Result<Self> {
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

    /// Computes the scratch location of `path`, or of the current directory
    /// when `path` is `None`, without creating anything. A `subdir` gets an
    /// isolated location within the repository's scratch root.
    pub fn resolve(
        &self,
        path: Option<&Path>,
        subdir: Option<&Path>,
        watchable: bool,
        encoder: &dyn Fn(&str) -> String,
    ) -> Result<ScratchPath> {
        // Canonicalize the provided path.  If no path was provided, fall
        // back to the cwd.
        let path = match path {
            Some(path) => fs::canonicalize(path)
                .map_err(|e| format_err!("unable to canonicalize path: {}: {e}", path.display()))?,
            None => env::current_dir()?,
        };

        // Resolve the path to the corresponding repo root.
        // If the path is not a repo then we use the provided path.
        let repo_root = locate_repo_root(&path)?.unwrap_or(&path);

        self.resolve_repo_root(repo_root, repo_root, subdir, watchable, encoder)
    }

    /// Computes the scratch location of the repository at `repo_root`, a
    /// canonical path, without reading `repo_root`, so it needn't exist.
    /// The repository's owner is the owner of `owner_path` instead.
    pub fn resolve_repo_root(
        &self,
        repo_root: &Path,
        owner_path: &Path,
        subdir: Option<&Path>,
        watchable: bool,
        encoder: &dyn Fn(&str) -> String,
    ) -> Result<ScratchPath> {
        // Get the base scratch path for this repo
        let scratch_root = scratch_root(self, repo_root, owner_path, encoder)?;
        let repo_owner = get_file_owner(owner_path)?;

        // If they asked for a subdir, compute it
        let mut result = scratch_root.clone();
        if let Some(subdir) = subdir {
            if watchable {
                result.push("watchable");
            }

            if let Some(ScratchStyle::Mirror) = self.style {
                if valid_curdir(subdir) {
                    result.push(subdir);
                } else {
                    bail!("subdir path contains parent component: {subdir:?}");
                }
            } else {
                let subdir = subdir
                    .to_str()
                    .ok_or_else(|| format_err!("{subdir:?} cannot be converted to utf8"))?;
                result.push(encoder(subdir));
            }
        }

        Ok(ScratchPath {
            scratch_root,
            path: result,
            repo_owner,
        })
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

/// This should return the owner of a path, but for now it just returns
/// the current user name on Windows systems.  This is probably correct
/// and good enough for the moment, and we can add support for the real
/// thing in a later diff.
#[cfg(windows)]
fn get_file_owner(_path: &Path) -> Result<String> {
    Ok(get_current_user())
}

/// Resolves the root directory to use as the scratch space for a given
/// repository path, owned by the owner of `owner_path`.  This is the function
/// that performs expansion of the $USER and $HOME placeholder tokens in the
/// configured template.
fn scratch_root(
    config: &Config,
    path: &Path,
    owner_path: &Path,
    encoder: &dyn Fn(&str) -> String,
) -> Result<PathBuf> {
    let path_str = path
        .to_str()
        .ok_or_else(|| format_err!("{path:?} cannot be converted to utf8"))?;
    let repo_owner = get_file_owner(owner_path)?;
    let repo_owner_home = lookup_home_dir_for_user(&repo_owner)?;
    let owner_home_for_root = repo_owner_home_for_root(owner_path, &repo_owner_home)?;
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

/// Validates curdir parameter. When using Mirror style scratch space, it is possible to escape the
/// scratch directory by passing in path with `..` component.
fn valid_curdir(p: &Path) -> bool {
    p.components().all(|c| c != Component::ParentDir)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_zzencode() {
        if cfg!(unix) {
            assert_eq!(zzencode("/foo/bar"), "fooZbar");
        } else {
            assert_eq!(zzencode("/foo/bar"), "ZfooZbar");
        }
        assert_eq!(zzencode("foo"), "foo");
        assert_eq!(zzencode("foo/bar"), "fooZbar");
        assert_eq!(zzencode(r"foo\bar"), "fooZbar");
        assert_eq!(zzencode("fooZbar"), "foo_Zbar");
        assert_eq!(zzencode("foo_Zbar"), "foo__Zbar");
        assert_eq!(zzencode(r"C:\foo\bar"), "C_ZfooZbar");
        assert_eq!(zzencode(r"\\unc\path"), "ZZuncZpath");

        if cfg!(windows) {
            assert_eq!(zzencode(r"\\?\C:\foo\bar"), "C_ZfooZbar");
            assert_eq!(zzencode(r"\\?\UNC\unc\path"), "ZZuncZpath");
            assert_eq!(zzencode(r"\??\C:\foo\bar"), "C_ZfooZbar");
            assert_eq!(zzencode(r"\??\UNC\unc\path"), "ZZuncZpath");
        }
    }

    #[cfg(windows)]
    #[test]
    fn test_normalize_windows_path() {
        assert_eq!(normalize_windows_path(r"c:\foo\bar"), r"c:\foo\bar");
        assert_eq!(normalize_windows_path(r"c:/foo/bar"), r"c:\foo\bar");
        assert_eq!(normalize_windows_path(r"\??\c:\foo\bar"), r"c:\foo\bar");
        assert_eq!(normalize_windows_path(r"\\?\c:\foo\bar"), r"c:\foo\bar");
        assert_eq!(
            normalize_windows_path(r"\??\UNC\server\foo\bar"),
            r"\\server\foo\bar"
        );
        assert_eq!(
            normalize_windows_path(r"\\?\UNC\server\foo\bar"),
            r"\\server\foo\bar"
        );
    }

    #[test]
    fn test_hash() {
        // A smoke test to make sure we don't accidentally change the hash.
        assert_eq!(hashencode("test"), "9f86d081884c7d65");
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
    fn resolve_computes_layouts_without_creating_anything() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo = fs::canonicalize(temp_dir.path())?.join("repo");
        let scratch_base = temp_dir.path().join("scratch");
        fs::create_dir_all(repo.join(".git"))?;
        fs::create_dir(repo.join("subdir"))?;
        let subdir = Path::new("edenfs/redirections/buck-out");

        let flat = Config {
            template: Some(scratch_base.display().to_string()),
            ..Config::default()
        };
        let scratch_namespace = scratch_root(&flat, &repo, &repo, &zzencode)?;
        fs::create_dir_all(&scratch_namespace)?;

        // A path inside the repository resolves within the repository's namespace.
        let resolved = flat.resolve(Some(&repo.join("subdir")), Some(subdir), false, &zzencode)?;
        assert_eq!(resolved.scratch_root, scratch_namespace);
        assert_eq!(
            resolved.path,
            scratch_namespace.join("edenfsZredirectionsZbuck-out")
        );

        let mirror = Config {
            style: Some(ScratchStyle::Mirror),
            ..flat
        };
        let resolved = mirror.resolve(Some(&repo), Some(subdir), true, &zzencode)?;
        assert_eq!(
            resolved.path,
            scratch_namespace.join("watchable").join(subdir)
        );

        assert_eq!(
            fs::read_dir(&scratch_namespace)?.count(),
            0,
            "resolving must not create directories or a README"
        );
        Ok(())
    }

    #[test]
    fn resolve_repo_root_does_not_need_the_root() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let base = fs::canonicalize(temp_dir.path())?;
        let parent_repo = base.join("parent-repo");
        let checkout = parent_repo.join("checkout");
        let owner_path = base.join("client-state");
        let scratch_base = base.join("scratch");
        fs::create_dir_all(parent_repo.join(".git"))?;
        fs::create_dir(&owner_path)?;
        let owner = get_file_owner(&owner_path)?;
        let config = Config {
            template: Some(scratch_base.join("$REPO_OWNER_USER").display().to_string()),
            ..Config::default()
        };
        let subdir = Path::new("edenfs/redirections");

        let missing =
            config.resolve_repo_root(&checkout, &owner_path, Some(subdir), false, &zzencode)?;
        let namespace = scratch_base.join(&owner).join(zzencode(
            checkout.to_str().expect("checkout path should be UTF-8"),
        ));
        assert_eq!(
            missing.scratch_root, namespace,
            "a missing root should keep its own namespace instead of its parent repository's"
        );
        assert_eq!(missing.path, namespace.join("edenfsZredirections"));
        assert_eq!(missing.repo_owner, owner);
        assert!(!checkout.exists(), "resolving must not create the root");
        assert!(
            !scratch_base.exists(),
            "resolving must not create scratch directories"
        );

        // Once the root exists, normal resolution finds the same location.
        fs::create_dir_all(checkout.join(".git"))?;
        let present = config.resolve(Some(&checkout), Some(subdir), false, &zzencode)?;
        assert_eq!(present.scratch_root, missing.scratch_root);
        assert_eq!(present.path, missing.path);
        assert_eq!(present.repo_owner, missing.repo_owner);
        Ok(())
    }
}
