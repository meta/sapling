/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Path-related utilities.

use std::borrow::Cow;
use std::env;
use std::ffi::OsStr;
#[cfg(unix)]
use std::fs::Permissions;
use std::io;
use std::io::ErrorKind;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use fn_error_context::context;
use fs::remove_file as fs_remove_file;
use fs_err as fs;

use crate::errors::IOContext;
use crate::file::get_umask;

/// Resolve a path to its canonical form, handling non-existent paths.
///
/// If the full path exists, behaves like `std::fs::canonicalize` (resolves
/// all symlinks). If the path does not exist, canonicalizes the closest
/// existing ancestor and appends the remaining (non-existent) components.
///
/// This is useful when you need a canonical absolute path for a target that
/// hasn't been created yet (e.g., a destination directory for a clone). The
/// standard `std::fs::canonicalize` fails on non-existent paths, but callers
/// often still need symlinks in the existing prefix resolved for consistency.
pub fn canonical_path_allow_missing(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = absolute(path)?;
    if let Ok(p) = fs::canonicalize(&path) {
        return Ok(p);
    }
    let mut existing = path.clone();
    let mut tail = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                existing.pop();
            }
            None => return Ok(path),
        }
    }
    let mut result = fs::canonicalize(&existing)?;
    for component in tail.into_iter().rev() {
        result.push(component);
    }
    Ok(result)
}

/// Pick a random file name `path.$RAND.atomic` as `real_path`. Write `data` to
/// it.  Then modify the symlink `path` to point to `real_path`.  Attempt to
/// delete files that are no longer referred.
///
/// Since the symlink itself cannot be mmap-ed on Windows, this function is
/// suitable for large mmap buffer on Windows. Without a symlink the mmap
/// file has to be removed first, otherwise it cannot be replaced.
///
/// Unlike `tempfile::NamedTempFile`, this function does not `chmod` the file.
///
/// This function has a side effect of creating a `path.lock` file for
/// locking.
///
/// Attention: the deletion attempt is based on file name. So do not use
/// confusing file names like `path.0001.atomic` in the same directory.
pub fn atomic_write_symlink(path: &Path, data: &[u8]) -> io::Result<()> {
    atomic_write_symlink_inner(path, data).path_context("atomic writing symlink", path)
}

fn atomic_write_symlink_inner(path: &Path, data: &[u8]) -> io::Result<()> {
    let append_name = |suffix: &str| -> PathBuf {
        let mut s = path.to_path_buf().into_os_string();
        s.push(suffix);
        s.into()
    };
    let temp_name = || -> PathBuf { append_name(&format!(".{:x}.atomic", rand::random::<u32>())) };

    // Protect racy write operations by a lock.
    let _lock = crate::lock::PathLock::exclusive(append_name(".lock"))?;

    // Pick a name. Open the file.
    let (real_path, mut file) = loop {
        let real_path = temp_name();
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&real_path)
        {
            Ok(file) => break Ok((real_path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue, // try another file name
            Err(e) => {
                break Err(e);
            }
        }
    }?;
    let real_file_name = real_path
        .file_name()
        .expect("real_path should have a file name");

    // Write the content.
    file.write_all(data)?;
    drop(file);

    // Update the symlink by creating a temporary symlink and rename it.
    let symlink_tmp_path = loop {
        let symlink_path = temp_name();
        match symlink_file(Path::new(real_file_name), &symlink_path) {
            Ok(()) => break Ok(symlink_path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue, // try another file name
            Err(e) => {
                break Err(e);
            }
        }
    }?;

    // Overwrite the original symlink. This works on both Windows and Linux.
    fs::rename(symlink_tmp_path, path)?;

    // Scan. Remove unreferenced files.
    let _ = (|| -> io::Result<()> {
        let looks_like_atomic = |s: &OsStr, prefix: &OsStr| -> bool {
            if let (Some(s), Some(prefix)) = (s.to_str(), prefix.to_str()) {
                s.starts_with(prefix) && s.ends_with(".atomic")
            } else {
                false
            }
        };
        if let (Some(dir), Some(prefix)) = (path.parent(), path.file_name()) {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let name = entry.file_name();
                if name != prefix && looks_like_atomic(&name, prefix) && name != real_file_name {
                    let _ = remove_file(entry.path());
                }
            }
        }
        Ok(())
    })();

    Ok(())
}

/// Create symlink for a file.
pub fn symlink_file(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(windows)]
    return std::os::windows::fs::symlink_file(src, dst);
    #[cfg(unix)]
    return std::os::unix::fs::symlink(src, dst);
    #[cfg(all(not(unix), not(windows)))]
    return Err(io::Error::new(
        ErrorKind::Other,
        "symlink is not supported by the system",
    ));
}

/// Create symlink for a dir.
pub fn symlink_dir(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(windows)]
    return std::os::windows::fs::symlink_dir(src, dst);
    #[cfg(not(windows))]
    symlink_file(src, dst)
}

/// Removes the extended-length path prefix `\\?\` on Windows.
/// Returns the path unchanged on non-Windows or if no prefix is present.
pub fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    path.to_string_lossy()
        .strip_prefix(r"\\?\")
        .map(PathBuf::from)
        .unwrap_or(path)
}

