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
    pub(super) bonsai: Option<Arc<BonsaiChangeset>>,
    pub(super) include_permission_changes: bool,
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
        let copy_sources = self.ordinary_sources(&change, &path).await?;
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
        let permission_change = if self.include_permission_changes {
            let (base, target) = match &sides {
                ChangedPathRestrictionSides::Added(target) => {
                    let base = match &self.base {
                        Some(base) => {
                            base.restriction_side_info(&path, self.check_permissions)
                                .await?
                        }
                        None => PathPolicyInfo {
                            restrictions: Vec::new(),
                            has_access: self.check_permissions.then_some(true),
                        },
                    };
                    (base, target.clone())
                }
                ChangedPathRestrictionSides::Removed(base) => (
                    base.clone(),
                    self.target
                        .restriction_side_info(&path, self.check_permissions)
                        .await?,
                ),
                ChangedPathRestrictionSides::Changed { base, target } => {
                    (base.clone(), target.clone())
                }
            };
            PathPermissionChangeInfo::between(base, target, &copy_sources)
        } else {
            None
        };
        Ok((
            path,
            ChangedPathRestrictionInfo {
                sides,
                copy_sources,
                permission_change,
            },
        ))
    }
}
