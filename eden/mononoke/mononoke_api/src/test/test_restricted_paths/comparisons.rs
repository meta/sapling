/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

mod copies;
mod endpoints;

fn rich_restriction_options(check_permissions: bool) -> ChangedPathsRestrictionOptions {
    ChangedPathsRestrictionOptions { check_permissions }
}

async fn assert_rich_restriction_pages(
    tip: &ChangesetContext<Repo>,
    base: Option<&ChangesetContext<Repo>>,
    check_permissions: bool,
) -> Result<Vec<(NonRootMPath, ChangedPathRestrictionInfo)>> {
    let full = tip
        .changed_paths_restriction_info(base, None, rich_restriction_options(check_permissions))
        .await?
        .try_collect::<Vec<_>>()
        .await?;
    for limit in [1, 2, 3] {
        let mut after = None;
        let mut joined = Vec::new();
        loop {
            let mut page = tip
                .changed_paths_restriction_info(
                    base,
                    after.clone(),
                    rich_restriction_options(check_permissions),
                )
                .await?
                .take(limit + 1)
                .try_collect::<Vec<_>>()
                .await?;
            let has_more = page.len() > limit;
            page.truncate(limit);
            if let Some((path, _)) = page.last() {
                after = Some(MPath::from(path.clone()));
            }
            joined.extend(page);
            if !has_more {
                break;
            }
        }
        assert_eq!(
            joined, full,
            "paging preserves all literal-side and source metadata"
        );
    }
    for (cursor, _) in &full {
        let suffix = tip
            .changed_paths_restriction_info(
                base,
                Some(MPath::from(cursor.clone())),
                rich_restriction_options(check_permissions),
            )
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        assert_eq!(
            suffix,
            full.iter()
                .filter(|(path, _)| path > cursor)
                .cloned()
                .collect::<Vec<_>>()
        );
    }
    Ok(full)
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
