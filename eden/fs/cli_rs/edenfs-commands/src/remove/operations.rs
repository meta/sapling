/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use edenfs_remove::LocalCheckoutState;
use fail::fail_point;
use tracing::debug;
use tracing::warn;

use super::types::PathType;
use super::types::RemoveContext;
use super::utils;
use crate::get_edenfs_instance;

/// What `remove` decided about one path argument.
pub enum Classification {
    /// Remove the canonicalized path as the given type.
    Remove(PathBuf, PathType),
    /// The path is not an EdenFS checkout and the command may not ask: nothing is removed,
    /// and `remove` exits `EX_NOINPUT` so that the DSU pipeline does not count it as an
    /// EdenFS failure.
    Refuse(String),
}

// Validate and canonicalize the given path into absolute path with the type of PathBuf.
// Then determine a type for this path.
//
// `non_interactive` is `--yes`: a plain file or a directory with content that is not a
// checkout cannot be confirmed, so it is refused instead of deleted.
pub async fn classify_path(path: &str, non_interactive: bool) -> Result<Classification> {
    let path_buf = PathBuf::from(path);

    let canonicalized_path = match path_buf.canonicalize() {
        // A registered checkout whose folder is gone can still be removed. Any other
        // missing path is not an EdenFS checkout.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Ok(path) = edenfs_remove::canonicalize_checkout_path(&path_buf).await
                && let LocalCheckoutState::Inactive =
                    edenfs_remove::classify_checkout(get_edenfs_instance(), &path).await?
            {
                return Ok(Classification::Remove(path, PathType::InactiveEdenMount));
            }
            return Ok(Classification::Refuse(format!(
                "error: {path} is neither an EdenFS mount nor an existing directory"
            )));
        }
        Err(e) => return Err(e.into()),
        Ok(canonicalized_path) => canonicalized_path,
    };
    let path = canonicalized_path.as_path();
    if path.is_file() {
        if non_interactive {
            return Ok(Classification::Refuse(format!(
                "error: {} exists but it's a file; remove it manually.",
                path.display()
            )));
        }
        return Ok(Classification::Remove(
            path_to_remove_for_file(&path_buf, canonicalized_path)?,
            PathType::RegularFile,
        ));
    }

    if !path.is_dir() {
        // This is rare, but when it happens we should warn it.
        warn!(
            "path {} is not a file or directory, please make sure it exists and you have permission to it.",
            path.display()
        );
        return Err(anyhow!("Not a file or directory"));
    }

    debug!("{} is determined as a directory", path.display());

    let path_type = match edenfs_remove::classify_checkout(get_edenfs_instance(), path).await? {
        LocalCheckoutState::Active => PathType::ActiveEdenMount,
        LocalCheckoutState::Inactive => PathType::InactiveEdenMount,
        // A directory that is not listed inside config.json. We don't know how
        // to handle it properly, so try to handle it from here with best efforts.
        LocalCheckoutState::Unknown => PathType::Unknown,
        LocalCheckoutState::InsideCheckout { root } => {
            return Ok(Classification::Refuse(format!(
                "error: {} is not the root of checkout {}, not removing",
                path.display(),
                root.display()
            )));
        }
    };
    debug!("path {} is determined to be {}", path.display(), path_type);
    if path_type == PathType::Unknown && non_interactive && !is_leftover_mount_point(path) {
        // The prompt that protects the directory's content cannot be shown.
        return Ok(Classification::Refuse(format!(
            "error: {} is not an EdenFS mount and is not empty; refusing to delete it without confirmation",
            path.display()
        )));
    }
    Ok(Classification::Remove(canonicalized_path, path_type))
}

/// True when the directory holds nothing but what EdenFS itself leaves at a mount point:
/// it is empty, or it holds only the not-mounted README as a plain file. An entry that
/// cannot be read counts as content: the directory is deleted only when every entry is known.
/// The Python CLI uses the same rule.
fn is_leftover_mount_point(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    entries.into_iter().all(|entry| {
        let Ok(entry) = entry else {
            return false;
        };
        entry.file_name() == "README_EDEN.txt"
            && entry.file_type().is_ok_and(|file_type| file_type.is_file())
    })
}

fn path_to_remove_for_file(given_path: &Path, canonicalized_path: PathBuf) -> Result<PathBuf> {
    if !given_path.symlink_metadata()?.file_type().is_symlink() {
        return Ok(canonicalized_path);
    }
    let link_name = given_path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", given_path.display()))?;
    let link_parent = match given_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    Ok(link_parent.canonicalize()?.join(link_name))
}

pub async fn clean_up(context: &RemoveContext) -> Result<()> {
    if context.preserve_mount_point {
        context.io.warn(format!(
            "preserve_mount_point flag is set, not removing the mount point {}!",
            context.original_path
        ));
        Ok(())
    } else {
        context.io.info(format!(
            "Cleaning up the directory {} ...",
            context.original_path
        ));
        utils::clean_mount_point(&context.canonical_path)
            .with_context(|| anyhow!("Failed to clean mount point {context}"))?;
        context.io.done();

        validate_removal_completion(context).await
    }
}