/// Return the absolute and normalized path without accessing the filesystem.
///
/// Unlike [`fs::canonicalize`], do not follow symlinks.
///
/// This function does not access the filesystem. Therefore it can behave
/// differently from the kernel or other library functions in corner cases.
/// For example:
///
/// - On some systems with symlink support, `foo/bar/..` and `foo` can be
///   different as seen by the kernel, if `foo/bar` is a symlink. This
///   function always returns `foo` in this case.
/// - On Windows, the official normalization rules are much more complicated.
///   See https://github.com/rust-lang/rust/pull/47363#issuecomment-357069527.
///   For example, this function cannot translate "drive relative" path like
///   "X:foo" to an absolute path.
///
/// Return an error if `std::env::current_dir()` fails or if this function
/// fails to produce an absolute path.
pub fn absolute(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref();
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    if !path.is_absolute() {
        return Err(io::Error::other(format!(
            "cannot get absolute path from {path:?}"
        )));
    }

    Ok(normalize(&path))
}

/// Canonicalize a path, falling back to lexical normalization when the path still
/// exists but cannot be canonicalized.
///
/// The fallback returns an absolute path but does not resolve symlinks or junctions.
pub fn canonicalize_best_effort(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref();
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(err) => {
            let full_path = absolute(path)?;
            match full_path.try_exists() {
                Ok(true) => Ok(full_path),
                _ => Err(err),
            }
        }
    }
}

/// Normalize path to collapse "..", ".", and duplicate separators. This
/// function does not access the filesystem, so it can return an
/// incorrect result if the path contains symlinks.
///
///     # use std::path::Path;
///     # use util::path::normalize;
///     assert_eq!(normalize("foo/.//bar/../baz/".as_ref()), Path::new("foo/baz"));
///
///     // Interesting edge cases:
///     assert_eq!(normalize("".as_ref()), Path::new("."));
///     assert_eq!(normalize("..".as_ref()), Path::new(".."));
///     assert_eq!(normalize("/..".as_ref()), Path::new("/"));
///
/// This behavior matches that of Python's `os.path.normpath` and Go's `path.Clean`.
pub fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    let mut poppable: usize = 0;
    let mut has_root = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => {
                poppable += 1;
                result.push(component);
            }
            Component::RootDir => {
                has_root = true;
                result.push(component);
            }
            Component::Prefix(_) => {
                result.push(component);
            }
            Component::ParentDir => {
                if poppable > 0 {
                    result.pop();
                    poppable -= 1;
                } else if !has_root {
                    result.push(component);
                }
            }
            Component::CurDir => {}
        }
    }

    if result.as_os_str().is_empty() {
        return ".".into();
    }

    result
}

/// Given cwd, return `path` relative to `root`, or None if `path` is not under `root`.
/// This is analogous to pathutil.canonpath() in Python.
pub fn root_relative_path(root: &Path, cwd: &Path, path: &Path) -> io::Result<Option<PathBuf>> {
    // Make `path` absolute. I'm not sure why `root` is included.
    // Maybe in case `cwd` is empty? Or to allow root-relative `cwd`?
    let path = normalize(&root.join(cwd).join(path));

    // Handle easy case when `path` lexically starts w/ `root`.
    if let Ok(suffix) = path.strip_prefix(root) {
        return Ok(Some(suffix.to_path_buf()));
    }

    // Resolve symlinks in `root` so we can do lexical `strip_prefix` below.
    let root = root.canonicalize().path_context("canonicalizing", root)?;

    // Test parents of `path` looking for symlinks that point under `root`.
    let mut test = PathBuf::new();
    let mut path_parts = path.components();
    while let Some(part) = path_parts.next() {
        test.push(part);
        if test.is_symlink() {
            // TODO: this makes our loop O(n^2)
            test = test.canonicalize().path_context("canonicalizing", &test)?;
        }
        if let Ok(suffix) = test.strip_prefix(&root) {
            return Ok(Some(suffix.join(path_parts)));
        }
    }

    Ok(None)
}

/// Remove the file pointed by `path`.
pub fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = path.as_ref();
    // On Windows, try to rename the file before removing.
    // This allows re-creating a same file.
    // See https://boostgsoc13.github.io/boost.afio/doc/html/afio/FAQ/deleting_open_files.html
    let path: Cow<Path> = if cfg!(windows) {
        let tmp_path = path.with_extension(format!("tmp.{:x}", rand::random::<u16>()));
        fs::rename(path, &tmp_path)?;
        Cow::Owned(tmp_path)
    } else {
        Cow::Borrowed(path)
    };
    // This borrow is not needless when `#[cfg(windows)]`
    #[allow(clippy::needless_borrows_for_generic_args)]
    let result = fs_remove_file(&path);
    #[cfg(windows)]
    match &result {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            // The file might be a directory symlink
            if let Ok(r) = remove_directory_symlink(&path) {
                if r {
                    return Ok(());
                }
            }
            // This file might be mmapp-ed. Try a different way.
            if windows_remove_mmap_file(&path).is_ok() {
                return Ok(());
            }
        }
        _ => {}
    }
    result
}

#[cfg(windows)]
/// Tries to remove a symlink in case it was a directory symlink
fn remove_directory_symlink(path: &Path) -> io::Result<bool> {
    let metadata = path.symlink_metadata()?;
    if metadata.is_symlink() {
        fs::remove_dir(path)?;
        return Ok(true);
    }
    Ok(false)
}

