/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::anyhow;
use edenfs_error::EdenFsError;
use edenfs_error::Result;
use edenfs_utils::bytes_from_path;

use crate::client::Client;
use crate::client::EdenFsClient;
use crate::methods::EdenThriftMethod;
use crate::types::RootIdOptions;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum ScmFileStatus {
    Added = 0,
    Modified = 1,
    Removed = 2,
    Ignored = 3,
    Undefined = -1,
}

impl From<thrift_types::edenfs::ScmFileStatus> for ScmFileStatus {
    fn from(from: thrift_types::edenfs::ScmFileStatus) -> Self {
        match from {
            thrift_types::edenfs::ScmFileStatus::ADDED => ScmFileStatus::Added,
            thrift_types::edenfs::ScmFileStatus::MODIFIED => ScmFileStatus::Modified,
            thrift_types::edenfs::ScmFileStatus::REMOVED => ScmFileStatus::Removed,
            thrift_types::edenfs::ScmFileStatus::IGNORED => ScmFileStatus::Ignored,
            _ => ScmFileStatus::Undefined,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScmStatusDetails {
    pub entries: BTreeMap<Vec<u8>, ScmFileStatus>,
    pub errors: BTreeMap<Vec<u8>, String>,
}

impl From<thrift_types::edenfs::ScmStatus> for ScmStatusDetails {
    fn from(from: thrift_types::edenfs::ScmStatus) -> Self {
        Self {
            entries: from
                .entries
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            errors: from.errors,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScmStatus {
    pub status: ScmStatusDetails,
    pub version: String,
}

impl From<thrift_types::edenfs::GetScmStatusResult> for ScmStatus {
    fn from(from: thrift_types::edenfs::GetScmStatusResult) -> Self {
        Self {
            status: from.status.into(),
            version: from.version,
        }
    }
}

impl EdenFsClient {
    pub async fn get_scm_status_v2(
        &self,
        mount_point: PathBuf,
        commit_str: String,
        list_ignored: bool,
        root_id_options: Option<RootIdOptions>,
    ) -> Result<ScmStatus> {
        let get_scm_status_params = thrift_types::edenfs::GetScmStatusParams {
            mountPoint: bytes_from_path(mount_point)?,
            commit: commit_str.as_bytes().to_vec(),
            listIgnored: list_ignored,
            rootIdOptions: root_id_options.map(|r| r.into()),
            ..Default::default()
        };
        self.with_thrift(|thrift| {
            (
                thrift.getScmStatusV2(&get_scm_status_params),
                EdenThriftMethod::GetScmStatusV2,
            )
        })
        .await
        .map(|scm_status| scm_status.into())
        .map_err(|e| EdenFsError::Other(anyhow!("failed to get scm status v2 result: {e}")))
    }
}
