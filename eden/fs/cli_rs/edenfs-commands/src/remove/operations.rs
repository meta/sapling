/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

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

// Validate and canonicalize the given path into absolute path with the type of PathBuf.
// Then determine a type for this path.
//
// Returns a tuple of:
//   1. Canonicalized path
//   2. Type of path
pub async fn classify_path(path: &str) -> Result<(PathBuf, PathType)> {
    let path_buf = PathBuf::from(path);

    match path_buf.canonicalize() {
        // A registered checkout whose folder is gone can still be removed.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let Ok(path) = edenfs_remove::canonicalize_checkout_path(&path_buf).await else {
                return Err(e.into());
            };
            match edenfs_remove::classify_checkout(get_edenfs_instance(), &path).await {
                Ok(LocalCheckoutState::Inactive) => Ok((path, PathType::InactiveEdenMount)),
                Err(error) => Err(error),
                _ => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
        Ok(canonicalized_path) => {
            let path = canonicalized_path.as_path();
            if path.is_file() {
                return Ok((
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

            let path_type =
                match edenfs_remove::classify_checkout(get_edenfs_instance(), path).await? {
                    LocalCheckoutState::Active => PathType::ActiveEdenMount,
                    LocalCheckoutState::Inactive => PathType::InactiveEdenMount,
                    // A directory that is not listed inside config.json. We don't know how
                    // to handle it properly, so try to handle it from here with best efforts.
                    LocalCheckoutState::Unknown => PathType::Unknown,
                };
            debug!("path {} is determined to be {}", path.display(), path_type);
            Ok((canonicalized_path, path_type))
        }
    }
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

        let result = classify_path(file_path_buf.to_str().unwrap()).await;
        assert!(
            result.is_ok(),
            "path of a regular file should be classified"
        );
        let (p, t) = result.unwrap();
        assert!(
            p == file_path_buf.canonicalize().unwrap(),
            "path of a regular file should be canonicalized"
        );
        assert!(
            matches!(t, PathType::RegularFile),
            "path of a regular file should be classified as RegFile"
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

        let (p, t) = classify_path(link.to_str().unwrap()).await.unwrap();
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
