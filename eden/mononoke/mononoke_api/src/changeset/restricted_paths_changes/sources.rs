/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::resolution::Comparison;
use super::*;

pub(super) enum SubtreePolicy {
    File(SubtreeSourcePolicyInfo),
    AbsentFile(SubtreeSourcePolicyInfo),
}

impl SubtreePolicy {
    pub(super) fn policy(&self) -> &SubtreeSourcePolicyInfo {
        match self {
            Self::File(info) | Self::AbsentFile(info) => info,
        }
    }

    pub(super) fn file(&self) -> Option<&SubtreeSourcePolicyInfo> {
        match self {
            Self::File(info) => Some(info),
            Self::AbsentFile(_) => None,
        }
    }

    pub(super) fn into_file(self) -> Option<SubtreeSourcePolicyInfo> {
        match self {
            Self::File(info) => Some(info),
            Self::AbsentFile(_) => None,
        }
    }
}

impl<R: MononokeRepo> Comparison<R> {
    pub(super) async fn ordinary_sources(
        &self,
        change: &ChangedPath,
        path: &NonRootMPath,
    ) -> Result<Vec<PathRestrictionCopySourceInfo>, MononokeError> {
        let (Some(bonsai), Some(base)) = (&self.bonsai, &self.base) else {
            return Ok(Vec::new());
        };
        if !matches!(change, ChangedPath::Added(_)) {
            return Ok(Vec::new());
        }
        let Some((source, path)) = self
            .target
            .restriction_file_copy_source(bonsai, path, base)
            .await?
        else {
            return Ok(Vec::new());
        };
        let info = source
            .restriction_side_info(&path, self.check_permissions)
            .await?;
        Ok(vec![PathRestrictionCopySourceInfo {
            commit_id: source.id(),
            path,
            info,
        }])
    }

    pub(super) async fn subtree_source(
        &self,
        change: &ChangedPath,
    ) -> Result<Option<SubtreePolicy>, MononokeError> {
        let Some((destination_prefix, Some((source, source_prefix)))) = self
            .subtree_sources
            .get_nearest_parent(change.path(), Option::is_some)
        else {
            return Ok(None);
        };
        let path = change.path().reparent(&destination_prefix, source_prefix)?;
        let exists = source.restriction_copy_source_is_file(&path).await?;
        if !exists && !self.include_permission_changes {
            return Ok(None);
        }
        let info = SubtreeSourcePolicyInfo {
            mapping: SubtreeSourceMapping {
                commit_id: source.id(),
                source_prefix: source_prefix.clone(),
                destination_prefix,
            },
            info: source
                .restriction_policy_info(path, self.check_permissions)
                .await?,
        };
        Ok(Some(if exists {
            SubtreePolicy::File(info)
        } else {
            SubtreePolicy::AbsentFile(info)
        }))
    }
}
