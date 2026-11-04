/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

mod endpoints;

fn rich_restriction_options(check_permissions: bool) -> ChangedPathsRestrictionOptions {
    ChangedPathsRestrictionOptions { check_permissions }
}

async fn comparison_restrictions(
    tip: &ChangesetContext<Repo>,
    base: Option<&ChangesetContext<Repo>>,
) -> Result<BTreeMap<NonRootMPath, Vec<PathAccessInfo>>> {
    Ok(tip
        .changed_paths_restriction_info(base, None, rich_restriction_options(false))
        .await?
        .map_ok(|(path, info)| {
            let mut infos: Vec<_> = info.restriction_info().cloned().collect();
            infos.sort_by(|left, right| {
                (
                    left.restriction_root(),
                    left.repo_region_acl(),
                    left.permission_request_group(),
                    &left.restriction.rollout_allowlist_group,
                    left.has_access,
                )
                    .cmp(&(
                        right.restriction_root(),
                        right.repo_region_acl(),
                        right.permission_request_group(),
                        &right.restriction.rollout_allowlist_group,
                        right.has_access,
                    ))
            });
            infos.dedup();
            (path, infos)
        })
        .try_collect()
        .await?)
}