/// Deletes a file even if it is being mmap-ed on Windows.
/// See https://stackoverflow.com/questions/54138684.
#[cfg(windows)]
fn windows_remove_mmap_file(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use winapi::shared::minwindef::FALSE;
    use winapi::um::fileapi::CreateFileW;
    use winapi::um::fileapi::OPEN_EXISTING;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::winbase::FILE_FLAG_DELETE_ON_CLOSE;
    use winapi::um::winnt::DELETE;
    use winapi::um::winnt::FILE_SHARE_DELETE;
    use winapi::um::winnt::FILE_SHARE_READ;
    use winapi::um::winnt::FILE_SHARE_WRITE;

    let wpath: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wpath.as_ptr(),
            DELETE,
            FILE_SHARE_DELETE | FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_DELETE_ON_CLOSE,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(err)
        }
    } else if unsafe { CloseHandle(handle) } == FALSE {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn add_stat_context<T, E: Into<anyhow::Error>>(
    res: anyhow::Result<T, E>,
    path: Option<&Path>,
) -> anyhow::Result<T, anyhow::Error> {
    use std::os::unix::fs::MetadataExt;

    use anyhow::Context;

    let res = res.map_err(Into::into);

    if let Some(path) = path {
        if let Ok(md) = path.metadata() {
            return res.context(format!(
                "stat({:?}) = dev:{} ino:{} mode:0o{:o} uid:{} gid:{} mtime:{}",
                path,
                md.dev(),
                md.ino(),
                md.mode(),
                md.uid(),
                md.gid(),
                md.mtime()
            ));
        }
    }

    res
}

/// Resolve leaf symlinks until we get to a non-symlink or non-existent, returning Ok(dest).
/// Propagates unexpected errors like permission errors.
#[cfg(unix)]
fn resolve_symlinks(path: &Path) -> anyhow::Result<PathBuf> {
    use std::collections::HashSet;

    use anyhow::Context;
    use anyhow::bail;
    fn inner(path: PathBuf, seen: &mut HashSet<PathBuf>) -> anyhow::Result<PathBuf> {
        if seen.contains(&path) {
            bail!("symlink cycle containing {path:?}");
        }

        seen.insert(path.clone());

        match path.read_link() {
            Ok(target) => inner(target, seen),

            // Not a symlink.
            Err(err) if err.kind() == io::ErrorKind::InvalidInput => Ok(path),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(path),

            // Unexpected error reading file.
            Err(err) => add_stat_context(
                Err(err).context(format!("statting {path:?}")),
                path.parent(),
            ),
        }
    }

    let mut seen = HashSet::new();
    let mut res = inner(path.to_path_buf(), &mut seen);
    if seen.len() > 1 {
        res = res.with_context(|| format!("traversing symlinks from {path:?}"));
    }
    res
}

/// Create the directory with specified permission on UNIX systems. Return
/// Ok(()) if directory already exists. We create a temporary directory at the
/// parent directory of the the directory being created, run chmod to change the
/// permission then rename the temporary directory to the desired name to
/// prevent leaking directory with incorrect permissions.
#[cfg(unix)]
#[context("creating dir {:?} with mode 0o{:o}", path, mode)]
fn create_dir_with_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    use anyhow::Context;
    use anyhow::anyhow;

    let path = resolve_symlinks(path)?;

    match path.metadata() {
        Ok(md) if md.is_file() => {
            return Err(anyhow!(io::Error::from(ErrorKind::AlreadyExists)))
                .context(format!("path exists as a file: {path:?}"));
        }
        Ok(md) => {
            // Symlinks were resolved above - assume is_dir.
            if md.permissions().mode() & mode != mode {
                // Best effort to fix permissions.
                let _ = fs::set_permissions(path, Permissions::from_mode(mode));
            }
            return Ok(());
        }
        // Fall through and try creating it.
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return add_stat_context(
                Err(err).context(format!("error statting {path:?}")),
                path.parent(),
            );
        }
    };

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            ErrorKind::NotFound,
            format!("`{path:?}` does not have a parent directory"),
        )
    })?;

    let parent = resolve_symlinks(parent)?;

    let temp = add_stat_context(tempfile::TempDir::new_in(&parent), Some(&parent))
        .with_context(|| format!("creating temp dir in {parent:?}"))?;

    fs::set_permissions(&temp, Permissions::from_mode(mode))
        .with_context(|| format!("setting permissions on temp dir {temp:?}"))?;

    let temp = temp.keep();
    if let Err(e) = fs::rename(&temp, &path) {
        // In the unlikely event where the rename fails, we attempt to clean up the
        // previously leaked temporary file before returning.
        let _ = fs::remove_dir(&temp);

        // The rename may fail if the desinated directory already exists and is not empty. In this
        // case it will return `ENOTEMPTY` instead of `EEXIST`.
        // Similarly, when the destinated directory is a file, we get `ENOTDIR` instead of `EEXIST`.
        match e.kind() {
            ErrorKind::DirectoryNotEmpty => {
                // Target directory exists now - we probably raced with someone else to create it.

                // Best effort to fix permissions.
                let _ = fs::set_permissions(path, Permissions::from_mode(mode));

                Ok(())
            }
            ErrorKind::NotADirectory => Err(io::Error::from(ErrorKind::AlreadyExists).into()),
            _ => Err(e.into()),
        }
    } else {
        Ok(())
    }
}

