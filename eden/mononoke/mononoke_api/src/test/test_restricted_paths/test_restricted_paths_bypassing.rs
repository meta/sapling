/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Tests verifying that `create_changeset` blocks unauthorized tampering
//! with `.slacl` files (code tenting ACLs) via `validate_acl_file_changes`,
//! while letting members of the repo's admin bypass group through.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use chrono::FixedOffset;
use chrono::TimeZone;
use fbinit::FacebookInit;
use maplit::hashmap;
use metadata::Metadata;
use mononoke_macros::mononoke;
use mononoke_types::RepositoryId;
use mononoke_types::path::MPath;
use permission_checker::Acl;
use permission_checker::Acls;
use permission_checker::InternalAclProvider;
use permission_checker::MononokeIdentity;
use permission_checker::MononokeIdentitySet;
use restricted_paths::RestrictedPaths;
use restricted_paths::RestrictedPathsConfigBased;
use restricted_paths::SqlRestrictedPathsManifestIdStoreBuilder;
use scuba_ext::MononokeScubaSampleBuilder;
use sql_construct::SqlConstruct;
use tests_utils::CreateCommitContext;

use crate::CreateChange;
use crate::CreateChangeFile;
use crate::CreateChangesetChecks;
use crate::CreateInfo;
use crate::Mononoke;
use crate::SessionContainer;
use crate::repo::Repo;

/// Sample .slacl file content.
const SLACL_CONTENT: &str = r#"{"version": 1, "acl": "REPO_REGION:repos/hg/test/=secret_project"}"#;

/// Alternate .slacl file content (different ACL).
const SLACL_CONTENT_2: &str =
    r#"{"version": 1, "acl": "REPO_REGION:repos/hg/test/=other_project"}"#;

/// The repo region ACL name that protects restricted directories.
const RESTRICTED_ACL: &str = "repos/hg/test/=secret_project";

/// The full `REPO_REGION:` identity string for the restricted ACL.
const RESTRICTED_IDENTITY: &str = "REPO_REGION:repos/hg/test/=secret_project";

/// The admin bypass group configured on every test repo, matching prod.
const ADMIN_BYPASS_GROUP: &str = "path_acls_admin_bypass";

