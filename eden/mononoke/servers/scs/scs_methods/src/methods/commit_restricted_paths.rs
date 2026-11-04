/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use anyhow::Context;
use anyhow::Result;
use futures::StreamExt;
use futures::stream::BoxStream;
use mononoke_api::MononokeError;
use mononoke_api::PathAccessInfo;
use mononoke_api::Repo;
use mononoke_api::changeset::ChangesetContext;
use mononoke_api::restricted_paths::ChangedPathRestrictionInfo;
use mononoke_types::NonRootMPath;
use mononoke_types::path::MPath;
use source_control as thrift;

const COMPARISON_KNOB: &str = "scm/mononoke:enable_scs_restricted_paths_changes_comparison";

pub(crate) fn use_new_comparison(
    params: &thrift::CommitRestrictedPathsChangesParams,
) -> Result<bool, scs_errors::ServiceError> {
    // OSS has no runtime knob registry; its comparison remains enabled.
    let enabled = cfg!(not(fbcode_build)) || justknobs::eval(COMPARISON_KNOB, None, None);
    if !enabled
        && (params.format.is_some()
            || params.ordered_params.is_some()
            || params.other_commit_id.is_some())
    {
        return Err(scs_errors::not_implemented(
            "restriction comparison is disabled; only original requests are supported".to_owned(),
        )
        .into());
    }
    Ok(enabled)
}

/// The rollback response checks only paths recorded in the target Bonsai changeset.
pub(crate) async fn bonsai_changes_response(
    changeset: &ChangesetContext<Repo>,
) -> Result<thrift::CommitRestrictedPathsChangesResponse, scs_errors::ServiceError> {
    let bonsai = changeset.bonsai_changeset().await?;
    let changed_paths: BTreeSet<String> = bonsai
        .file_changes_map()
        .keys()
        .map(|path| path.to_string())
        .collect();
    let access = restricted_paths_access_impl(changeset, changed_paths.clone(), true).await?;
    Ok(thrift::CommitRestrictedPathsChangesResponse {
        are_restricted: access.are_restricted,
        has_access: access.has_access,
        restriction_roots: access.restriction_roots,
        authorized_paths: access.authorized_paths,
        changed_paths: changed_paths.into_iter().collect(),
        ..Default::default()
    })
}

