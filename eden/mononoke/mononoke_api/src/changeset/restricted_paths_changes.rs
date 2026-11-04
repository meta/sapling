/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::anyhow;
use futures::future::try_join;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use manifest::Diff;
use manifest::Entry;
use manifest::ManifestOps;
use manifest::ManifestOrderedOps;
use manifest::PathOrPrefix;
use mononoke_types::BonsaiChangeset;
use mononoke_types::FileChange;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use repo_blobstore::RepoBlobstoreRef;

use super::ChangesetContext;
use crate::MononokeRepo;
use crate::errors::MononokeError;
use crate::restricted_paths::ChangedPathRestrictionInfo;
use crate::restricted_paths::ChangedPathRestrictionSides;
use crate::restricted_paths::ChangedPathsRestrictionOptions;
use crate::restricted_paths::PathAccessInfo;
use crate::restricted_paths::PathPermissionChangeInfo;
use crate::restricted_paths::PathPolicyInfo;
use crate::restricted_paths::PathRestrictionCopySourceInfo;
use crate::restricted_paths::RestrictedChangeGroup;
use crate::restricted_paths::RestrictedPathsChangesInfo;

mod resolution;
mod sources;

const RESTRICTION_LOOKUP_CONCURRENCY: usize = 25;

enum ChangedPath {
    Added(MPath),
    Removed(MPath),
    Changed(MPath),
}

impl ChangedPath {
    fn path(&self) -> &MPath {
        match self {
            Self::Added(path) | Self::Removed(path) | Self::Changed(path) => path,
        }
    }
}

impl<R: MononokeRepo> ChangesetContext<R> {
    /// Stream net manifest leaf differences and their actual endpoint restrictions.
    /// Base defaults to the first parent, or an empty tree for a root commit.
    /// Denied paths remain metadata; `after_path` is exclusive in component order.
    pub async fn changed_paths_restriction_info(
        &self,
        base: Option<&Self>,
        after_path: Option<MPath>,
        options: ChangedPathsRestrictionOptions,
    ) -> Result<
        BoxStream<'static, Result<(NonRootMPath, ChangedPathRestrictionInfo), MononokeError>>,
        MononokeError,
    > {
        let ChangedPathsRestrictionOptions {
            check_permissions,
            include_permission_changes,
        } = options;
        let parents = self.parents().await?;
        let base = base.cloned().or_else(|| {
            parents
                .first()
                .map(|id| Self::new(self.repo_ctx().clone(), *id))
        });
        let bonsai = if base
            .as_ref()
            .is_some_and(|base| parents.contains(&base.id()))
        {
            Some(Arc::new(self.bonsai_changeset().await?))
        } else {
            None
        };
        let (new_root, old_root) = try_join(self.root_content_manifest_id(), async {
            match &base {
                Some(base) => base.root_content_manifest_id().await.map(Some),
                None => Ok(None),
            }
        })
        .await?;
        let store = self.repo_ctx().repo().repo_blobstore().clone();
        let changes = match old_root.zip(base.as_ref()) {
            Some((old_root, base)) => old_root
                .filtered_diff_ordered(
                    self.ctx().clone(),
                    base.repo_ctx().repo().repo_blobstore().clone(),
                    new_root,
                    store,
                    after_path.clone(),
                    |diff| match diff {
                        Diff::Added(path, Entry::Leaf(_)) => Some(ChangedPath::Added(path)),
                        Diff::Removed(path, Entry::Leaf(_)) => Some(ChangedPath::Removed(path)),
                        Diff::Changed(path, Entry::Leaf(_), Entry::Leaf(_)) => {
                            Some(ChangedPath::Changed(path))
                        }
                        _ => None,
                    },
                    |_| true,
                    Default::default(),
                )
                .map_err(MononokeError::from)
                .boxed(),
            None => new_root
                .find_entries_ordered(
                    self.ctx().clone(),
                    store,
                    [PathOrPrefix::Prefix(MPath::ROOT)],
                    after_path.clone(),
                )
                .try_filter_map(|(path, entry)| async move {
                    Ok(match entry {
                        Entry::Leaf(_) => Some(ChangedPath::Added(path)),
                        Entry::Tree(_) => None,
                    })
                })
                .map_err(MononokeError::from)
                .boxed(),
        };

        let comparison = Arc::new(resolution::Comparison {
            target: self.clone(),
            base,
            bonsai,
            check_permissions,
            include_permission_changes,
        });
        Ok(changes
            .map_ok(move |change| {
                let comparison = comparison.clone();
                async move { comparison.resolve(change).await }
            })
            .try_buffered(RESTRICTION_LOOKUP_CONCURRENCY)
            .boxed())
    }