/// What the test expects from `create_changeset`.
enum ExpectedOutcome {
    /// The operation must fail; the error message must contain the given substring.
    Blocked(&'static str),
    /// The operation must succeed.
    Allowed,
}

// ---------------------------------------------------------------------------
// Bypass tests — these should be BLOCKED by validate_acl_file_changes
// ---------------------------------------------------------------------------

/// Deleting a .slacl in a restricted directory is blocked —
/// an unauthorized user deleting .slacl bypasses code tenting.
#[mononoke::fbinit_test]
async fn test_acl_bypass_delete_slacl_in_restricted_dir(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;
    let changes = BTreeMap::from([(MPath::try_from("secret/.slacl")?, CreateChange::Deletion)]);

    run_single_changeset_test(
        fb,
        "unauthorized_user",
        repo,
        &[
            ("secret/.slacl", SLACL_CONTENT),
            ("secret/data.txt", "sensitive data"),
        ],
        changes,
        "delete .slacl",
        ExpectedOutcome::Blocked("does not have maintainer access"),
    )
    .await
}

/// Modifying .slacl content in a restricted directory is blocked —
/// changing the ACL bypasses the original restriction.
#[mononoke::fbinit_test]
async fn test_acl_bypass_modify_slacl_in_restricted_dir(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;
    let changes = BTreeMap::from([(
        MPath::try_from("secret/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT_2), None),
    )]);

    run_single_changeset_test(
        fb,
        "unauthorized_user",
        repo,
        &[("secret/.slacl", SLACL_CONTENT)],
        changes,
        "modify .slacl",
        ExpectedOutcome::Blocked("does not have maintainer access"),
    )
    .await
}

/// Replacing a restricted directory with a file (explicitly deleting
/// the .slacl and all contents) is blocked — this removes the ACL.
#[mononoke::fbinit_test]
async fn test_acl_bypass_implicit_delete_dir_to_file(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;
    let changes = BTreeMap::from([
        (
            MPath::try_from("secret")?,
            CreateChange::Tracked(
                CreateChangeFile::new_regular("replacing dir with file"),
                None,
            ),
        ),
        (MPath::try_from("secret/.slacl")?, CreateChange::Deletion),
        (MPath::try_from("secret/data.txt")?, CreateChange::Deletion),
    ]);

    run_single_changeset_test(
        fb,
        "unauthorized_user",
        repo,
        &[
            ("secret/.slacl", SLACL_CONTENT),
            ("secret/data.txt", "sensitive data"),
        ],
        changes,
        "replace dir with file",
        ExpectedOutcome::Blocked("does not have maintainer access"),
    )
    .await
}

/// Adding a nested .slacl under a restricted parent is blocked.
/// The parent .slacl protects all descendants, so adding a nested .slacl with
/// a different ACL requires the parent ACL.
#[mononoke::fbinit_test]
async fn test_acl_bypass_add_nested_slacl_under_restricted(fb: FacebookInit) -> Result<()> {
    let repo = build_restricted_dir_repo(fb).await?;
    let changes = BTreeMap::from([(
        MPath::try_from("restricted/subdir/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT_2), None),
    )]);

    run_single_changeset_test(
        fb,
        "unauthorized_user",
        repo,
        &[
            ("restricted/.slacl", SLACL_CONTENT),
            ("restricted/subdir/file.txt", "data"),
        ],
        changes,
        "add nested .slacl",
        ExpectedOutcome::Blocked("does not have maintainer access"),
    )
    .await
}

/// Adding a normal file inside a restricted directory should be blocked —
/// unauthorized users should not be able to modify ANY file in a restricted
/// directory, not just .slacl files.
#[mononoke::fbinit_test]
async fn test_acl_bypass_normal_file_in_restricted_dir(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;
    let changes = BTreeMap::from([(
        MPath::try_from("secret/new_file.txt")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("hello"), None),
    )]);

    run_single_changeset_test(
        fb,
        "unauthorized_user",
        repo,
        &[
            ("secret/.slacl", SLACL_CONTENT),
            ("secret/data.txt", "sensitive data"),
        ],
        changes,
        "add normal file in restricted dir",
        // DELIBERATELY NOT COVERED. Modifying non-.slacl files inside
        // restricted directories is allowed at create time — the threat
        // model is read access, which is enforced at read time via the
        // closest-public-ancestor AclManifest lookup. Adding write-time
        // validation here would require checking every file path against
        // the parent's AclManifest (expensive). Test kept to document
        // the policy choice.
        ExpectedOutcome::Allowed,
    )
    .await
}

/// Replacing a restricted directory with a file WITHOUT listing .slacl as
/// an explicit deletion. Adding a file at path "secret" implicitly removes
/// the directory "secret/" and everything in it, including "secret/.slacl".
/// The guard must detect this implicit removal.
#[mononoke::fbinit_test]
async fn test_acl_bypass_implicit_delete_no_explicit_slacl(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;
    // Only add "secret" as a file — do NOT list "secret/.slacl" as an
    // explicit deletion. The directory replacement implicitly deletes it.
    let changes = BTreeMap::from([(
        MPath::try_from("secret")?,
        CreateChange::Tracked(
            CreateChangeFile::new_regular("replacing dir with file"),
            None,
        ),
    )]);

    run_single_changeset_test(
        fb,
        "unauthorized_user",
        repo,
        &[
            ("secret/.slacl", SLACL_CONTENT),
            ("secret/data.txt", "sensitive data"),
        ],
        changes,
        "implicit delete via dir-to-file",
        // DELIBERATELY NOT COVERED. Detecting implicit .slacl deletes
        // requires find_restricted_descendants, which triggers AclManifest
        // derivation — too expensive for the changeset-creation hot path.
        // The .slacl is gone, but the attacker did not gain read access
        // to the protected code (the threat model is read access, not
        // destruction). Land-time hooks catch the missing .slacl before
        // it becomes public; read-time enforcement still blocks
        // unauthorized reads. Test kept to document the policy choice —
        // flip back to Blocked if we change our minds.
        ExpectedOutcome::Allowed,
    )
    .await
}

// ---------------------------------------------------------------------------
// Positive tests (should ALWAYS be allowed)
// ---------------------------------------------------------------------------

/// Adding a .slacl to an unrestricted directory is always allowed — creating
/// new restrictions in dirs that have none is fine.
#[mononoke::fbinit_test]
async fn test_acl_add_slacl_to_unrestricted_allowed(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;
    let changes = BTreeMap::from([(
        MPath::try_from("public/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT), None),
    )]);

    run_single_changeset_test(
        fb,
        "unauthorized_user",
        repo,
        &[("public/file.txt", "hello")],
        changes,
        "add .slacl to unrestricted dir",
        ExpectedOutcome::Allowed,
    )
    .await
}

/// A maintainer (who has "maintainers" permission on the ACL) CAN modify
/// .slacl in a restricted directory.
#[mononoke::fbinit_test]
async fn test_acl_modify_slacl_with_acl_allowed(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;
    let changes = BTreeMap::from([(
        MPath::try_from("secret/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT_2), None),
    )]);

    run_single_changeset_test(
        fb,
        "maintainer_user",
        repo,
        &[("secret/.slacl", SLACL_CONTENT)],
        changes,
        "modify .slacl (authorized)",
        ExpectedOutcome::Allowed,
    )
    .await
}

/// A user with only "read" permission (not "maintainers") should NOT be able
/// to modify .slacl files. Only maintainers of the REPO_REGION ACL should
/// be allowed to change code tenting restrictions.
///
/// The current `authorized_user` has "read" access and the test above shows
/// they CAN modify .slacl. This test uses the same user to demonstrate that
/// "read" alone should not be sufficient.
#[mononoke::fbinit_test]
async fn test_acl_read_only_user_cannot_modify_slacl(fb: FacebookInit) -> Result<()> {
    // authorized_user has "read" but not "maintainers" in the test ACL provider
    let repo = build_secret_restricted_repo(fb).await?;
    let changes = BTreeMap::from([(
        MPath::try_from("secret/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT_2), None),
    )]);

    run_single_changeset_test(
        fb,
        "authorized_user",
        repo,
        &[("secret/.slacl", SLACL_CONTENT)],
        changes,
        "modify .slacl (read-only user)",
        ExpectedOutcome::Blocked("does not have maintainer access"),
    )
    .await
}

/// A read-only user (has "read" but not "maintainers") CAN modify non-.slacl
/// files in a restricted directory. The ACL file validation only protects
/// .slacl files — regular file modifications are not blocked.
#[mononoke::fbinit_test]
async fn test_acl_read_only_user_can_modify_non_slacl_file(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;
    let changes = BTreeMap::from([(
        MPath::try_from("secret/data.txt")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("modified data"), None),
    )]);

    run_single_changeset_test(
        fb,
        "authorized_user",
        repo,
        &[
            ("secret/.slacl", SLACL_CONTENT),
            ("secret/data.txt", "original data"),
        ],
        changes,
        "modify non-.slacl file (read-only user)",
        ExpectedOutcome::Allowed,
    )
    .await
}

// ---------------------------------------------------------------------------
// Stack tests — should be BLOCKED by validate_acl_file_changes
// ---------------------------------------------------------------------------

/// Deleting .slacl in the second commit of a stack is blocked.
/// Restriction state must be tracked through the stack.
#[mononoke::fbinit_test]
async fn test_acl_bypass_stack_delete_slacl_in_c2(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;

    let c1_changes = BTreeMap::from([(
        MPath::try_from("unrelated.txt")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("hello"), None),
    )]);
    let c2_changes = BTreeMap::from([(MPath::try_from("secret/.slacl")?, CreateChange::Deletion)]);

    run_stack_changeset_test(
        fb,
        "unauthorized_user",
        repo,
        &[("secret/.slacl", SLACL_CONTENT)],
        vec![c1_changes, c2_changes],
        vec!["stack C1: unrelated change", "stack C2: delete .slacl"],
        ExpectedOutcome::Blocked("does not have maintainer access"),
    )
    .await
}

/// Deleting .slacl and re-adding it with a different ACL is blocked.
/// C1's deletion should be blocked even though C2 re-adds a .slacl —
/// the attacker is replacing the original ACL with one they control.
#[mononoke::fbinit_test]
async fn test_acl_bypass_delete_slacl_via_stack_with_add(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;

    let c1_changes = BTreeMap::from([(MPath::try_from("secret/.slacl")?, CreateChange::Deletion)]);
    let c2_changes = BTreeMap::from([(
        MPath::try_from("secret/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT_2), None),
    )]);

    run_stack_changeset_test(
        fb,
        "unauthorized_user",
        repo,
        &[("secret/.slacl", SLACL_CONTENT)],
        vec![c1_changes, c2_changes],
        vec![
            "stack C1: delete .slacl",
            "stack C2: re-add .slacl with different ACL",
        ],
        ExpectedOutcome::Blocked("does not have maintainer access"),
    )
    .await
}

// ---------------------------------------------------------------------------
// Merge tests — restriction must be enforced against ALL parents, not just
// the first.
// ---------------------------------------------------------------------------

/// A merge changeset that modifies `secret/.slacl` is blocked when ANY parent
/// restricts the directory and the caller lacks maintainer access — even when
/// the restricting `.slacl` lives only on the second parent.
#[mononoke::fbinit_test]
async fn test_acl_bypass_merge_modify_slacl_restricted_in_one_parent(
    fb: FacebookInit,
) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;
    let changes = BTreeMap::from([(
        MPath::try_from("secret/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT_2), None),
    )]);

    // First parent is an unrelated branch with no restricted content; the
    // second parent carries the restricted `secret/.slacl`. A naive
    // first-parent-only check would miss the restriction.
    run_merge_changeset_test(
        fb,
        "authorized_user",
        repo,
        &[
            &[("public/file.txt", "unrelated branch")],
            &[("secret/.slacl", SLACL_CONTENT)],
        ],
        changes,
        "merge modify .slacl (read-only user)",
        ExpectedOutcome::Blocked("does not have maintainer access"),
    )
    .await
}

/// The merge companion: a maintainer of the restricting ACL CAN modify
/// `secret/.slacl` in a merge changeset.
#[mononoke::fbinit_test]
async fn test_acl_merge_modify_slacl_with_maintainer_allowed(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;
    let changes = BTreeMap::from([(
        MPath::try_from("secret/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT_2), None),
    )]);

    run_merge_changeset_test(
        fb,
        "maintainer_user",
        repo,
        &[
            &[("public/file.txt", "unrelated branch")],
            &[("secret/.slacl", SLACL_CONTENT)],
        ],
        changes,
        "merge modify .slacl (maintainer)",
        ExpectedOutcome::Allowed,
    )
    .await
}

// ---------------------------------------------------------------------------
// Admin bypass group tests — members of `admin_bypass_group` may change any
// .slacl, even without maintaining the region's ACL. Non-members are covered
// by the bypass tests above, which run with the same group configured.
// ---------------------------------------------------------------------------

/// What it tests: an admin bypass group member who does not maintain the
/// region ACL deletes an existing `.slacl` in a restricted directory.
/// Expected: allowed — bypass group membership replaces maintainer access.
#[mononoke::fbinit_test]
async fn test_admin_bypass_delete_slacl_in_restricted_dir(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;
    let changes = BTreeMap::from([(MPath::try_from("secret/.slacl")?, CreateChange::Deletion)]);

    run_single_changeset_test(
        fb,
        "admin_bypass_user",
        repo,
        &[
            ("secret/.slacl", SLACL_CONTENT),
            ("secret/data.txt", "sensitive data"),
        ],
        changes,
        "delete .slacl (admin bypass)",
        ExpectedOutcome::Allowed,
    )
    .await
}

/// What it tests: an admin bypass group member who does not maintain the
/// region ACL rewrites an existing `.slacl` in a restricted directory.
/// Expected: allowed — bypass group membership replaces maintainer access.
#[mononoke::fbinit_test]
async fn test_admin_bypass_modify_slacl_in_restricted_dir(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;
    let changes = BTreeMap::from([(
        MPath::try_from("secret/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT_2), None),
    )]);

    run_single_changeset_test(
        fb,
        "admin_bypass_user",
        repo,
        &[("secret/.slacl", SLACL_CONTENT)],
        changes,
        "modify .slacl (admin bypass)",
        ExpectedOutcome::Allowed,
    )
    .await
}

/// What it tests: an admin bypass group member who does not maintain the
/// parent ACL adds a nested `.slacl` under a restricted directory.
/// Expected: allowed — the inherited restriction is bypassed too.
#[mononoke::fbinit_test]
async fn test_admin_bypass_add_nested_slacl_under_restricted(fb: FacebookInit) -> Result<()> {
    let repo = build_restricted_dir_repo(fb).await?;
    let changes = BTreeMap::from([(
        MPath::try_from("restricted/subdir/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT_2), None),
    )]);

    run_single_changeset_test(
        fb,
        "admin_bypass_user",
        repo,
        &[
            ("restricted/.slacl", SLACL_CONTENT),
            ("restricted/subdir/file.txt", "data"),
        ],
        changes,
        "add nested .slacl (admin bypass)",
        ExpectedOutcome::Allowed,
    )
    .await
}

/// What it tests: an admin bypass group member creates a stack whose second
/// changeset deletes `secret/.slacl`.
/// Expected: allowed — stack validation applies the same bypass.
#[mononoke::fbinit_test]
async fn test_admin_bypass_stack_delete_slacl_in_c2(fb: FacebookInit) -> Result<()> {
    let repo = build_secret_restricted_repo(fb).await?;

    let c1_changes = BTreeMap::from([(
        MPath::try_from("unrelated.txt")?,
        CreateChange::Tracked(CreateChangeFile::new_regular("hello"), None),
    )]);
    let c2_changes = BTreeMap::from([(MPath::try_from("secret/.slacl")?, CreateChange::Deletion)]);

    run_stack_changeset_test(
        fb,
        "admin_bypass_user",
        repo,
        &[("secret/.slacl", SLACL_CONTENT)],
        vec![c1_changes, c2_changes],
        vec![
            "stack C1: unrelated change",
            "stack C2: delete .slacl (admin bypass)",
        ],
        ExpectedOutcome::Allowed,
    )
    .await
}

/// What it tests: an admin bypass group member creates a stack that adds a
/// nested `.slacl` under a restricted directory and modifies it again in the
/// next changeset.
/// Expected: allowed — admin bypass group members bypass every `.slacl`
/// restriction, including the one-change-per-`.slacl`-per-stack rule.
#[mononoke::fbinit_test]
async fn test_admin_bypass_stack_add_then_modify_nested_slacl(fb: FacebookInit) -> Result<()> {
    let repo = build_restricted_dir_repo(fb).await?;

    let c1_changes = BTreeMap::from([(
        MPath::try_from("restricted/subdir/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT), None),
    )]);
    let c2_changes = BTreeMap::from([(
        MPath::try_from("restricted/subdir/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT_2), None),
    )]);

    run_stack_changeset_test(
        fb,
        "admin_bypass_user",
        repo,
        &[("restricted/.slacl", SLACL_CONTENT)],
        vec![c1_changes, c2_changes],
        vec![
            "stack C1: add nested .slacl (admin bypass)",
            "stack C2: modify nested .slacl (admin bypass)",
        ],
        ExpectedOutcome::Allowed,
    )
    .await
}

/// What it tests: a maintainer of the restricting ACL creates a stack that
/// adds a nested `.slacl` under a restricted directory and modifies it again
/// in the next changeset.
/// Expected: blocked by the one-change-per-`.slacl`-per-stack rule — only
/// admin bypass group members are exempt from it, not maintainers.
#[mononoke::fbinit_test]
async fn test_maintainer_stack_add_then_modify_nested_slacl_blocked(
    fb: FacebookInit,
) -> Result<()> {
    let repo = build_restricted_dir_repo(fb).await?;

    let c1_changes = BTreeMap::from([(
        MPath::try_from("restricted/subdir/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT), None),
    )]);
    let c2_changes = BTreeMap::from([(
        MPath::try_from("restricted/subdir/.slacl")?,
        CreateChange::Tracked(CreateChangeFile::new_regular(SLACL_CONTENT_2), None),
    )]);

    run_stack_changeset_test(
        fb,
        "maintainer_user",
        repo,
        &[("restricted/.slacl", SLACL_CONTENT)],
        vec![c1_changes, c2_changes],
        vec![
            "stack C1: add nested .slacl (maintainer)",
            "stack C2: modify nested .slacl (maintainer)",
        ],
        ExpectedOutcome::Blocked("already modified by an earlier changeset in this stack"),
    )
    .await
}

// ---- helpers ----

/// Standard `CreateChangesetChecks` that validates all conditions.
fn test_checks() -> CreateChangesetChecks {
    CreateChangesetChecks::check()
}

/// Build a `CreateInfo` struct suitable for tests.
fn test_create_info(message: &str) -> Result<CreateInfo> {
    let author_date = FixedOffset::east_opt(0)
        .ok_or_else(|| anyhow::anyhow!("FixedOffset::east_opt(0) failed"))?
        .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
        .single()
        .ok_or_else(|| anyhow::anyhow!("ambiguous or invalid datetime"))?;
    Ok(CreateInfo {
        author: "Test User <test@example.com>".to_string(),
        author_date,
        committer: None,
        committer_date: None,
        message: message.to_string(),
        extra: BTreeMap::new(),
        git_extra_headers: None,
    })
}

/// Build an `InternalAclProvider` with these callers:
/// - `maintainer_user`: has both "read" and "maintainers" — can modify .slacl
/// - `authorized_user`: has only "read" — can read restricted content but
///   cannot modify .slacl files (only maintainers can)
/// - `unauthorized_user`: has neither — cannot read or modify
/// - `admin_bypass_user`: has neither, but is in [`ADMIN_BYPASS_GROUP`] — can
///   modify .slacl through the admin bypass
fn test_acl_provider() -> Arc<dyn permission_checker::AclProvider> {
    let maintainer = MononokeIdentity::from_legacy_type_data("USER", "maintainer_user");
    let reader = MononokeIdentity::from_legacy_type_data("USER", "authorized_user");
    let admin = MononokeIdentity::from_legacy_type_data("USER", "admin_bypass_user");

    let acls = Acls {
        repos: HashMap::new(),
        repo_regions: hashmap! {
            RESTRICTED_ACL.to_string() => Arc::new(Acl {
                actions: hashmap! {
                    "read".to_string() => MononokeIdentitySet::from([
                        maintainer.clone(), reader,
                    ]),
                    "maintainers".to_string() => MononokeIdentitySet::from([
                        maintainer,
                    ]),
                },
            }),
        },
        tiers: HashMap::new(),
        workspaces: HashMap::new(),
        groups: hashmap! {
            ADMIN_BYPASS_GROUP.to_string() => Arc::new(MononokeIdentitySet::from([admin])),
        },
    };

    InternalAclProvider::new(acls)
}

/// Build a `CoreContext` whose caller identity is `USER:<username>`.
async fn test_ctx_with_identity(fb: FacebookInit, username: &str) -> Result<context::CoreContext> {
    let identity = MononokeIdentity::from_legacy_type_data("USER", username);
    let identities = BTreeSet::from([identity]);
    let metadata = Arc::new(
        Metadata::new(
            Some(&"acl_file_protection_test".to_string()),
            identities,
            false,
            false,
            None,
            None,
        )
        .await,
    );
    let session = SessionContainer::builder(fb).metadata(metadata).build();
    Ok(context::CoreContext::test_mock_session(session))
}

/// Build a repo with config-based restricted paths, [`ADMIN_BYPASS_GROUP`] as
/// its admin bypass group, and the callers from [`test_acl_provider`].
///
/// The `restricted_dirs` list maps directory paths (e.g. `"secret"`) to the
/// `MononokeIdentity` for the repo region ACL that protects them.
async fn build_restricted_repo(
    fb: FacebookInit,
    restricted_dirs: Vec<(&str, &str)>,
) -> Result<Repo> {
    let repo_id = RepositoryId::new(0);

    let config = metaconfig_types::RestrictedPathsConfig {
        path_restriction_metadata: super::build_path_restriction_metadata(restricted_dirs)?,
        admin_bypass_group: Some(MononokeIdentity::from_legacy_type_data(
            "GROUP",
            ADMIN_BYPASS_GROUP,
        )),
        ..Default::default()
    };

    let manifest_id_store = Arc::new(
        SqlRestrictedPathsManifestIdStoreBuilder::with_sqlite_in_memory()?.with_repo_id(repo_id),
    );

    let config_based = Arc::new(RestrictedPathsConfigBased::new(
        config,
        manifest_id_store,
        None,
    ));

    // Build a first repo to get ArcRepoDerivedData (provides derivation config).
    let first_repo: Repo = test_repo_factory::TestRepoFactory::new(fb)?.build().await?;
    let repo_derived_data =
        repo_derived_data::RepoDerivedDataArc::repo_derived_data_arc(&first_repo);

    let acl_provider = test_acl_provider();
    let scuba = MononokeScubaSampleBuilder::with_discard();

    let restricted_paths = Arc::new(RestrictedPaths::new(
        "repo".to_string(),
        config_based,
        acl_provider,
        scuba,
        repo_derived_data,
    )?);

    // Rebuild with the custom restricted_paths so create_changeset picks them up.
    let repo: Repo = test_repo_factory::TestRepoFactory::new(fb)?
        .with_restricted_paths(restricted_paths)
        .build()
        .await?;

    Ok(repo)
}

/// Convenience: build a repo where `"secret"` is restricted by the test ACL.
async fn build_secret_restricted_repo(fb: FacebookInit) -> Result<Repo> {
    build_restricted_repo(fb, vec![("secret", RESTRICTED_IDENTITY)]).await
}

/// Convenience: build a repo where `"restricted"` is restricted by the test ACL.
async fn build_restricted_dir_repo(fb: FacebookInit) -> Result<Repo> {
    build_restricted_repo(fb, vec![("restricted", RESTRICTED_IDENTITY)]).await
}

/// Shared test driver for single-commit ACL bypass tests.
///
/// Handles the full setup-through-assertion lifecycle so individual tests
/// only specify what varies.
async fn run_single_changeset_test(
    fb: FacebookInit,
    username: &str,
    repo: Repo,
    parent_files: &[(&str, &str)],
    changes: BTreeMap<MPath, CreateChange>,
    changeset_msg: &str,
    expected: ExpectedOutcome,
) -> Result<()> {
    let ctx = test_ctx_with_identity(fb, username).await?;

    let mut commit_ctx = CreateCommitContext::new_root(&ctx, &repo);
    for &(path, content) in parent_files {
        commit_ctx = commit_ctx.add_file(path, content);
    }
    let parent = commit_ctx.commit().await?;

    let mononoke = Mononoke::new_test(vec![("test".to_string(), repo)]).await?;
    let repo_ctx = mononoke
        .repo(ctx, "test")
        .await?
        .ok_or_else(|| anyhow::anyhow!("repo 'test' not found"))?
        .build()
        .await?;

    let result = repo_ctx
        .create_changeset(
            vec![parent],
            test_create_info(changeset_msg)?,
            changes,
            None,
            test_checks(),
        )
        .await;

    assert_outcome("create_changeset", result.map(|_| ()), expected)
}

/// Shared test driver for stack-commit ACL bypass tests.
async fn run_stack_changeset_test(
    fb: FacebookInit,
    username: &str,
    repo: Repo,
    parent_files: &[(&str, &str)],
    stack_changes: Vec<BTreeMap<MPath, CreateChange>>,
    stack_messages: Vec<&str>,
    expected: ExpectedOutcome,
) -> Result<()> {
    let ctx = test_ctx_with_identity(fb, username).await?;

    let mut commit_ctx = CreateCommitContext::new_root(&ctx, &repo);
    for &(path, content) in parent_files {
        commit_ctx = commit_ctx.add_file(path, content);
    }
    let parent = commit_ctx.commit().await?;

    let mononoke = Mononoke::new_test(vec![("test".to_string(), repo)]).await?;
    let repo_ctx = mononoke
        .repo(ctx, "test")
        .await?
        .ok_or_else(|| anyhow::anyhow!("repo 'test' not found"))?
        .build()
        .await?;

    let infos = stack_messages
        .iter()
        .map(|msg| test_create_info(msg))
        .collect::<Result<Vec<_>>>()?;

    let result = repo_ctx
        .create_changeset_stack(vec![parent], infos, stack_changes, None, test_checks())
        .await;

    assert_outcome("create_changeset_stack", result.map(|_| ()), expected)
}

/// Shared test driver for merge-changeset ACL bypass tests.
///
/// Builds two independent root parents from `parent_files_per_parent` (one
/// file-set per parent), then creates a single merge changeset with both
/// parents applying `changes`. Used to prove that `.slacl` validation blocks
/// when ANY parent restricts the touched directory and the caller lacks
/// maintainer access — not just the first parent.
async fn run_merge_changeset_test(
    fb: FacebookInit,
    username: &str,
    repo: Repo,
    parent_files_per_parent: &[&[(&str, &str)]],
    changes: BTreeMap<MPath, CreateChange>,
    changeset_msg: &str,
    expected: ExpectedOutcome,
) -> Result<()> {
    let ctx = test_ctx_with_identity(fb, username).await?;

    let mut parents = Vec::new();
    for parent_files in parent_files_per_parent {
        let mut commit_ctx = CreateCommitContext::new_root(&ctx, &repo);
        for &(path, content) in *parent_files {
            commit_ctx = commit_ctx.add_file(path, content);
        }
        parents.push(commit_ctx.commit().await?);
    }

    let mononoke = Mononoke::new_test(vec![("test".to_string(), repo)]).await?;
    let repo_ctx = mononoke
        .repo(ctx, "test")
        .await?
        .ok_or_else(|| anyhow::anyhow!("repo 'test' not found"))?
        .build()
        .await?;

    let result = repo_ctx
        .create_changeset(
            parents,
            test_create_info(changeset_msg)?,
            changes,
            None,
            test_checks(),
        )
        .await;

    assert_outcome("create_changeset", result.map(|_| ()), expected)
}

/// Assert that a `create_changeset`-family call produced the expected outcome.
/// For `Blocked`, the error message must contain the given substring; for
/// `Allowed`, the call must have succeeded. `op` names the operation for
/// failure messages.
fn assert_outcome(
    op: &str,
    result: std::result::Result<(), crate::errors::MononokeError>,
    expected: ExpectedOutcome,
) -> Result<()> {
    match expected {
        ExpectedOutcome::Blocked(expected_substr) => {
            let err = result
                .err()
                .ok_or_else(|| anyhow::anyhow!("expected {op} to fail, but it succeeded"))?;
            let err_msg = format!("{err:#}");
            assert!(
                err_msg.contains(expected_substr),
                "Error should contain '{expected_substr}', got: {err_msg}",
            );
        }
        ExpectedOutcome::Allowed => {
            result
                .map_err(|e| anyhow::anyhow!("expected {op} to succeed, but it failed: {e:#}"))?;
        }
    }

    Ok(())
}