/// Create the directory. Return Ok(()) if directory already exists.
/// The mode argument is ignored on non-UNIX systems.
#[cfg(not(unix))]
#[context("creating dir {:?}", path)]
fn create_dir_with_mode(path: &Path, _mode: u32) -> anyhow::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub fn dir_mode(umask: u32) -> u32 {
    // umask affects the "group" and "other" permission.
    0o777 ^ (0o077 & umask)
}

fn shared_dir_mode(umask: u32) -> u32 {
    // umask affects the "other" permission.
    0o2777 ^ (0o0007 & umask)
}

fn is_io_error_kind(err: &anyhow::Error, kind: ErrorKind) -> bool {
    err.downcast_ref::<io::Error>()
        .is_some_and(|err| err.kind() == kind)
}

/// Create the directory and ignore failures when a directory of the same name already exists.
pub fn create_dir(path: impl AsRef<Path>) -> anyhow::Result<()> {
    create_dir_with_mode(path.as_ref(), dir_mode(get_umask()))
}

/// Create the directory with group write permission on UNIX systems.
pub fn create_shared_dir(path: impl AsRef<Path>) -> anyhow::Result<()> {
    create_dir_with_mode(path.as_ref(), shared_dir_mode(get_umask()))
}

/// Create the directory and its ancestors. The mode argument is ignored on non-UNIX systems.
pub fn create_dir_all_with_mode(path: impl AsRef<Path>, mode: u32) -> anyhow::Result<()> {
    let mut to_create = vec![path.as_ref()];
    while let Some(dir) = to_create.pop() {
        match create_dir_with_mode(dir, mode) {
            Ok(()) => continue,
            Err(err) if is_io_error_kind(&err, io::ErrorKind::NotFound) => {
                to_create.push(dir);
                match dir.parent() {
                    Some(parent) => to_create.push(parent),
                    None => return Err(err),
                }
            }
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Create the directory and ancestors with group write permission on UNIX systems.
pub fn create_shared_dir_all(path: impl AsRef<Path>) -> anyhow::Result<()> {
    create_dir_all_with_mode(path, shared_dir_mode(get_umask()))
}

/// Expand the user's home directory and any environment variables references in
/// the given path.
///
/// This function is designed to emulate the behavior of Mercurial's `util.expandpath`
/// function, which in turn uses Python's `os.path.expand{user,vars}` functions. This
/// results in behavior that is notably different from the default expansion behavior
/// of the `shellexpand` crate. In particular:
///
/// - If a reference to an environment variable is missing or invalid, the reference
///   is left unchanged in the resulting path rather than emitting an error.
///
/// - Home directory expansion explicitly happens after environment variable
///   expansion, meaning that if an environment variable is expanded into a
///   string starting with a tilde (`~`), the tilde will be expanded into the
///   user's home directory.
pub fn expand_path(path: impl AsRef<str>) -> PathBuf {
    expand_path_impl(path.as_ref(), |k| env::var(k).ok(), dirs::home_dir)
}

/// Same as `expand_path` but explicitly takes closures for environment variable
/// and home directory lookup for the sake of testability.
fn expand_path_impl<E, H>(path: &str, getenv: E, homedir: H) -> PathBuf
where
    E: FnMut(&str) -> Option<String>,
    H: FnOnce() -> Option<PathBuf>,
{
    // The shellexpand crate does not expand Windows environment variables
    // like `%PROGRAMDATA%`. We'd like to expand them too. So let's do some
    // pre-processing.
    //
    // XXX: Doing this preprocessing has the unfortunate side-effect that
    // if an environment variable fails to expand on Windows, the resulting
    // string will contain a UNIX-style environment variable reference.
    //
    // e.g., "/foo/%MISSING%/bar" will expand to "/foo/${MISSING}/bar"
    //
    // The current approach is good enough for now, but likely needs to
    // be improved later for correctness.
    let path = {
        let mut new_path = String::new();
        let mut is_starting = true;
        for ch in path.chars() {
            if ch == '%' {
                if is_starting {
                    new_path.push_str("${");
                } else {
                    new_path.push('}');
                }
                is_starting = !is_starting;
            } else if cfg!(windows) && ch == '/' {
                // Only on Windows, change "/" to "\" automatically.
                // This makes sure "%include /foo" works as expected.
                new_path.push('\\')
            } else {
                new_path.push(ch);
            }
        }
        new_path
    };

    let path = shellexpand::env_with_context_no_errors(&path, getenv);
    shellexpand::path::tilde_with_context(&*path, homedir)
        .as_ref()
        .into()
}

/// Return a relative [`PathBuf`] to the path from the base path.
pub fn relativize(base: &Path, path: &Path) -> PathBuf {
    let mut base_iter = base.iter();
    let mut path_iter = path.iter();
    let mut rel_path = PathBuf::new();
    loop {
        match (base_iter.next(), path_iter.next()) {
            (Some(ref c), Some(ref p)) if c == p => continue,

            // Examples:
            // b: foo/bar/baz/
            // p: foo/bar/biz/buzz.html
            // This is the common case where we have to go up some number of directories
            // (so one "../" per unique path component of base) and then back down.
            //
            // b: foo/bar/baz/biz/
            // p: foo/bar/
            // If foo/bar was a file and then the user replaced it with a directory, and now
            // the user is in a subdirectory of that directory, then one "../" per unique path
            // component of base.
            (Some(_c), remaining_path) => {
                // Find the common prefix of path and base. Prefix with one "../" per unique
                // path component of base and then append the unique sequence of components from
                // path.
                rel_path.push(".."); // This is for the current component, c.
                for _ in base_iter {
                    rel_path.push("..");
                }

                if let Some(p) = remaining_path {
                    rel_path.push(p);
                    for component in path_iter {
                        rel_path.push(component);
                    }
                }
                break;
            }

            // Example:
            // b: foo/bar/
            // p: foo/bar/baz/buzz.html
            (None, Some(p)) => {
                rel_path.push(p);
                for component in path_iter {
                    rel_path.push(component);
                }
                break;
            }

            // Example:
            // b: foo/bar/baz/
            // p: foo/bar/baz/
            // If foo/bar/baz was a file and then the user replaced it with a directory, which
            // is also the user's current directory, then "" should be returned.
            (None, None) => {
                break;
            }
        }
    }

    rel_path
}

/// Replace forward slashes (/) with backward slashes (\) on a wide coded-path
#[cfg(windows)]
pub fn replace_slash_with_backslash(path: &Path) -> PathBuf {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::ffi::OsStringExt;

    use widestring::U16Str;

    // Convert OsString to Vec<u16> (UTF-16 representation on Windows)
    let mut utf16_string: Vec<u16> = path.as_os_str().encode_wide().collect();

    let to_replace = U16Str::from_slice(&mut utf16_string)
        .char_indices()
        .filter_map(|(i, c)| {
            c.ok()
                .map(|c| if c == '/' { Some(i) } else { None })
                .flatten()
        })
        .collect::<Vec<_>>();

    for i in to_replace {
        utf16_string[i] = '\\' as u16;
    }

    PathBuf::from(std::ffi::OsString::from_wide(&utf16_string))
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::fs::create_dir_all;
    use std::fs::remove_dir_all;

    use anyhow::Result;
    use tempfile::TempDir;

    use super::*;

    #[cfg(windows)]
    mod windows {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::ffi::OsStringExt;

        use super::*;

        #[test]
        fn test_absolute_fullpath() {
            assert_eq!(absolute("C:/foo").unwrap(), Path::new("C:\\foo"));
            assert_eq!(
                absolute("x:\\a/b\\./.\\c").unwrap(),
                Path::new("x:\\a\\b\\c")
            );
            assert_eq!(
                absolute("y:/a/b\\../..\\c\\../d\\./.").unwrap(),
                Path::new("y:\\d")
            );
            assert_eq!(
                absolute("z:/a/b\\../..\\../..\\..").unwrap(),
                Path::new("z:\\")
            );
        }

        #[test]
        fn test_normalize_path() {
            assert_eq!(normalize(r"a/b\c\..\.".as_ref()), Path::new(r"a\b"));
            assert_eq!(normalize("z:/a//b/./".as_ref()), Path::new(r"z:\a\b"));
        }

        #[test]
        fn test_replace_slash_with_backslash() {
            // Test replacing a normal string
            let utf16_bytes: &[u16] = &[0xd83c, 0xdf31, 0x002f, 0x0073, 0x0061, 0x0070];
            let path = PathBuf::from(std::ffi::OsString::from_wide(utf16_bytes));
            let expected = "🌱\\sap".encode_utf16().collect::<Vec<_>>();
            assert_eq!(
                replace_slash_with_backslash(&path)
                    .as_os_str()
                    .encode_wide()
                    .collect::<Vec<_>>(),
                expected,
            );

            // Test replacing string with the / character encoded on it
            // This string is the same as "expected" above, but with one character corrupted
            let utf16_bytes: &[u16] = &[0xd83c, 0x002f, 0x002f, 0x0073, 0x0061, 0x0070];
            // 0x005c is the UTF-16 character for backslash
            let expected: Vec<u16> = Vec::from([0xd83c, 0x005c, 0x005c, 0x0073, 0x0061, 0x0070]);
            let path = PathBuf::from(std::ffi::OsString::from_wide(utf16_bytes));
            assert_eq!(
                replace_slash_with_backslash(&path)
                    .as_os_str()
                    .encode_wide()
                    .collect::<Vec<_>>(),
                expected,
            );

            // Another case of / being on unexpected places
            let utf16_bytes: &[u16] = &[0x002f, 0xdf31, 0x002f, 0x0073, 0x0061, 0x0070];
            let expected: Vec<u16> = Vec::from([0x005c, 0xdf31, 0x005c, 0x0073, 0x0061, 0x0070]);
            let path = PathBuf::from(std::ffi::OsString::from_wide(utf16_bytes));
            assert_eq!(
                replace_slash_with_backslash(&path)
                    .as_os_str()
                    .encode_wide()
                    .collect::<Vec<_>>(),
                expected,
            );
        }
    }

    #[cfg(unix)]
    mod unix {
        use super::*;

        #[test]
        fn test_absolute_fullpath() {
            assert_eq!(absolute("/a/./b\\c/../d/.").unwrap(), Path::new("/a/d"));
            assert_eq!(absolute("/a/../../../../b").unwrap(), Path::new("/b"));
            assert_eq!(absolute("/../../..").unwrap(), Path::new("/"));
            assert_eq!(absolute("/../../../").unwrap(), Path::new("/"));
            assert_eq!(
                absolute("//foo///bar//baz").unwrap(),
                Path::new("/foo/bar/baz")
            );
            assert_eq!(absolute("//").unwrap(), Path::new("/"));
        }

        #[test]
        fn test_normalize_path() {
            assert_eq!(normalize("/a/./b/../d/.".as_ref()), Path::new("/a/d"));
            assert_eq!(normalize("./a/b/c/../../".as_ref()), Path::new("a"));
            assert_eq!(normalize("".as_ref()), Path::new("."));
            assert_eq!(normalize(".".as_ref()), Path::new("."));
            assert_eq!(normalize("..".as_ref()), Path::new(".."));
            assert_eq!(normalize("/..".as_ref()), Path::new("/"));
            assert_eq!(normalize("/../..".as_ref()), Path::new("/"));
            assert_eq!(normalize("./..".as_ref()), Path::new(".."));
            assert_eq!(normalize("../../..".as_ref()), Path::new("../../.."));
            assert_eq!(normalize("////".as_ref()), Path::new("/"));
        }

        #[test]
        fn test_create_dir_mode() -> Result<()> {
            let tempdir = TempDir::new()?;
            let mut path = tempdir.path().to_path_buf();
            path.push("dir");
            create_dir(&path)?;
            assert!(path.is_dir());
            let metadata = path.metadata()?;
            assert_eq!(metadata.permissions().mode(), 0o40755);
            // check we don't have temporary directory left
            assert_eq!(tempdir.path().read_dir()?.count(), 1);
            Ok(())
        }

        #[cfg(not(target_os = "macos"))]
        #[test]
        fn test_create_shared_dir() -> Result<()> {
            let tempdir = TempDir::new()?;
            let mut path = tempdir.path().to_path_buf();
            path.push("shared");
            create_shared_dir(&path)?;
            assert!(path.is_dir());
            let metadata = path.metadata()?;
            assert_eq!(metadata.permissions().mode(), 0o42775);
            // check we don't have temporary directory left
            assert_eq!(tempdir.path().read_dir()?.count(), 1);
            Ok(())
        }

        #[cfg(not(target_os = "macos"))]
        #[test]
        fn test_fixup_perms() -> Result<()> {
            let tempdir = TempDir::new()?;
            let mut path = tempdir.path().to_path_buf();
            path.push("shared");

            // Create it without the SGID bit.
            create_dir_with_mode(&path, 0o775)?;
            let metadata = path.metadata()?;
            assert_eq!(metadata.permissions().mode(), 0o40775);

            // Fix it up.
            create_shared_dir(&path)?;
            let metadata = path.metadata()?;
            assert_eq!(metadata.permissions().mode(), 0o42775);

            Ok(())
        }

        #[test]
        fn test_create_dir_no_perms() -> Result<()> {
            let tempdir = TempDir::new()?;
            let mut path = tempdir.path().to_path_buf();
            path.push("nope");

            std::fs::create_dir(&path)?;
            std::fs::set_permissions(&path, Permissions::from_mode(0o0))?;

            let err = create_dir_with_mode(&path.join("dir"), 0o775).unwrap_err();
            assert!(is_io_error_kind(&err, io::ErrorKind::PermissionDenied));

            // Make sure we give parent dir's info in error.
            assert!(format!("{err:?}").contains(&format!("stat({path:?}) = ")));

            Ok(())
        }
    }

    fn test_create_dir_all_fn(
        create_fn: &dyn Fn(&PathBuf) -> anyhow::Result<()>,
        mode: u32,
    ) -> Result<()> {
        let tempdir = TempDir::new()?;
        let path = tempdir.path().join("foo").join("bar");
        create_fn(&path)?;
        assert!(path.is_dir());

        #[cfg(unix)]
        {
            let metadata = path.metadata()?;
            assert_eq!(metadata.permissions().mode(), mode);
        }

        #[cfg(windows)]
        let _ = mode;

        // Sanity that there is no error if directory already exists.
        create_fn(&path)?;

        #[cfg(unix)]
        {
            let broken_symlink = tempdir.path().join("foo").join("bar").join("oops");
            symlink_file(&tempdir.path().join("doesnt_exist"), &broken_symlink)?;

            // Resolve symlinks first, allowing us to create dirs across broken symlinks.
            assert!(create_fn(&broken_symlink.join("nope")).is_ok());
            assert!(tempdir.path().join("doesnt_exist").join("nope").is_dir());
        }

        // Sanity that we get errors if there is a regular file in the way.
        let regular_file = tempdir.path().join("regular_file");
        File::create(&regular_file)?;
        assert!(create_fn(&regular_file).is_err());
        assert!(create_fn(&regular_file.join("no_can_do")).is_err());

        Ok(())
    }

    #[test]
    #[cfg_attr(windows, ignore)] // FIXME: see D45267362, this needs a different test on Windows on 1.69+
    fn test_atomic_write_symlink() -> Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let j = |name| -> PathBuf { path.join(name) };
        // a: unrelated file
        fs::write(j("a"), b"1")?;
        // c: symlink -> c.xxxx: "2"
        atomic_write_symlink(&j("c"), b"2")?;
        // Keep an mmap of the current "c".
        let file = fs::OpenOptions::new().read(true).open(j("c"))?;
        let mmap = unsafe { memmap2::Mmap::map(&file) }?;
        // This file should be automatically deleted.
        fs::write(j("c.aaaa.atomic"), b"0")?;
        // Rewrite c with different data.
        atomic_write_symlink(&j("c"), b"3")?;
        // mmap has the old content.
        assert_eq!(mmap.as_ref(), b"2");
        // Reading c gets new data.
        assert_eq!(fs::read(j("c"))?, b"3");
        // a: should exist
        assert!(j("a").exists());
        // 4 files: a, c, c.xxxx, c.lock
        let count = || {
            fs::read_dir(path)
                .unwrap()
                .filter(|e| e.as_ref().unwrap().path().exists())
                .count()
        };
        assert_eq!(count(), 4);

        // It's possible to replace a non-symlink to a symlink.
        // (but this might fail if "a" is mmap-ed on Windows).
        atomic_write_symlink(&j("a"), b"4")?;
        // Exercise the GC logic a bit.
        atomic_write_symlink(&j("a"), b"5")?;
        atomic_write_symlink(&j("a"), b"6")?;
        // 6 files: a, a.xxxx, a.lock, c, c.xxxx, c.lock
        assert_eq!(count(), 6);
        Ok(())
    }

    #[test]
    fn test_mmap_delete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foo");
        fs::write(&path, b"bar").unwrap();
        let file = fs::OpenOptions::new().read(true).open(&path).unwrap();
        let _mmap = unsafe { memmap2::Mmap::map(&file) }.unwrap();
        remove_file(&path).unwrap();
        assert!(!path.exists());
        // The file can be recreated in-place.
        fs::write(&path, b"baz").unwrap();
    }

    #[test]
    fn test_create_dir_non_exist() -> Result<()> {
        let tempdir = TempDir::new()?;
        let mut path = tempdir.path().to_path_buf();
        path.push("dir");
        create_dir(&path)?;
        assert!(path.is_dir());
        Ok(())
    }

    #[test]
    fn test_create_dir_exist() -> Result<()> {
        let tempdir = TempDir::new()?;
        let mut path = tempdir.path().to_path_buf();
        path.push("dir");
        create_dir(&path)?;
        assert!(&path.is_dir());
        create_dir(&path)?;
        assert!(&path.is_dir());
        Ok(())
    }

    #[test]
    fn test_create_dir_file_exist() -> Result<()> {
        let tempdir = TempDir::new()?;
        let mut path = tempdir.path().to_path_buf();
        path.push("dir");
        File::create(&path)?;

        let err = create_dir(&path).unwrap_err();
        assert!(is_io_error_kind(&err, io::ErrorKind::AlreadyExists));

        Ok(())
    }

    #[test]
    fn test_create_dir_with_nonexistent_parent() -> Result<()> {
        let tempdir = TempDir::new()?;
        let mut path = tempdir.path().to_path_buf();
        path.push("nonexistentparent");
        path.push("dir");
        let err = create_dir(&path).unwrap_err();
        assert!(is_io_error_kind(&err, ErrorKind::NotFound));
        Ok(())
    }

    #[test]
    fn test_create_dir_without_empty_path() {
        let empty = Path::new("");
        let err = create_dir(empty).unwrap_err();
        assert!(is_io_error_kind(&err, ErrorKind::NotFound));
    }

    #[test]
    fn test_path_expansion() {
        fn getenv(key: &str) -> Option<String> {
            match key {
                "foo" => Some("~/a".into()),
                "bar" => Some("b".into()),
                _ => None,
            }
        }

        fn homedir() -> Option<PathBuf> {
            Some(PathBuf::from("/home/user"))
        }

        let path = "$foo/${bar}/$baz";
        let expected = PathBuf::from("/home/user/a/b/$baz");

        assert_eq!(expand_path_impl(path, getenv, homedir), expected);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn test_create_shared_dir_all() -> Result<()> {
        test_create_dir_all_fn(&|path| create_shared_dir_all(path), 0o42775)
    }

    #[test]
    fn test_create_dir_all_with_mode() -> Result<()> {
        test_create_dir_all_fn(&|path| create_dir_all_with_mode(path, 0o777), 0o40777)
    }

    #[test]
    fn test_relativize_absolute_paths() {
        let check = |base, path, expected| {
            assert_eq!(
                relativize(Path::new(base), Path::new(path)),
                Path::new(expected)
            );
        };
        check("/", "/", "");
        check("/foo/bar/baz", "/foo/bar/baz", "");
        check("/foo/bar", "/foo/bar/baz", "baz");
        check("/foo", "/foo/bar/baz", "bar/baz");
        check("/foo/bar/baz", "/foo/bar", "..");
        check("/foo/bar/baz", "/foo", "../..");
        check("/foo/bar/baz", "/foo/BAR", "../../BAR");
        check("/foo/bar/baz", "/foo/BAR/BAZ", "../../BAR/BAZ");
    }

    #[test]
    fn test_relativize_platform_absolute_paths() {
        // This test with Windows-style absolute paths on Windows, and Unix-style path on Unix
        let cwd = Path::new(".").canonicalize().unwrap();
        let result = relativize(&cwd, &cwd.join("a").join("b"));
        assert_eq!(result, Path::new("a").join("b"));
    }

    #[test]
    fn test_root_relative_path() -> Result<()> {
        let tempdir = TempDir::new()?;

        let parent = tempdir.path().join("parent");
        let root = parent.join("root");
        let child = root.join("child");
        create_dir_all(&child)?;

        assert_eq!(
            root_relative_path(&root, &root, ".".as_ref())?,
            Some(PathBuf::from(""))
        );
        assert_eq!(
            root_relative_path(&root, &child, ".".as_ref())?,
            Some(PathBuf::from("child"))
        );
        assert_eq!(
            root_relative_path(&root, &child, "foo".as_ref())?,
            Some(["child", "foo"].iter().collect::<PathBuf>()),
        );

        let symlink_to_root = parent.join("symlink_to_root");
        symlink_dir(&root, &symlink_to_root)?;
        assert_eq!(
            root_relative_path(&root, &root, &symlink_to_root.join("child"))?,
            Some(PathBuf::from("child")),
        );

        let symlink_to_child = parent.join("symlink_to_child");
        symlink_dir(&child, &symlink_to_child)?;
        assert_eq!(
            root_relative_path(&root, &root, &symlink_to_child.join("foo"))?,
            Some(["child", "foo"].iter().collect::<PathBuf>()),
        );

        assert!(root_relative_path(&root, &parent, "foo".as_ref())?.is_none());

        // Sanity that we don't treat "root" as prefix of "rootbeer".
        assert!(root_relative_path(&root, &root, &parent.join("rootbeer"))?.is_none());

        Ok(())
    }

    #[test]
    fn test_dir_mode() {
        assert_eq!(dir_mode(0o022), 0o755);
        assert_eq!(shared_dir_mode(0o022), 0o2775);

        assert_eq!(dir_mode(0), 0o777);
        assert_eq!(shared_dir_mode(0), 0o2777);

        assert_eq!(dir_mode(0o777), 0o700);
        assert_eq!(shared_dir_mode(0o777), 0o2770);
    }

    #[test]
    // Windows doesn't suffer from the issue being tested, but has a different, less
    // important issue.
    #[cfg_attr(windows, ignore)]
    fn test_create_dir_with_mode_race() {
        let tempdir = TempDir::new().unwrap();

        let dir = tempdir.path().join("dir");

        // Make sure we don't have race conditions between threads creating the same dir.
        std::thread::scope(|s| {
            for _ in 0..10 {
                s.spawn(|| {
                    for _ in 0..100 {
                        create_dir_with_mode(&dir, 0o755).unwrap();
                        let _ = std::fs::write(dir.join("file"), "contents");
                        let _ = remove_dir_all(&dir);
                    }
                });
            }
        });
    }

    // --- strip_unc_prefix tests ---

    #[test]
    fn test_strip_unc_prefix_with_prefix() {
        let path = PathBuf::from(r"\\?\C:\open\test1");
        assert_eq!(strip_unc_prefix(path), PathBuf::from(r"C:\open\test1"));
    }

    #[test]
    fn test_strip_unc_prefix_without_prefix() {
        let path = PathBuf::from(r"C:\open\test1");
        assert_eq!(strip_unc_prefix(path), PathBuf::from(r"C:\open\test1"));
    }

    #[test]
    fn test_strip_unc_prefix_unix_path() {
        let path = PathBuf::from("/home/user/repo");
        assert_eq!(strip_unc_prefix(path), PathBuf::from("/home/user/repo"));
    }

    #[test]
    fn test_strip_unc_prefix_unc_network_path() {
        // UNC network paths (\\server\share) should NOT be stripped —
        // only the extended-length prefix \\?\ should be removed.
        let path = PathBuf::from(r"\\server\share\dir");
        assert_eq!(strip_unc_prefix(path), PathBuf::from(r"\\server\share\dir"));
    }

    #[test]
    fn test_canonicalize_best_effort_existing_path() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            canonicalize_best_effort(dir.path()).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn test_canonicalize_best_effort_requires_existing_path() {
        let missing = tempfile::tempdir().unwrap().path().join("missing");
        let expected = std::fs::canonicalize(&missing).unwrap_err();
        let actual = canonicalize_best_effort(&missing).unwrap_err();

        assert_eq!(actual.kind(), expected.kind());
        assert_eq!(actual.raw_os_error(), expected.raw_os_error());
    }

    // --- canonical_path_allow_missing tests ---

    #[test]
    fn test_canonical_path_allow_missing_existing() {
        let dir = tempfile::tempdir().unwrap();
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        let result = canonical_path_allow_missing(dir.path()).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_canonical_path_allow_missing_nonexistent_tail() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_dir = std::fs::canonicalize(dir.path()).unwrap();
        let non_existent = dir.path().join("does_not_exist");
        let result = canonical_path_allow_missing(&non_existent).unwrap();
        assert_eq!(result, canonical_dir.join("does_not_exist"));
    }

    #[cfg(unix)]
    #[test]
    fn test_canonical_path_allow_missing_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();

        let result = canonical_path_allow_missing(&link).unwrap();
        let expected = std::fs::canonicalize(&real_dir).unwrap();
        assert_eq!(result, expected);
    }
}