    async fn restriction_side_info(
        &self,
        path: &NonRootMPath,
        check_permissions: bool,
    ) -> Result<PathPolicyInfo, MononokeError> {
        self.restriction_policy_info(path.clone().into(), check_permissions)
            .await
    }

    async fn restriction_policy_info(
        &self,
        path: MPath,
        check_permissions: bool,
    ) -> Result<PathPolicyInfo, MononokeError> {
        if !self.may_have_restricted_paths() {
            return Ok(PathPolicyInfo {
                restrictions: Vec::new(),
                has_access: check_permissions.then_some(true),
            });
        }
        let mut restrictions = self
            .path_restriction(path)
            .await?
            .restriction_info(check_permissions)
            .await?;
        restrictions.sort();
        restrictions.dedup();
        let has_access = check_permissions.then(|| {
            restrictions
                .iter()
                .all(|info| info.has_access == Some(true))
        });
        Ok(PathPolicyInfo {
            restrictions,
            has_access,
        })
    }

    async fn restriction_file_copy_source(
        &self,
        bonsai: &BonsaiChangeset,
        path: &NonRootMPath,
        base: &Self,
    ) -> Result<Option<(Self, NonRootMPath)>, MononokeError> {
        let overridden = self
            .mutable_history
            .as_ref()
            .and_then(|history| history.get(&MPath::from(path.clone())));
        let copy_from = match overridden {
            Some(history) if history.is_override() => {
                history.get_copy_from().map(|(id, path)| (id, path.clone()))
            }
            _ => match bonsai.file_changes_map().get(path) {
                Some(FileChange::Change(change)) => change
                    .copy_from()
                    .map(|(path, id)| (*id, MPath::from(path.clone()))),
                _ => None,
            },
        };
        let Some((source_id, source_path)) = copy_from else {
            return Ok(None);
        };
        if source_id != base.id() {
            return Ok(None);
        }
        let root = base.root_content_manifest_id().await?;
        let store = base.repo_ctx().repo().repo_blobstore().clone();
        let (destination, source) = try_join(
            root.find_entry(base.ctx().clone(), store.clone(), MPath::from(path.clone())),
            root.find_entry(base.ctx().clone(), store, source_path.clone()),
        )
        .await?;
        if destination.is_some() || !matches!(source, Some(Entry::Leaf(_))) {
            return Ok(None);
        }
        Ok(Some((base.clone(), NonRootMPath::try_from(source_path)?)))
    }

    /// Group net changed files by full restriction identity and access result.
    ///
    /// Uses the first parent, or an empty tree for a root commit. A path can
    /// appear in multiple groups. Permission checks do not enforce content access.
    pub async fn restricted_paths_changes(
        &self,
        check_permissions: bool,
    ) -> Result<RestrictedPathsChangesInfo, MononokeError> {
        let groups: BTreeMap<PathAccessInfo, Vec<NonRootMPath>> = self
            .changed_paths_restriction_info(
                None,
                None,
                ChangedPathsRestrictionOptions {
                    check_permissions,
                    include_permission_changes: false,
                },
            )
            .await?
            .try_fold(
                BTreeMap::<PathAccessInfo, Vec<NonRootMPath>>::new(),
                |mut groups, (path, info)| async move {
                    for info in info.restriction_info().cloned() {
                        groups.entry(info).or_default().push(path.clone());
                    }
                    Ok(groups)
                },
            )
            .await?;
        let restricted_changes = groups
            .into_iter()
            .map(|(restriction_info, mut changed_paths)| {
                changed_paths.sort();
                changed_paths.dedup();
                RestrictedChangeGroup {
                    restriction_info,
                    changed_paths,
                }
            })
            .collect();
        Ok(RestrictedPathsChangesInfo { restricted_changes })
    }
}
