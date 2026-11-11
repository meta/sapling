/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Context;
use anyhow::Result;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use serde_derive::Deserialize;
use serde_derive::Serialize;
use thiserror::Error;

use crate::subtree::mpath;

const SNAPSHOT_STATE_VERSION: u64 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotHgState {
    pub untracked: Vec<NonRootMPath>,
    pub missing: Vec<NonRootMPath>,
}

#[derive(Debug, Error)]
pub enum SnapshotHgStateError {
    #[error("Unsupported version of snapshot state: {0}")]
    UnsupportedVersion(u64),
    #[error("Snapshot state contains the root path")]
    RootPath,
}

#[derive(Serialize, Deserialize)]
struct SnapshotHgStateJson {
    v: u64,
    untracked: Vec<SnapshotPathJson>,
    missing: Vec<SnapshotPathJson>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotPathJson(#[serde(with = "mpath")] MPath);

impl SnapshotHgState {
    pub fn new(
        untracked: impl IntoIterator<Item = NonRootMPath>,
        missing: impl IntoIterator<Item = NonRootMPath>,
    ) -> Self {
        Self {
            untracked: sorted_dedup(untracked),
            missing: sorted_dedup(missing),
        }
    }

    pub fn to_json(&self) -> Result<String> {
        let json = SnapshotHgStateJson {
            v: SNAPSHOT_STATE_VERSION,
            untracked: to_json_paths(&self.untracked),
            missing: to_json_paths(&self.missing),
        };
        serde_json::to_string(&json).context("Failed to serialize snapshot state")
    }

    pub fn from_json(json: &[u8]) -> Result<Self> {
        let parsed: SnapshotHgStateJson =
            serde_json::from_slice(json).context("Failed to parse snapshot state")?;
        if parsed.v != SNAPSHOT_STATE_VERSION {
            return Err(SnapshotHgStateError::UnsupportedVersion(parsed.v).into());
        }
        Ok(Self {
            untracked: from_json_paths(parsed.untracked)?,
            missing: from_json_paths(parsed.missing)?,
        })
    }
}

fn sorted_dedup(paths: impl IntoIterator<Item = NonRootMPath>) -> Vec<NonRootMPath> {
    let mut paths: Vec<_> = paths.into_iter().collect();
    paths.sort_by(NonRootMPath::compare_bytes);
    paths.dedup();
    paths
}

fn to_json_paths(paths: &[NonRootMPath]) -> Vec<SnapshotPathJson> {
    paths
        .iter()
        .map(|path| SnapshotPathJson(MPath::from(path.clone())))
        .collect()
}

fn from_json_paths(paths: Vec<SnapshotPathJson>) -> Result<Vec<NonRootMPath>> {
    paths
        .into_iter()
        .map(|SnapshotPathJson(path)| {
            path.into_optional_non_root_path()
                .ok_or_else(|| SnapshotHgStateError::RootPath.into())
        })
        .collect()
}