async fn validate_removal_completion(context: &RemoveContext) -> Result<()> {
    context
        .io
        .info("Checking eden mount list and file system to verify the removal...".to_string());
    // check eden list
    if utils::path_in_eden_config(context.canonical_path.as_path()).await? {
        return Err(anyhow!("Repo {context} is still mounted"));
    }

    fail_point!("remove:validate", |_| {
        Err(anyhow!("failpoint: expected failure"))
    });

    // check directory clean up
    if !context.preserve_mount_point {
        match context.canonical_path.try_exists() {
            Ok(false) => {
                context.io.done();
                Ok(())
            }
            Ok(true) => Err(anyhow!("Directory left by repo {context} is not removed")),
            Err(e) => Err(anyhow!("Failed to check the status of path {context}: {e}")),
        }
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Context;
    use tempfile::TempDir;
    use tempfile::tempdir;

    use super::*;

    /// This helper function creates a directory structure that looks like this:
    /// "some_tmp_dir/test/nested/inner"
    /// then it returns the path to the "some_tmp_dir" directory
    fn prepare_directory() -> TempDir {
        let temp_dir = tempdir().context("couldn't create temp dir").unwrap();
        let path = temp_dir.path().join("test").join("nested").join("inner");
        let prefix = path.parent().unwrap();
        println!("creating dirs: {:?}", prefix.to_str().unwrap());
        std::fs::create_dir_all(prefix).unwrap();
        temp_dir
    }

    #[tokio::test]
    async fn test_classify_path_regular_file() {
        let temp_dir = prepare_directory();
        let file_path_buf = temp_dir.path().join("temporary-file.txt");
        fs::write(file_path_buf.as_path(), "anything").unwrap_or_else(|err| {
            panic!(
                "cannot write to a file at {}: {}",
                file_path_buf.display(),
                err
            )
        });

        let result = classify_path(file_path_buf.to_str().unwrap(), false).await;
        assert!(
            result.is_ok(),
            "path of a regular file should be classified"
        );
        let Classification::Remove(p, t) = result.unwrap() else {
            panic!("an interactive run should offer to remove a regular file");
        };
        assert!(
            p == file_path_buf.canonicalize().unwrap(),
            "path of a regular file should be canonicalized"
        );
        assert!(
            matches!(t, PathType::RegularFile),
            "path of a regular file should be classified as RegFile"
        );
    }

    #[tokio::test]
    async fn test_classify_path_refuses_a_regular_file_without_a_prompt() {
        let temp_dir = prepare_directory();
        let file_path_buf = temp_dir.path().join("temporary-file.txt");
        fs::write(file_path_buf.as_path(), "anything").unwrap();

        let result = classify_path(file_path_buf.to_str().unwrap(), true)
            .await
            .unwrap();
        assert!(
            matches!(result, Classification::Refuse(_)),
            "--yes must not delete a plain file"
        );
        assert!(file_path_buf.exists(), "the file must be left alone");
    }

    #[test]
    fn test_leftover_mount_point_holds_only_edenfs_leftovers() {
        let temp_dir = prepare_directory();
        let empty = temp_dir.path().join("empty");
        fs::create_dir(&empty).unwrap();
        let readme_only = temp_dir.path().join("readme-only");
        fs::create_dir(&readme_only).unwrap();
        fs::write(readme_only.join("README_EDEN.txt"), "").unwrap();
        let with_content = temp_dir.path().join("with-content");
        fs::create_dir(&with_content).unwrap();
        fs::write(with_content.join("README_EDEN.txt"), "").unwrap();
        fs::write(with_content.join("notes.txt"), "keep me").unwrap();
        let readme_dir = temp_dir.path().join("readme-dir");
        fs::create_dir_all(readme_dir.join("README_EDEN.txt")).unwrap();

        assert!(
            is_leftover_mount_point(&empty),
            "an empty directory is a leftover"
        );
        assert!(
            is_leftover_mount_point(&readme_only),
            "only EdenFS's README is a leftover"
        );
        assert!(
            !is_leftover_mount_point(&readme_dir),
            "a directory named like the README is content"
        );
        assert!(
            !is_leftover_mount_point(&with_content),
            "other content is not a leftover"
        );
        assert!(
            !is_leftover_mount_point(&temp_dir.path().join("missing")),
            "a missing directory is not a leftover"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_classify_path_symlink_to_regular_file() {
        let temp_dir = prepare_directory();
        let target = temp_dir.path().join("target.txt");
        fs::write(&target, "anything").unwrap();
        let link = temp_dir.path().join("link-to-target");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let Classification::Remove(p, t) =
            classify_path(link.to_str().unwrap(), false).await.unwrap()
        else {
            panic!("an interactive run should offer to remove a symlink to a file");
        };
        assert!(matches!(t, PathType::RegularFile));
        assert_eq!(
            p,
            temp_dir
                .path()
                .canonicalize()
                .unwrap()
                .join("link-to-target")
        );

        fs::remove_file(&p).unwrap();
        assert!(target.exists());
        assert!(link.symlink_metadata().is_err());
    }
}
