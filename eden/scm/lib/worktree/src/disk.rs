/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Optional free-space advisory for new worktrees.

use std::path::Path;

use anyhow::Result;
use anyhow::bail;
use configmodel::Config;
use configmodel::ConfigExt;
use configmodel::convert::ByteCount;
use fsinfo::FsSpace;

/// Measure free space at a new worktree's destination when a warning threshold
/// is configured. An absent, empty, or zero threshold disables the check.
/// Returns the measured space and whether it is below the warning threshold.
/// A failed measurement is reported to the caller, which counts the error and
/// lets the add continue without blocking.
pub fn check_disk_space(config: &dyn Config, dest: &Path) -> Result<Option<(FsSpace, bool)>> {
    let Some(threshold) =
        config.get_nonempty_opt::<ByteCount>("worktree", "disk-warning-threshold")?
    else {
        return Ok(None);
    };
    if threshold.value() == 0 {
        return Ok(None);
    }

    let Some(existing) = dest.ancestors().find(|path| path.is_dir()) else {
        bail!("no existing directory ancestor for {}", dest.display());
    };
    let space = fsinfo::fs_space(existing)?;

    Ok(Some((space, space.available < threshold.value())))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_off_when_unset_empty_or_zero() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("new");
        for config in [
            BTreeMap::new(),
            BTreeMap::from([("worktree.disk-warning-threshold", "")]),
            BTreeMap::from([("worktree.disk-warning-threshold", "0")]),
        ] {
            assert_eq!(check_disk_space(&config, &dest).unwrap(), None);
        }
    }

    #[test]
    fn test_measure_nearest_existing_directory_and_compare_units() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("not-yet-created").join("worktree");
        let config = BTreeMap::from([("worktree.disk-warning-threshold", "9999TB")]);
        let expected_total = fsinfo::fs_space(dir.path()).unwrap().total;
        let (space, below_warning) = check_disk_space(&config, &dest).unwrap().unwrap();
        assert_eq!(space.total, expected_total);
        assert!(below_warning, "9999TB threshold should warn on a test host");

        let config = BTreeMap::from([("worktree.disk-warning-threshold", "1B")]);
        let (space, below_warning) = check_disk_space(&config, &dest).unwrap().unwrap();
        assert_eq!(below_warning, space.available < 1);
    }

    #[test]
    fn test_missing_existing_directory_is_reported() {
        let config = BTreeMap::from([("worktree.disk-warning-threshold", "100GB")]);
        assert!(check_disk_space(&config, Path::new("")).is_err());
    }

    #[test]
    fn test_invalid_threshold_is_reported_for_optional_check() {
        let dir = TempDir::new().unwrap();
        let config = BTreeMap::from([("worktree.disk-warning-threshold", "not-a-size")]);
        assert!(check_disk_space(&config, dir.path()).is_err());
    }
}
