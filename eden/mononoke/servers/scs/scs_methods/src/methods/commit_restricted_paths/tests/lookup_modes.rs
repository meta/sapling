/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

fn checked_restriction_options() -> ChangedPathsRestrictionOptions {
    ChangedPathsRestrictionOptions {
        check_permissions: true,
        include_permission_changes: true,
        include_subtree_source_changes: false,
    }
}

#[mononoke::fbinit_test]
async fn test_aggregate_requests_skip_opposite_file_policy_lookup(fb: FacebookInit) -> Result<()> {
    let lookups = Arc::new(AtomicUsize::new(0));
    let base = create_test_changeset_with_acl_provider(
        fb,
        vec![("secret", "REPO_REGION:acl")],
        TestAclProvider::counting(lookups.clone()),
    )
    .await?;
    let added_id = CreateCommitContext::new(base.ctx(), base.repo_ctx().repo(), vec![base.id()])
        .add_file("secret/file", "new")
        .commit()
        .await?;
    let added = base
        .repo_ctx()
        .changeset(added_id)
        .await?
        .context("added commit exists")?;
    let removed_id = CreateCommitContext::new(base.ctx(), base.repo_ctx().repo(), vec![added_id])
        .delete_file("secret/file")
        .commit()
        .await?;
    let removed = base
        .repo_ctx()
        .changeset(removed_id)
        .await?
        .context("removed commit exists")?;
    for tip in [added, removed] {
        lookups.store(0, Ordering::Relaxed);
        let legacy: Vec<(NonRootMPath, ChangedPathRestrictionInfo)> = tip
            .changed_paths_restriction_info(
                None,
                None,
                ChangedPathsRestrictionOptions {
                    check_permissions: true,
                    include_permission_changes: false,
                    include_subtree_source_changes: false,
                },
            )
            .await?
            .try_collect()
            .await?;
        assert_eq!(
            lookups.load(Ordering::Relaxed),
            1,
            "only the actual file side is checked"
        );
        assert!(legacy[0].1.permission_change.is_none());
        lookups.store(0, Ordering::Relaxed);
        let rich: Vec<(NonRootMPath, ChangedPathRestrictionInfo)> = tip
            .changed_paths_restriction_info(None, None, checked_restriction_options())
            .await?
            .try_collect()
            .await?;
        assert_eq!(
            lookups.load(Ordering::Relaxed),
            2,
            "requested policy changes check the opposite absent-file side"
        );
        assert_eq!(legacy[0].1.sides, rich[0].1.sides);
        assert_eq!(legacy[0].1.copy_sources, rich[0].1.copy_sources);
        assert_eq!(
            changes_restriction_results_into_response(legacy),
            changes_restriction_results_into_response(rich)
        );
    }
    Ok(())
}
