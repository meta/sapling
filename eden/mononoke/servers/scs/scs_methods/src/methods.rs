/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use context::CoreContext;
use itertools::Itertools;
use metaconfig_types::CommitIdentityScheme;
use mononoke_api::DenyingRestriction;
use mononoke_api::RestrictedPathsPolicy;
use source_control as thrift;

use crate::from_request::FromRequest;
use crate::source_control_impl::SourceControlServiceImpl;

pub(crate) mod cloud;
pub(crate) mod commit;
pub(crate) mod commit_lookup_pushrebase_history;
pub(crate) mod commit_path;
pub(crate) mod commit_restricted_paths;
pub mod commit_sparse_profile_info;
#[cfg(fbcode_build)]
mod facebook;
pub(crate) mod file;
pub(crate) mod git;
pub(crate) mod git_repo_state;
pub(crate) mod megarepo;
pub(crate) mod repo;
pub(crate) mod tree;

/// Partial-response verdict for a unary method, from the request's omission
/// policy. `Some` carrying the exact signal when partial responses are
/// allowed for this request (`None`, field unset, when not). Group counts
/// reconcile with the total by construction: every recorded omission carries
/// its denying restriction. The caller sets `nocache: 1` from the same flag,
/// so a cached partial can never be served to an entitled caller.
pub(crate) fn partial_response_info(
    policy: &RestrictedPathsPolicy,
) -> Option<thrift::PartialResponseInfo> {
    Some(partial_response_info_from_omissions(
        policy.omitted_count()? as u64,
        policy
            .omitted_groups()
            .unwrap_or_default()
            .into_iter()
            .map(|(restriction, count)| (restriction, count as u64)),
    ))
}

/// Partial-response verdict from a count of omitted paths and its breakdown
/// by denying restriction. `omitted_by_restriction` stays unset when there
/// are no groups.
pub(crate) fn partial_response_info_from_omissions(
    omitted_count: u64,
    groups: impl IntoIterator<Item = (DenyingRestriction, u64)>,
) -> thrift::PartialResponseInfo {
    let groups = groups
        .into_iter()
        .map(
            |(restriction, omitted_count)| thrift::OmittedRestrictionGroup {
                restriction_root: restriction.restriction_root.map(|root| root.to_string()),
                repo_region_acl: restriction.repo_region_acl.to_string(),
                permission_request_group: restriction
                    .permission_request_group
                    .id_data()
                    .to_string(),
                omitted_count: omitted_count as i64,
                ..Default::default()
            },
        )
        .collect::<Vec<_>>();
    thrift::PartialResponseInfo {
        partial: omitted_count > 0,
        omitted_restricted_paths_count: Some(omitted_count as i64),
        omitted_by_restriction: (!groups.is_empty()).then_some(groups),
        ..Default::default()
    }
}

