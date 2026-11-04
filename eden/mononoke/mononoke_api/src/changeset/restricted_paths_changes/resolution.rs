/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

pub(super) struct Comparison<R: MononokeRepo> {
    pub(super) target: ChangesetContext<R>,
    pub(super) base: Option<ChangesetContext<R>>,
    pub(super) check_permissions: bool,
}
impl<R: MononokeRepo> Comparison<R> {
    fn base(&self) -> Result<&ChangesetContext<R>, MononokeError> {
        self.base
            .as_ref()
            .ok_or_else(|| anyhow!("comparison path requires a base").into())
    }
    pub(super) async fn resolve(
        &self,
        change: ChangedPath,
    ) -> Result<(NonRootMPath, ChangedPathRestrictionInfo), MononokeError> {
        let path = NonRootMPath::try_from(change.path().clone())?;
        let sides = match change {
            ChangedPath::Added(_) => ChangedPathRestrictionSides::Added(
                self.target
                    .restriction_side_info(&path, self.check_permissions)
                    .await?,
            ),
            ChangedPath::Removed(_) => ChangedPathRestrictionSides::Removed(
                self.base()?
                    .restriction_side_info(&path, self.check_permissions)
                    .await?,
            ),
            ChangedPath::Changed(_) => {
                let (base, target) = try_join(
                    self.base()?
                        .restriction_side_info(&path, self.check_permissions),
                    self.target
                        .restriction_side_info(&path, self.check_permissions),
                )
                .await?;
                ChangedPathRestrictionSides::Changed { base, target }
            }
        };
        Ok((path, ChangedPathRestrictionInfo { sides }))
    }
}