pub(crate) async fn restricted_paths_access_impl(
    cs_ctx: &ChangesetContext<Repo>,
    paths: BTreeSet<String>,
    check_permissions: bool,
) -> Result<thrift::CommitRestrictedPathsAccessResponse, scs_errors::ServiceError> {
    // If no restricted paths configured, return empty response
    if !cs_ctx.may_have_restricted_paths() {
        return Ok(thrift::CommitRestrictedPathsAccessResponse {
            are_restricted: thrift::PathCoverage::NONE,
            has_access: thrift::PathCoverage::ALL,
            restriction_roots: BTreeMap::new(),
            authorized_paths: if check_permissions {
                paths.into_iter().collect()
            } else {
                vec![]
            },
            ..Default::default()
        });
    }

    let non_root_paths: Vec<NonRootMPath> = paths
        .iter()
        .map(|p| {
            NonRootMPath::try_from(p.as_str())
                .with_context(|| format!("Casting path {p} to NonRootMPath"))
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map_err(|err| MononokeError::InternalError(err.into()))?;

    let restriction_results = cs_ctx
        .paths_restriction_info(non_root_paths, check_permissions)
        .await?;

    struct AggregatedResult {
        is_restricted: Vec<bool>,
        has_access: Vec<bool>,
        authorized_paths: Vec<String>,
        restriction_roots: BTreeMap<String, Vec<thrift::PathRestrictionRoot>>,
    }

    let result = restriction_results.iter().fold(
        AggregatedResult {
            is_restricted: Vec::with_capacity(restriction_results.len()),
            has_access: Vec::with_capacity(restriction_results.len()),
            authorized_paths: Vec::new(),
            restriction_roots: BTreeMap::new(),
        },
        |mut acc, (path, infos)| {
            let restricted = !infos.is_empty();
            acc.is_restricted.push(restricted);

            let access = if restricted {
                infos.iter().all(|info| info.has_access.unwrap_or(false))
            } else {
                true
            };
            acc.has_access.push(access);

            if check_permissions && access {
                acc.authorized_paths.push(path.to_string());
            }

            if restricted {
                acc.restriction_roots.insert(
                    path.to_string(),
                    infos
                        .iter()
                        .map(|info| thrift::PathRestrictionRoot {
                            path: info.restriction_root().to_string(),
                            acls: vec![info.repo_region_acl().to_string()],
                            permission_request_group: Some(
                                info.permission_request_group().id_data().to_string(),
                            ),
                            ..Default::default()
                        })
                        .collect(),
                );
            }

            acc
        },
    );

    let authorized_paths = {
        let mut paths = result.authorized_paths;
        paths.sort();
        paths
    };

    Ok(thrift::CommitRestrictedPathsAccessResponse {
        are_restricted: compute_path_coverage(result.is_restricted),
        has_access: compute_path_coverage(result.has_access),
        restriction_roots: result.restriction_roots,
        authorized_paths,
        ..Default::default()
    })
}

pub(crate) fn changes_restriction_results_into_response(
    results: Vec<(NonRootMPath, ChangedPathRestrictionInfo)>,
) -> thrift::CommitRestrictedPathsChangesResponse {
    let mut response = comparison_common_response(&results);
    response.restriction_roots = results
        .iter()
        .filter_map(|(path, info)| {
            let roots: BTreeSet<_> = info
                .restriction_info()
                .map(path_restriction_root_into_response)
                .collect();
            (!roots.is_empty()).then(|| (path.to_string(), roots.into_iter().collect()))
        })
        .collect();
    response
}

/// Unchecked side results are not evidence of access.
fn has_access_to_content(info: &ChangedPathRestrictionInfo) -> bool {
    info.content_sides()
        .all(|side| side.has_access == Some(true))
}

fn comparison_common_response(
    results: &[(NonRootMPath, ChangedPathRestrictionInfo)],
) -> thrift::CommitRestrictedPathsChangesResponse {
    let mut is_restricted = Vec::with_capacity(results.len());
    let mut has_access = Vec::with_capacity(results.len());
    let mut changed_paths = Vec::with_capacity(results.len());
    let mut authorized_paths = Vec::new();
    for (path, info) in results {
        let path = path.to_string();
        is_restricted.push(info.restriction_info().next().is_some());
        let access = has_access_to_content(info);
        has_access.push(access);
        if access {
            authorized_paths.push(path.clone());
        }
        changed_paths.push(path);
    }
    thrift::CommitRestrictedPathsChangesResponse {
        are_restricted: compute_path_coverage(is_restricted),
        has_access: if changed_paths.is_empty() {
            thrift::PathCoverage::ALL
        } else {
            compute_path_coverage(has_access)
        },
        authorized_paths,
        changed_paths,
        checked_both_sides: Some(true),
        ..Default::default()
    }
}

fn path_restriction_root_into_response(info: &PathAccessInfo) -> thrift::PathRestrictionRoot {
    thrift::PathRestrictionRoot {
        path: info.restriction_root().to_string(),
        acls: vec![info.repo_region_acl().to_string()],
        permission_request_group: Some(info.permission_request_group().id_data().to_string()),
        ..Default::default()
    }
}

/// Find all restriction roots that are nested under the given filter paths.
/// Returns a stream of restriction roots where the restriction root path
/// starts with one of the filter paths (i.e., the root is under the filter).
///
/// If filter_roots is empty, returns all restriction roots in the repository.
pub(crate) async fn find_nested_restricted_roots(
    cs_ctx: &ChangesetContext<Repo>,
    filter_roots: BTreeSet<String>,
    check_permissions: bool,
    return_only_accessible: bool,
) -> Result<
    BoxStream<
        'static,
        Result<thrift::CommitFindRestrictedPathsStreamItem, scs_errors::ServiceError>,
    >,
    scs_errors::ServiceError,
> {
    let roots: Vec<MPath> = if filter_roots.is_empty() {
        vec![MPath::ROOT]
    } else {
        filter_roots
            .iter()
            .map(|r| {
                MPath::try_from(r.as_str())
                    .map_err(|e| MononokeError::InvalidRequest(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    let descendants = cs_ctx
        .find_restricted_descendants(roots, check_permissions || return_only_accessible)
        .await?;

    Ok((async_stream::stream! {
        for info in descendants {
            if return_only_accessible && info.has_access != Some(true) {
                continue;
            }

            yield Ok(thrift::CommitFindRestrictedPathsStreamItem {
                path: info.restriction_root().to_string(),
                acls: vec![info.repo_region_acl().to_string()],
                has_access: if check_permissions { info.has_access } else { None },
                permission_request_group: Some(
                    info.permission_request_group().id_data().to_string(),
                ),
                ..Default::default()
            });
        }
    })
    .boxed())
}

/// Check if the mock API should be used for this repo.
pub(crate) fn use_mock_api(repo_name: &str) -> Result<bool, scs_errors::ServiceError> {
    Ok(justknobs::eval(
        "scm/mononoke:scs_restricted_paths_use_mock_api",
        None,
        Some(repo_name),
    ))
}

pub(crate) fn compute_path_coverage(
    values: impl IntoIterator<Item = bool>,
) -> thrift::PathCoverage {
    let (has_true, has_false) = values.into_iter().fold((false, false), |(ht, hf), value| {
        (ht || value, hf || !value)
    });

    match (has_true, has_false) {
        (true, true) => thrift::PathCoverage::SOME,
        (true, false) => thrift::PathCoverage::ALL,
        (false, _) => thrift::PathCoverage::NONE,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use anyhow::Context;
    use async_trait::async_trait;
    use context::CoreContext;
    use fbinit::FacebookInit;
    use futures::TryStreamExt;
    use metaconfig_types::AclManifestMode;
    use metaconfig_types::PathRestrictionMetadata;
    use metaconfig_types::RestrictedPathsConfig;
    use metaconfig_types::RestrictedPathsManifestIdStoreConfig;
    use mononoke_api::ChangedPathsRestrictionOptions;
    use mononoke_api::Repo;
    use mononoke_api::RepoContext;
    use mononoke_api::changeset::ChangesetContext;
    use mononoke_macros::mononoke;
    use mononoke_types::RepositoryId;
    use permission_checker::AclProvider;
    use permission_checker::AlwaysMember;
    use permission_checker::BoxMembershipChecker;
    use permission_checker::BoxPermissionChecker;
    use permission_checker::MononokeIdentity;
    use permission_checker::MononokeIdentitySet;
    use permission_checker::NeverMember;
    use permission_checker::PermissionCheckerBuilder;
    use permission_checker::dummy::DummyAclProvider;
    use repo_derived_data::RepoDerivedDataArc;
    use restricted_paths::ArcRestrictedPaths;
    use restricted_paths::RestrictedPaths;
    use restricted_paths::RestrictedPathsConfigBased;
    use restricted_paths::SqlRestrictedPathsManifestIdStoreBuilder;
    use scuba_ext::MononokeScubaSampleBuilder;
    use sql_construct::SqlConstruct;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;

    use super::*;

    mod content_sides;
    mod lookup_modes;
    #[cfg(fbcode_build)]
    mod rollback;

    #[cfg(not(fbcode_build))]
    #[mononoke::test]
    fn oss_comparison_stays_enabled_without_a_runtime_knob() {
        assert!(
            use_new_comparison(&thrift::CommitRestrictedPathsChangesParams::default())
                .expect("OSS uses the new comparison")
        );
    }

    struct TestAclProvider {
        allow_repo_region_reads: bool,
        allowed_repo_region: Option<String>,
        region_lookups: Option<Arc<AtomicUsize>>,
    }

    impl TestAclProvider {
        fn new(allow_repo_region_reads: bool) -> Arc<dyn AclProvider> {
            Arc::new(Self {
                allow_repo_region_reads,
                allowed_repo_region: None,
                region_lookups: None,
            })
        }

        fn counting(lookups: Arc<AtomicUsize>) -> Arc<dyn AclProvider> {
            Arc::new(Self {
                allow_repo_region_reads: true,
                allowed_repo_region: None,
                region_lookups: Some(lookups),
            })
        }
    }

    #[async_trait]
    impl AclProvider for TestAclProvider {
        async fn repo_acl(&self, _name: &str) -> Result<BoxPermissionChecker> {
            Ok(PermissionCheckerBuilder::new().allow_all().build())
        }

        async fn repo_region_acl(&self, name: &str) -> Result<BoxPermissionChecker> {
            if let Some(lookups) = &self.region_lookups {
                lookups.fetch_add(1, Ordering::Relaxed);
            }
            let builder = PermissionCheckerBuilder::new();
            Ok(
                if self.allow_repo_region_reads || self.allowed_repo_region.as_deref() == Some(name)
                {
                    builder.allow_all().build()
                } else {
                    builder.build()
                },
            )
        }

        async fn tier_acl(&self, _name: &str) -> Result<BoxPermissionChecker> {
            Ok(PermissionCheckerBuilder::new().allow_all().build())
        }

        async fn commitcloud_workspace_acl(
            &self,
            _name: &str,
            _create_with_owner: &Option<MononokeIdentitySet>,
        ) -> Result<Option<BoxPermissionChecker>> {
            Ok(Some(PermissionCheckerBuilder::new().allow_all().build()))
        }

        async fn group(&self, _name: &str) -> Result<BoxMembershipChecker> {
            Ok(NeverMember::new())
        }

        async fn admin_group(&self) -> Result<BoxMembershipChecker> {
            Ok(NeverMember::new())
        }

        async fn reviewers_group(&self) -> Result<BoxMembershipChecker> {
            Ok(AlwaysMember::new())
        }
    }

    async fn create_test_restricted_paths_with_acl_provider(
        fb: FacebookInit,
        path_acls: Vec<(&str, &str)>, // (path, "TYPE:acl_name")
        acl_provider: Arc<dyn AclProvider>,
    ) -> Result<ArcRestrictedPaths> {
        create_test_restricted_paths_with_mode(
            fb,
            path_acls,
            acl_provider,
            AclManifestMode::Disabled,
        )
        .await
    }

    async fn create_test_restricted_paths_with_mode(
        fb: FacebookInit,
        path_acls: Vec<(&str, &str)>,
        acl_provider: Arc<dyn AclProvider>,
        acl_manifest_mode: AclManifestMode,
    ) -> Result<ArcRestrictedPaths> {
        let repo_id = RepositoryId::new(0);
        let path_restriction_metadata = build_path_restriction_metadata(path_acls)?;

        let config = RestrictedPathsConfig {
            path_restriction_metadata,
            manifest_id_store_config: RestrictedPathsManifestIdStoreConfig {
                use_manifest_id_cache: false,
                cache_update_interval_ms: 100,
                ..Default::default()
            },
            acl_manifest_mode,
            ..Default::default()
        };

        let manifest_id_store = Arc::new(
            SqlRestrictedPathsManifestIdStoreBuilder::with_sqlite_in_memory()?
                .with_repo_id(repo_id),
        );

        let scuba = MononokeScubaSampleBuilder::with_discard();

        let config_based = Arc::new(RestrictedPathsConfigBased::new(
            config,
            manifest_id_store,
            None,
        ));

        let test_repo: Repo = TestRepoFactory::new(fb)?.build().await?;
        let repo_derived_data = test_repo.repo_derived_data_arc();

        Ok(Arc::new(RestrictedPaths::new(
            config_based,
            acl_provider,
            scuba,
            repo_derived_data,
        )?))
    }

    async fn create_both_mode_test_changeset(
        fb: FacebookInit,
        config_path_acls: Vec<(&str, &str)>,
        acl_manifest_path_acls: Vec<(&str, &str)>,
        file_paths: Vec<&str>,
    ) -> Result<ChangesetContext<Repo>> {
        let ctx = CoreContext::test_mock(fb);
        let repo_id = RepositoryId::new(0);
        let config = RestrictedPathsConfig {
            path_restriction_metadata: build_path_restriction_metadata(config_path_acls)?,
            manifest_id_store_config: RestrictedPathsManifestIdStoreConfig {
                use_manifest_id_cache: false,
                cache_update_interval_ms: 100,
                ..Default::default()
            },
            acl_manifest_mode: AclManifestMode::Both,
            ..Default::default()
        };
        let manifest_id_store = Arc::new(
            SqlRestrictedPathsManifestIdStoreBuilder::with_sqlite_in_memory()?
                .with_repo_id(repo_id),
        );
        let mut factory = TestRepoFactory::new(fb)?;
        let repo: Repo = factory.build().await?;
        let restricted = Arc::new(RestrictedPaths::new(
            Arc::new(RestrictedPathsConfigBased::new(
                config,
                manifest_id_store,
                None,
            )),
            DummyAclProvider::new(fb)?,
            MononokeScubaSampleBuilder::with_discard(),
            repo.repo_derived_data_arc(),
        )?);
        let repo: Repo = factory.with_restricted_paths(restricted).build().await?;
        let mut commit = CreateCommitContext::new_root(&ctx, &repo);
        for path in file_paths {
            commit = commit.add_file(path, "content");
        }
        for (root, acl) in acl_manifest_path_acls {
            let path = format!("{root}/.slacl");
            commit = commit.add_file(path.as_str(), format!("repo_region_acl = \"{acl}\"\n"));
        }
        let id = commit.commit().await?;
        let repo = RepoContext::new_test(ctx, Arc::new(repo)).await?;
        repo.changeset(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("test changeset missing"))
    }

    fn build_path_restriction_metadata(
        path_acls: Vec<(&str, &str)>,
    ) -> Result<HashMap<NonRootMPath, PathRestrictionMetadata>> {
        path_acls
            .into_iter()
            .map(|(path, acl_str)| {
                Ok((
                    NonRootMPath::new(path)
                        .context("Failed to create NonRootMPath from test path")?,
                    PathRestrictionMetadata {
                        repo_region_acl: MononokeIdentity::from_str(acl_str)
                            .context("Failed to parse MononokeIdentity from ACL string")?,
                        permission_request_group: None,
                        rollout_allowlist_group: None,
                        read_only: false,
                    },
                ))
            })
            .collect()
    }

    /// Helper to create a ChangesetContext with restricted paths for testing.
    async fn create_test_changeset(
        fb: FacebookInit,
        path_acls: Vec<(&str, &str)>,
    ) -> Result<ChangesetContext<Repo>> {
        create_test_changeset_with_acl_provider(fb, path_acls, DummyAclProvider::new(fb)?).await
    }

    async fn create_test_changeset_with_acl_provider(
        fb: FacebookInit,
        path_acls: Vec<(&str, &str)>,
        acl_provider: Arc<dyn AclProvider>,
    ) -> Result<ChangesetContext<Repo>> {
        let restricted_paths =
            create_test_restricted_paths_with_acl_provider(fb, path_acls, acl_provider).await?;
        changeset_with_restricted_paths(fb, restricted_paths, "file.txt").await
    }

    #[mononoke::fbinit_test]
    async fn test_restricted_paths_access_exact_match(fb: FacebookInit) -> Result<()> {
        let cs_ctx = create_test_changeset(fb, vec![("restricted/dir", "TIER:my-acl")]).await?;

        let paths = BTreeSet::from(["restricted/dir".to_string()]);
        let response = restricted_paths_access_impl(&cs_ctx, paths, false)
            .await
            .expect("restricted_paths_access_impl failed");

        assert_eq!(response.are_restricted, thrift::PathCoverage::ALL);
        assert_eq!(response.restriction_roots.len(), 1);
        let roots = response
            .restriction_roots
            .get("restricted/dir")
            .expect("Expected restriction root for 'restricted/dir'");
        assert_eq!(roots[0].path, "restricted/dir");
        assert_eq!(roots[0].acls, vec!["TIER:my-acl"]);
        // No explicit permission_request_group configured, so it defaults to the
        // repo_region_acl identity `TIER:my-acl`, surfaced as its data portion.
        assert_eq!(
            roots[0].permission_request_group,
            Some("my-acl".to_string())
        );

        Ok(())
    }

    // What it tests: SCS restricted path access should include config-only
    // restrictions in Both mode after metadata results are unioned.
    // Expected: config-only restrictions are reported as restricted.
    #[mononoke::fbinit_test]
    async fn test_restricted_paths_access_both_mode_config_only_root(
        fb: FacebookInit,
    ) -> Result<()> {
        let cs_ctx = create_both_mode_test_changeset(
            fb,
            vec![("config_only", "TIER:config-acl")],
            vec![],
            vec!["config_only/file.txt"],
        )
        .await?;

        let paths = BTreeSet::from(["config_only/file.txt".to_string()]);
        let response = restricted_paths_access_impl(&cs_ctx, paths, false)
            .await
            .map_err(|err| anyhow::anyhow!("{err:?}"))?;

        assert_eq!(response.are_restricted, thrift::PathCoverage::ALL);
        let roots = response
            .restriction_roots
            .get("config_only/file.txt")
            .ok_or_else(|| anyhow::anyhow!("expected restriction root for config-only path"))?;
        let root = roots
            .first()
            .ok_or_else(|| anyhow::anyhow!("expected one config-only restriction root"))?;
        assert_eq!(root.path, "config_only");
        assert_eq!(root.acls, vec!["TIER:config-acl"]);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_restricted_paths_access_nested_path(fb: FacebookInit) -> Result<()> {
        let cs_ctx = create_test_changeset(fb, vec![("restricted", "TIER:my-acl")]).await?;

        let paths = BTreeSet::from(["restricted/subdir/file.txt".to_string()]);
        let response = restricted_paths_access_impl(&cs_ctx, paths, false)
            .await
            .expect("restricted_paths_access_impl failed");

        assert_eq!(response.are_restricted, thrift::PathCoverage::ALL);
        assert_eq!(response.restriction_roots.len(), 1);
        let roots = response
            .restriction_roots
            .get("restricted/subdir/file.txt")
            .expect("Expected restriction root for 'restricted/subdir/file.txt'");
        assert_eq!(roots[0].path, "restricted");

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_restricted_paths_access_no_match(fb: FacebookInit) -> Result<()> {
        let cs_ctx = create_test_changeset(fb, vec![("restricted/dir", "TIER:my-acl")]).await?;

        let paths = BTreeSet::from(["other/path/file.txt".to_string()]);
        let response = restricted_paths_access_impl(&cs_ctx, paths, false)
            .await
            .expect("restricted_paths_access_impl failed");

        assert_eq!(response.are_restricted, thrift::PathCoverage::NONE);
        assert!(response.restriction_roots.is_empty());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_restricted_paths_access_sibling_path(fb: FacebookInit) -> Result<()> {
        let cs_ctx = create_test_changeset(fb, vec![("foo/bar", "TIER:my-acl")]).await?;

        // foo/baz is a sibling of foo/bar, not under it
        let paths = BTreeSet::from(["foo/baz/file.txt".to_string()]);
        let response = restricted_paths_access_impl(&cs_ctx, paths, false)
            .await
            .expect("restricted_paths_access_impl failed");

        assert_eq!(response.are_restricted, thrift::PathCoverage::NONE);
        assert!(response.restriction_roots.is_empty());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_restricted_paths_access_multiple_restrictions_single_path(
        fb: FacebookInit,
    ) -> Result<()> {
        let cs_ctx = create_test_changeset(
            fb,
            vec![("first", "TIER:first-acl"), ("second", "TIER:second-acl")],
        )
        .await?;

        let paths = BTreeSet::from(["second/nested/file.txt".to_string()]);
        let response = restricted_paths_access_impl(&cs_ctx, paths, false)
            .await
            .expect("restricted_paths_access_impl failed");

        assert_eq!(response.are_restricted, thrift::PathCoverage::ALL);
        let roots = response
            .restriction_roots
            .get("second/nested/file.txt")
            .expect("Expected restriction root for 'second/nested/file.txt'");
        assert_eq!(roots[0].path, "second");
        assert_eq!(roots[0].acls, vec!["TIER:second-acl"]);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_restricted_paths_access_multiple_paths_from_different_roots(
        fb: FacebookInit,
    ) -> Result<()> {
        let cs_ctx = create_test_changeset(
            fb,
            vec![
                ("first", "TIER:first-acl"),
                ("second", "TIER:second-acl"),
                ("third/nested", "TIER:third-acl"),
            ],
        )
        .await?;

        let paths = BTreeSet::from([
            "first/file1.txt".to_string(),
            "second/subdir/file2.txt".to_string(),
            "third/nested/deep/file3.txt".to_string(),
            "unrestricted/file4.txt".to_string(),
        ]);
        let response = restricted_paths_access_impl(&cs_ctx, paths, false)
            .await
            .expect("restricted_paths_access_impl failed");

        // 3 restricted + 1 unrestricted = SOME
        assert_eq!(response.are_restricted, thrift::PathCoverage::SOME);
        assert_eq!(response.restriction_roots.len(), 3);

        // Verify each restricted path has correct root
        let first_roots = response
            .restriction_roots
            .get("first/file1.txt")
            .expect("Expected restriction root for 'first/file1.txt'");
        assert_eq!(first_roots[0].path, "first");
        assert_eq!(first_roots[0].acls, vec!["TIER:first-acl"]);

        let second_roots = response
            .restriction_roots
            .get("second/subdir/file2.txt")
            .expect("Expected restriction root for 'second/subdir/file2.txt'");
        assert_eq!(second_roots[0].path, "second");
        assert_eq!(second_roots[0].acls, vec!["TIER:second-acl"]);

        let third_roots = response
            .restriction_roots
            .get("third/nested/deep/file3.txt")
            .expect("Expected restriction root for 'third/nested/deep/file3.txt'");
        assert_eq!(third_roots[0].path, "third/nested");
        assert_eq!(third_roots[0].acls, vec!["TIER:third-acl"]);

        // Unrestricted path should not be in restriction_roots
        assert!(
            !response
                .restriction_roots
                .contains_key("unrestricted/file4.txt")
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_restricted_paths_access_no_restrictions_configured(
        fb: FacebookInit,
    ) -> Result<()> {
        let cs_ctx = create_test_changeset(fb, vec![]).await?;

        let paths = BTreeSet::from(["any/path/file.txt".to_string()]);
        let response = restricted_paths_access_impl(&cs_ctx, paths, false)
            .await
            .expect("restricted_paths_access_impl failed");

        assert_eq!(response.are_restricted, thrift::PathCoverage::NONE);
        assert_eq!(response.has_access, thrift::PathCoverage::ALL);
        assert!(response.restriction_roots.is_empty());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_restricted_paths_access_check_permissions_populates_authorized_paths(
        fb: FacebookInit,
    ) -> Result<()> {
        let cs_ctx = create_test_changeset(fb, vec![("restricted", "TIER:my-acl")]).await?;

        let paths = BTreeSet::from([
            "restricted/file.txt".to_string(),
            "unrestricted/file.txt".to_string(),
        ]);
        let response = restricted_paths_access_impl(
            &cs_ctx, paths, true, // check_permissions = true
        )
        .await
        .expect("restricted_paths_access_impl failed");

        // DummyAclProvider grants access, so both paths should be authorized
        // The unrestricted path is always authorized, and restricted path
        // depends on DummyAclProvider behavior
        assert!(!response.authorized_paths.is_empty());
        assert!(
            response
                .authorized_paths
                .contains(&"unrestricted/file.txt".to_string())
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_restricted_paths_access_all_paths_restricted(fb: FacebookInit) -> Result<()> {
        let cs_ctx = create_test_changeset(fb, vec![("restricted", "TIER:my-acl")]).await?;

        let paths = BTreeSet::from([
            "restricted/file1.txt".to_string(),
            "restricted/file2.txt".to_string(),
            "restricted/subdir/file3.txt".to_string(),
        ]);
        let response = restricted_paths_access_impl(&cs_ctx, paths, false)
            .await
            .expect("restricted_paths_access_impl failed");

        assert_eq!(response.are_restricted, thrift::PathCoverage::ALL);
        assert_eq!(response.restriction_roots.len(), 3);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_restricted_paths_access_all_paths_unrestricted(fb: FacebookInit) -> Result<()> {
        let cs_ctx = create_test_changeset(fb, vec![("restricted", "TIER:my-acl")]).await?;

        let paths = BTreeSet::from([
            "unrestricted/file1.txt".to_string(),
            "other/file2.txt".to_string(),
            "another/subdir/file3.txt".to_string(),
        ]);
        let response = restricted_paths_access_impl(&cs_ctx, paths, false)
            .await
            .expect("restricted_paths_access_impl failed");

        assert_eq!(response.are_restricted, thrift::PathCoverage::NONE);
        assert!(response.restriction_roots.is_empty());

        Ok(())
    }

    // Tests for `find_nested_restricted_roots`

    /// Helper to collect stream results into a Vec for easier testing
    async fn collect_nested_roots(
        cs_ctx: &ChangesetContext<Repo>,
        filter: BTreeSet<String>,
    ) -> Result<Vec<(String, String)>> {
        let result = collect_nested_roots_with_options(cs_ctx, filter, false, false).await?;
        Ok(result
            .into_iter()
            .map(|(path, acl, _has_access, _prg)| (path, acl))
            .collect())
    }

    async fn collect_nested_roots_with_options(
        cs_ctx: &ChangesetContext<Repo>,
        filter: BTreeSet<String>,
        check_permissions: bool,
        return_only_accessible: bool,
    ) -> Result<Vec<(String, String, Option<bool>, Option<String>)>> {
        use futures::TryStreamExt;
        use scs_errors::LoggableError;

        find_nested_restricted_roots(cs_ctx, filter, check_permissions, return_only_accessible)
            .await
            .map_err(|e| anyhow::anyhow!(e.status_and_description().1))?
            .map_ok(|item| {
                (
                    item.path,
                    item.acls.into_iter().next().unwrap_or_default(),
                    item.has_access,
                    item.permission_request_group,
                )
            })
            .try_collect()
            .await
            .map_err(|e| anyhow::anyhow!(e.status_and_description().1))
            .context("Collecting stream into vec")
    }

    #[mononoke::fbinit_test]
    async fn test_find_nested_roots_no_restrictions(fb: FacebookInit) -> Result<()> {
        let cs_ctx = create_test_changeset(fb, vec![]).await?;
        let filter = BTreeSet::new();

        let result = collect_nested_roots(&cs_ctx, filter).await?;

        assert!(result.is_empty());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_find_nested_roots_empty_filter_returns_all(fb: FacebookInit) -> Result<()> {
        let cs_ctx = create_test_changeset(
            fb,
            vec![
                ("first/path", "TIER:first-acl"),
                ("second/path", "TIER:second-acl"),
            ],
        )
        .await?;
        let filter = BTreeSet::new();

        let result = collect_nested_roots(&cs_ctx, filter).await?;

        assert_eq!(result.len(), 2);
        // Verify both paths are present (order may vary due to HashMap iteration)
        let paths: Vec<&str> = result.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"first/path"));
        assert!(paths.contains(&"second/path"));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_find_nested_roots_filter_exact_match(fb: FacebookInit) -> Result<()> {
        let cs_ctx = create_test_changeset(
            fb,
            vec![
                ("first/path", "TIER:first-acl"),
                ("second/path", "TIER:second-acl"),
            ],
        )
        .await?;
        let filter = BTreeSet::from(["first/path".to_string()]);

        let result = collect_nested_roots(&cs_ctx, filter).await?;

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "first/path");
        assert_eq!(result[0].1, "TIER:first-acl");

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_find_nested_roots_filter_parent_of_root(fb: FacebookInit) -> Result<()> {
        // Filter is parent of root (root starts with filter)
        let cs_ctx = create_test_changeset(fb, vec![("foo/bar/restricted", "TIER:my-acl")]).await?;
        let filter = BTreeSet::from(["foo".to_string()]);

        let result = collect_nested_roots(&cs_ctx, filter).await?;

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "foo/bar/restricted");

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_find_nested_roots_filter_child_of_root_returns_empty(
        fb: FacebookInit,
    ) -> Result<()> {
        // Filter is a child of the root - should NOT match because we only return
        // roots that are under the filter, not roots that contain the filter
        let cs_ctx = create_test_changeset(fb, vec![("foo/bar", "TIER:my-acl")]).await?;
        let filter = BTreeSet::from(["foo/bar/baz/deep".to_string()]);

        let result = collect_nested_roots(&cs_ctx, filter).await?;

        // The root "foo/bar" contains the filter "foo/bar/baz/deep", but is not under it,
        // so it should NOT be returned
        assert!(result.is_empty());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_find_nested_roots_filter_no_match(fb: FacebookInit) -> Result<()> {
        let cs_ctx = create_test_changeset(
            fb,
            vec![
                ("first/path", "TIER:first-acl"),
                ("second/path", "TIER:second-acl"),
            ],
        )
        .await?;
        let filter = BTreeSet::from(["third/path".to_string()]);

        let result = collect_nested_roots(&cs_ctx, filter).await?;

        assert!(result.is_empty());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_find_nested_roots_filter_sibling_no_match(fb: FacebookInit) -> Result<()> {
        // Sibling paths should not match
        let cs_ctx = create_test_changeset(fb, vec![("foo/bar", "TIER:my-acl")]).await?;
        let filter = BTreeSet::from(["foo/baz".to_string()]);

        let result = collect_nested_roots(&cs_ctx, filter).await?;

        assert!(result.is_empty());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_find_nested_roots_multiple_filters(fb: FacebookInit) -> Result<()> {
        let cs_ctx = create_test_changeset(
            fb,
            vec![
                ("first/path", "TIER:first-acl"),
                ("second/path", "TIER:second-acl"),
                ("third/path", "TIER:third-acl"),
            ],
        )
        .await?;
        let filter = BTreeSet::from(["first/path".to_string(), "third/path".to_string()]);

        let result = collect_nested_roots(&cs_ctx, filter).await?;

        assert_eq!(result.len(), 2);
        let paths: Vec<&str> = result.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"first/path"));
        assert!(paths.contains(&"third/path"));
        assert!(!paths.contains(&"second/path"));

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_find_nested_roots_check_permissions_populates_access(
        fb: FacebookInit,
    ) -> Result<()> {
        let cs_ctx =
            create_test_changeset(fb, vec![("restricted", "REPO_REGION:restricted-acl")]).await?;

        let result =
            collect_nested_roots_with_options(&cs_ctx, BTreeSet::new(), true, false).await?;

        assert_eq!(
            result,
            vec![(
                "restricted".to_string(),
                "REPO_REGION:restricted-acl".to_string(),
                Some(true),
                // permission_request_group defaults to the repo_region_acl data portion.
                Some("restricted-acl".to_string()),
            )]
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_find_nested_roots_return_only_accessible_filters_denied_results(
        fb: FacebookInit,
    ) -> Result<()> {
        let cs_ctx = create_test_changeset_with_acl_provider(
            fb,
            vec![("restricted", "REPO_REGION:restricted-acl")],
            TestAclProvider::new(false),
        )
        .await?;

        let result =
            collect_nested_roots_with_options(&cs_ctx, BTreeSet::new(), false, true).await?;

        assert!(result.is_empty());

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_find_nested_roots_return_only_accessible_hides_access_when_not_requested(
        fb: FacebookInit,
    ) -> Result<()> {
        let cs_ctx =
            create_test_changeset(fb, vec![("restricted", "REPO_REGION:restricted-acl")]).await?;

        let result =
            collect_nested_roots_with_options(&cs_ctx, BTreeSet::new(), false, true).await?;

        assert_eq!(
            result,
            vec![(
                "restricted".to_string(),
                "REPO_REGION:restricted-acl".to_string(),
                None,
                // permission_request_group is populated regardless of permission checks.
                Some("restricted-acl".to_string()),
            )]
        );

        Ok(())
    }

    // What it tests: Both mode should include config-only roots when finding
    // nested restrictions.
    // Expected: config-only roots are returned after Both mode unions config
    // and AclManifest metadata.
    #[mononoke::fbinit_test]
    async fn test_find_nested_roots_both_mode_config_only_root(fb: FacebookInit) -> Result<()> {
        let cs_ctx = create_both_mode_test_changeset(
            fb,
            vec![("config_only", "TIER:config-acl")],
            vec![],
            vec!["config_only/file.txt"],
        )
        .await?;

        let result = collect_nested_roots(&cs_ctx, BTreeSet::new()).await?;

        assert_eq!(
            result,
            vec![("config_only".to_string(), "TIER:config-acl".to_string())]
        );

        Ok(())
    }

    // Other unit tests

    #[mononoke::test]
    fn test_compute_path_coverage_empty() {
        assert_eq!(compute_path_coverage(vec![]), thrift::PathCoverage::NONE);
    }

    #[mononoke::test]
    fn test_compute_path_coverage_all_true() {
        assert_eq!(
            compute_path_coverage(vec![true, true, true]),
            thrift::PathCoverage::ALL
        );
    }

    #[mononoke::test]
    fn test_compute_path_coverage_all_false() {
        assert_eq!(
            compute_path_coverage(vec![false, false]),
            thrift::PathCoverage::NONE
        );
    }

    #[mononoke::test]
    fn test_compute_path_coverage_mixed() {
        assert_eq!(
            compute_path_coverage(vec![true, false, true]),
            thrift::PathCoverage::SOME
        );
    }

    #[mononoke::test]
    fn test_is_prefix_of_same_path() {
        let path = NonRootMPath::new("foo/bar").expect("Failed to create NonRootMPath");
        assert!(path.is_prefix_of(&path));
    }

    #[mononoke::test]
    fn test_compute_path_coverage_single_true() {
        assert_eq!(compute_path_coverage(vec![true]), thrift::PathCoverage::ALL);
    }

    #[mononoke::test]
    fn test_compute_path_coverage_single_false() {
        assert_eq!(
            compute_path_coverage(vec![false]),
            thrift::PathCoverage::NONE
        );
    }

    /// What it tests: a restriction root configured with an explicit
    /// `permission_request_group` (distinct from its `repo_region_acl`) surfaces the
    /// configured group in both the access response and the find stream.
    /// Expected: `permission_request_group` == Some("some_group") on both shapes,
    /// proving the builders read the effective group and not `repo_region_acl`.
    #[mononoke::fbinit_test]
    async fn test_permission_request_group_uses_configured_group(fb: FacebookInit) -> Result<()> {
        let cs_ctx = create_test_changeset_with_group(
            fb,
            "restricted",
            "REPO_REGION:restricted-acl",
            "GROUP:some_group",
        )
        .await?;

        // access response (PathRestrictionRoot)
        let paths = BTreeSet::from(["restricted/file.txt".to_string()]);
        let response = restricted_paths_access_impl(&cs_ctx, paths, false)
            .await
            .map_err(|err| anyhow::anyhow!("{err:?}"))?;
        let roots = response
            .restriction_roots
            .get("restricted/file.txt")
            .ok_or_else(|| anyhow::anyhow!("expected restriction root for restricted/file.txt"))?;
        assert_eq!(
            roots[0].permission_request_group,
            Some("some_group".to_string()),
            "access response should surface the configured group, not repo_region_acl"
        );

        // find stream (CommitFindRestrictedPathsStreamItem)
        let found =
            collect_nested_roots_with_options(&cs_ctx, BTreeSet::new(), false, false).await?;
        assert_eq!(
            found,
            vec![(
                "restricted".to_string(),
                "REPO_REGION:restricted-acl".to_string(),
                None,
                Some("some_group".to_string()),
            )],
            "find stream should surface the configured group"
        );

        Ok(())
    }

    /// Parse restriction metadata for a single root that pins an explicit
    /// `permission_request_group` distinct from its `repo_region_acl`, so a test can
    /// assert the response surfaces the configured group rather than the ACL fallback.
    fn build_path_restriction_metadata_with_group(
        path: &str,
        acl: &str,
        group: &str,
    ) -> Result<HashMap<NonRootMPath, PathRestrictionMetadata>> {
        Ok(HashMap::from([(
            NonRootMPath::new(path).context("Failed to create NonRootMPath from test path")?,
            PathRestrictionMetadata {
                repo_region_acl: MononokeIdentity::from_str(acl)
                    .context("Failed to parse repo_region_acl identity")?,
                permission_request_group: Some(
                    MononokeIdentity::from_str(group)
                        .context("Failed to parse permission_request_group identity")?,
                ),
                rollout_allowlist_group: None,
                read_only: false,
            },
        )]))
    }

    /// Build a ChangesetContext whose single restriction root carries an explicit
    /// `permission_request_group`. Mirrors `create_test_changeset` but injects the
    /// group so tests can distinguish the configured group from the ACL fallback.
    async fn create_test_changeset_with_group(
        fb: FacebookInit,
        path: &str,
        acl: &str,
        group: &str,
    ) -> Result<ChangesetContext<Repo>> {
        let config = RestrictedPathsConfig {
            path_restriction_metadata: build_path_restriction_metadata_with_group(
                path, acl, group,
            )?,
            manifest_id_store_config: RestrictedPathsManifestIdStoreConfig {
                use_manifest_id_cache: false,
                cache_update_interval_ms: 100,
                ..Default::default()
            },
            acl_manifest_mode: AclManifestMode::Disabled,
            ..Default::default()
        };
        let manifest_id_store = Arc::new(
            SqlRestrictedPathsManifestIdStoreBuilder::with_sqlite_in_memory()?
                .with_repo_id(RepositoryId::new(0)),
        );
        let config_based = Arc::new(RestrictedPathsConfigBased::new(
            config,
            manifest_id_store,
            None,
        ));
        let derived_data_repo: Repo = TestRepoFactory::new(fb)?.build().await?;
        let restricted_paths = Arc::new(RestrictedPaths::new(
            config_based,
            DummyAclProvider::new(fb)?,
            MononokeScubaSampleBuilder::with_discard(),
            derived_data_repo.repo_derived_data_arc(),
        )?);

        changeset_with_restricted_paths(fb, restricted_paths, format!("{path}/file.txt").as_str())
            .await
    }

    /// Build a root-commit ChangesetContext for a repo wired with the given
    /// restricted paths, adding `file_path` so the commit has content to query.
    /// Shared by `create_test_changeset_with_acl_provider` and
    /// `create_test_changeset_with_group`.
    async fn changeset_with_restricted_paths(
        fb: FacebookInit,
        restricted_paths: ArcRestrictedPaths,
        file_path: &str,
    ) -> Result<ChangesetContext<Repo>> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = TestRepoFactory::new(fb)
            .context("Failed to create TestRepoFactory")?
            .with_restricted_paths(restricted_paths)
            .build()
            .await
            .context("Failed to build test repo")?;
        let root_cs_id = CreateCommitContext::new_root(&ctx, &repo)
            .add_file(file_path, "content")
            .commit()
            .await
            .context("Failed to create root commit")?;
        let repo_ctx = RepoContext::new_test(ctx.clone(), Arc::new(repo))
            .await
            .context("Failed to create test RepoContext")?;
        repo_ctx
            .changeset(root_cs_id)
            .await
            .context(format!("Failed to resolve changeset {root_cs_id}"))?
            .ok_or_else(|| anyhow::anyhow!("Changeset {root_cs_id} not found"))
    }
}