impl SourceControlServiceImpl {
    pub(crate) async fn list_repos(
        &self,
        _ctx: CoreContext,
        params: thrift::ListReposParams,
    ) -> Result<Vec<thrift::Repo>, scs_errors::ServiceError> {
        let snapshot = self.mononoke.repo_names_in_tier.load();
        let names = snapshot.iter();
        let names: Box<dyn Iterator<Item = _>> =
            if let Some(identity_schemes) = params.identity_schemes {
                let schemes = identity_schemes
                    .iter()
                    .map(CommitIdentityScheme::from_request)
                    .collect::<Result<Vec<_>, _>>()?;

                Box::new(names.filter(move |(_, default_scheme)| schemes.contains(default_scheme)))
            } else {
                Box::new(names)
            };

        Ok(names
            .sorted_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(repo_name, _)| thrift::Repo {
                name: repo_name.clone(),
                ..Default::default()
            })
            .collect())
    }

    pub(crate) async fn repo_exists(
        &self,
        _ctx: CoreContext,
        params: thrift::RepoExistsParams,
    ) -> Result<thrift::RepoExistsResponse, scs_errors::ServiceError> {
        let exists = self
            .mononoke
            .repo_names_in_tier
            .load()
            .contains_key(&params.repo_name);
        Ok(thrift::RepoExistsResponse {
            exists,
            ..Default::default()
        })
    }

    pub(crate) async fn storage_config_exists(
        &self,
        _ctx: CoreContext,
        params: thrift::StorageConfigExistsParams,
    ) -> Result<thrift::StorageConfigExistsResponse, scs_errors::ServiceError> {
        let exists = self
            .configs
            .storage_configs()
            .storage
            .contains_key(&params.name);
        Ok(thrift::StorageConfigExistsResponse {
            exists,
            manifest_version: self.configs.manifest_version(),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;

    use mononoke_api::OmissionCollector;
    use mononoke_macros::mononoke;
    use mononoke_types::NonRootMPath;
    use permission_checker::MononokeIdentity;
    use restricted_paths::DenyingRestriction;

    use super::*;

    fn collecting_policy() -> RestrictedPathsPolicy {
        RestrictedPathsPolicy::SkipAndCollect(Arc::new(OmissionCollector::default()))
    }

    fn denying_restriction(
        root: Option<&str>,
        acl: &str,
        group: &str,
    ) -> anyhow::Result<DenyingRestriction> {
        Ok(DenyingRestriction {
            restriction_root: root.map(NonRootMPath::new).transpose()?,
            repo_region_acl: MononokeIdentity::from_str(acl)?,
            permission_request_group: MononokeIdentity::from_str(group)?,
        })
    }

    #[mononoke::test]
    fn test_partial_response_info() -> anyhow::Result<()> {
        // Not allowed: no field.
        assert!(partial_response_info(&RestrictedPathsPolicy::Strict).is_none());
        // Allowed: always populated; `partial` iff count is positive,
        // agreeing with the `nocache` decision (set iff count positive) by
        // construction. A complete response carries no groups.
        let complete = partial_response_info(&collecting_policy()).expect("populated when allowed");
        assert!(!complete.partial);
        assert_eq!(complete.omitted_restricted_paths_count, Some(0));
        assert_eq!(complete.omitted_by_restriction, None);

        // Denials are grouped by restriction; group counts sum to the total.
        // A group whose source reports no root omits it; the request group
        // renders as its data portion.
        let policy = collecting_policy();
        let tent_a = denying_restriction(Some("tent_a"), "REPO_REGION:a_acl", "GROUP:a_group")?;
        let rootless =
            denying_restriction(None, "REPO_REGION:manifest_acl", "REPO_REGION:manifest_acl")?;
        policy.record_denial(&tent_a);
        policy.record_denial(&rootless);
        policy.record_denial(&tent_a);
        let partial = partial_response_info(&policy).expect("populated when allowed");
        assert!(partial.partial);
        assert_eq!(partial.omitted_restricted_paths_count, Some(3));
        let groups = partial
            .omitted_by_restriction
            .expect("partial response carries groups");
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups.iter().map(|group| group.omitted_count).sum::<i64>(),
            3,
        );
        let tent_group = groups
            .iter()
            .find(|group| group.restriction_root.as_deref() == Some("tent_a"))
            .expect("tent_a group present");
        assert_eq!(tent_group.repo_region_acl, "REPO_REGION:a_acl");
        assert_eq!(tent_group.permission_request_group, "a_group");
        assert_eq!(tent_group.omitted_count, 2);
        let rootless_group = groups
            .iter()
            .find(|group| group.restriction_root.is_none())
            .expect("rootless group present");
        assert_eq!(rootless_group.repo_region_acl, "REPO_REGION:manifest_acl");
        assert_eq!(rootless_group.permission_request_group, "manifest_acl");
        assert_eq!(rootless_group.omitted_count, 1);
        Ok(())
    }
}
