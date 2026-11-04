/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::anyhow;
use futures::future::try_join;

use super::*;

/// Per-path resolution over one fixed endpoint comparison.
pub(super) struct Comparison<R: MononokeRepo> {
    pub(super) target: ChangesetContext<R>,
    pub(super) base: Option<ChangesetContext<R>>,
    pub(super) bonsai: Option<Arc<BonsaiChangeset>>,
    pub(super) subtree_sources: Arc<manifest::PathTree<Option<(ChangesetContext<R>, MPath)>>>,
    pub(super) check_permissions: bool,
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
        let (copy_sources, source) = try_join(
            self.ordinary_sources(&change, &path),
            self.subtree_source(&change),
        )
        .await?;
        let sides = self
            .literal_sides(
                &change,
                &path,
                source.as_ref().and_then(sources::SubtreePolicy::file),
            )
            .await?;
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
                ChangedPathRestrictionSides::Changed { base, target }
                | ChangedPathRestrictionSides::Unchanged { base, target } => {
                    (base.clone(), target.clone())
                }
                ChangedPathRestrictionSides::SourceOnly(_) => {
                    try_join(
                        self.base()?
                            .restriction_side_info(&path, self.check_permissions),
                        self.target
                            .restriction_side_info(&path, self.check_permissions),
                    )
                    .await?
                }
            };
            PathPermissionChangeInfo::between(
                base,
                target,
                &copy_sources,
                source.as_ref().map(|source| source.policy().clone()),
            )
        } else {
            None
        };
        let subtree_source = match &sides {
            ChangedPathRestrictionSides::SourceOnly(_) => None,
            _ => source.and_then(sources::SubtreePolicy::into_file),
        };
        Ok((
            path,
            ChangedPathRestrictionInfo {
                sides,
                copy_sources,
                subtree_source,
                permission_change,
            },
        ))
    }

    async fn literal_sides(
        &self,
        change: &ChangedPath,
        path: &NonRootMPath,
        source: Option<&SubtreeSourcePolicyInfo>,
    ) -> Result<ChangedPathRestrictionSides, MononokeError> {
        Ok(match change {
            ChangedPath::Added(_) => ChangedPathRestrictionSides::Added(
                self.target
                    .restriction_side_info(path, self.check_permissions)
                    .await?,
            ),
            ChangedPath::Removed(_) => ChangedPathRestrictionSides::Removed(
                self.base()?
                    .restriction_side_info(path, self.check_permissions)
                    .await?,
            ),
            ChangedPath::Changed(_) => {
                let (base, target) = try_join(
                    self.base()?
                        .restriction_side_info(path, self.check_permissions),
                    self.target
                        .restriction_side_info(path, self.check_permissions),
                )
                .await?;
                ChangedPathRestrictionSides::Changed { base, target }
            }
            ChangedPath::SubtreeRelative { target_exists, .. } => {
                let base_exists = self
                    .base()?
                    .restriction_copy_source_is_file(change.path())
                    .await?;
                let (base, target) = try_join(
                    async {
                        if base_exists {
                            self.base()?
                                .restriction_side_info(path, self.check_permissions)
                                .await
                                .map(Some)
                        } else {
                            Ok(None)
                        }
                    },
                    async {
                        if *target_exists {
                            self.target
                                .restriction_side_info(path, self.check_permissions)
                                .await
                                .map(Some)
                        } else {
                            Ok(None)
                        }
                    },
                )
                .await?;
                match (base, target) {
                    (Some(base), Some(target)) => {
                        ChangedPathRestrictionSides::Unchanged { base, target }
                    }
                    (Some(base), None) => ChangedPathRestrictionSides::Removed(base),
                    (None, Some(target)) => ChangedPathRestrictionSides::Added(target),
                    (None, None) => ChangedPathRestrictionSides::SourceOnly(
                        source
                            .ok_or_else(|| anyhow!("source-relative deletion has no source file"))?
                            .clone(),
                    ),
                }
            }
        })
    }
}
