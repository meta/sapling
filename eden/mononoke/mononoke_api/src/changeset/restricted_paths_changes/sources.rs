/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::resolution::Comparison;
use super::*;

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
}
