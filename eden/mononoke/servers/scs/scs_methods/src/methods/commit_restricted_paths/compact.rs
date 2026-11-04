/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use dedupmap::DedupMap;
use mononoke_api::ChangedPathRestrictionInfo;
use mononoke_api::MononokeError;
use mononoke_api::PathAccessInfo;
use mononoke_api::PathPermissionChangeInfo;
use mononoke_api::PathPolicyInfo;
use mononoke_api::SubtreeSourcePolicyInfo;
use mononoke_types::NonRootMPath;
use source_control as thrift;

#[cfg(test)]
mod tests;

#[derive(Default)]
struct PolicyTable {
    policies: DedupMap<thrift::CheckedPathRestrictionPolicy>,
    subtree_sources: DedupMap<thrift::SubtreeSource>,
}

/// Public policy identity excludes rollout controls, but retains their checked outcome.
fn public_policy(info: &PathAccessInfo) -> Result<thrift::CheckedPathRestrictionPolicy> {
    Ok(thrift::CheckedPathRestrictionPolicy {
        path: info.restriction_root().to_string(),
        acls: vec![info.repo_region_acl().to_string()],
        permission_request_group_identity: info.permission_request_group().to_string(),
        has_access: info
            .has_access
            .context("compact restriction access was not checked")?,
        ..Default::default()
    })
}

fn public_policies(
    info: &PathPolicyInfo,
) -> Result<BTreeSet<thrift::CheckedPathRestrictionPolicy>> {
    check_access(info)?;
    info.restrictions.iter().map(public_policy).collect()
}

impl PolicyTable {
    fn index(&mut self, info: &PathAccessInfo) -> Result<i32> {
        self.public_index(public_policy(info)?)
    }

    fn public_index(&mut self, policy: thrift::CheckedPathRestrictionPolicy) -> Result<i32> {
        i32::try_from(
            self.policies
                .insert::<thrift::CheckedPathRestrictionPolicy, _>(Cow::Owned(policy)),
        )
        .context("compact policy index exceeds i32")
    }

    fn info(
        &mut self,
        policies: BTreeSet<thrift::CheckedPathRestrictionPolicy>,
    ) -> Result<thrift::PathPolicyInfoCompact> {
        Ok(thrift::PathPolicyInfoCompact {
            restrictions: policies
                .into_iter()
                .map(|policy| self.public_index(policy))
                .collect::<Result<BTreeSet<_>>>()?
                .into_iter()
                .collect(),
            ..Default::default()
        })
    }

    fn subtree_source(
        &mut self,
        source: &SubtreeSourcePolicyInfo,
        policies: BTreeSet<thrift::CheckedPathRestrictionPolicy>,
    ) -> Result<thrift::SubtreeSourcePolicyInfoCompact> {
        let mapping = thrift::SubtreeSource {
            commit_id: thrift::CommitId::bonsai(source.mapping.commit_id.as_ref().to_vec()),
            source_prefix: source.mapping.source_prefix.to_string(),
            destination_prefix: source.mapping.destination_prefix.to_string(),
            ..Default::default()
        };
        let index = self
            .subtree_sources
            .insert::<thrift::SubtreeSource, _>(Cow::Owned(mapping));
        Ok(thrift::SubtreeSourcePolicyInfoCompact {
            subtree_source_index: i32::try_from(index)
                .context("compact subtree source index exceeds i32")?,
            info: self.info(policies)?,
            ..Default::default()
        })
    }

    fn permission_change(
        &mut self,
        change: &PathPermissionChangeInfo,
    ) -> Result<Option<thrift::PathPermissionChangeInfoCompact>> {
        let base = public_policies(&change.base)?;
        let target = public_policies(&change.target)?;
        let copies = change
            .copy_sources
            .iter()
            .map(|source| Ok((source, public_policies(&source.info)?)))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|(_, policy)| policy != &target)
            .collect::<Vec<_>>();
        let subtree = change
            .subtree_source
            .as_ref()
            .map(|source| -> Result<_> { Ok((source, public_policies(&source.info)?)) })
            .transpose()?
            .filter(|(_, policy)| policy != &target);
        if base == target && copies.is_empty() && subtree.is_none() {
            return Ok(None);
        }
        // Decide which public differences survive before interning: hidden-only
        // differences must not leave unreferenced policy or mapping rows.
        Ok(Some(thrift::PathPermissionChangeInfoCompact {
            base: self.info(base)?,
            target: self.info(target)?,
            copy_sources: copies
                .into_iter()
                .map(|(source, policies)| {
                    Ok(thrift::PathRestrictionCopySourceInfoCompact {
                        commit_id: thrift::CommitId::bonsai(source.commit_id.as_ref().to_vec()),
                        path: source.path.to_string(),
                        info: self.info(policies)?,
                        ..Default::default()
                    })
                })
                .collect::<Result<_>>()?,
            subtree_source: subtree
                .map(|(source, policies)| self.subtree_source(source, policies))
                .transpose()?,
            ..Default::default()
        }))
    }
}

fn check_access(info: &PathPolicyInfo) -> Result<()> {
    let access = info
        .has_access
        .context("compact path access was not checked")?;
    let conjunction = info
        .restrictions
        .iter()
        .try_fold(true, |allowed, restriction| {
            restriction
                .has_access
                .context("compact restriction access was not checked")
                .map(|access| allowed && access)
        })?;
    ensure!(
        access == conjunction,
        "compact path access disagrees with its restriction checks"
    );
    Ok(())
}

/// Tables are response-local. Interning while emitting references ensures that
/// every definition is used, without assuming access is immutable during a read.
fn encode(
    results: &[(NonRootMPath, ChangedPathRestrictionInfo)],
) -> Result<thrift::CommitRestrictedPathsChangesCompact> {
    let mut table = PolicyTable::default();
    let mut restriction_roots = BTreeMap::new();
    let mut permission_changes = BTreeMap::new();
    for (path, info) in results {
        for content in info.content_sides() {
            check_access(content)?;
        }
        let roots = info
            .restriction_info()
            .map(|restriction| table.index(restriction))
            .collect::<Result<BTreeSet<_>>>()?;
        if !roots.is_empty() {
            restriction_roots.insert(path.to_string(), roots.into_iter().collect());
        }
        if let Some(change) = &info.permission_change
            && let Some(change) = table.permission_change(change)?
        {
            permission_changes.insert(path.to_string(), change);
        }
    }
    Ok(thrift::CommitRestrictedPathsChangesCompact {
        policies: table.policies.into_items(),
        restriction_roots,
        permission_changes,
        subtree_sources: table.subtree_sources.into_items(),
        ..Default::default()
    })
}

pub(crate) fn into_response(
    mut results: Vec<(NonRootMPath, ChangedPathRestrictionInfo)>,
    limit: Option<usize>,
) -> Result<thrift::CommitRestrictedPathsChangesResponse, scs_errors::ServiceError> {
    let last_path = super::retain_comparison_page(&mut results, limit);
    let compact = encode(&results).map_err(MononokeError::from)?;
    let mut response = super::comparison_common_response(&results);
    response.compact = Some(compact);
    response.last_path = last_path;
    Ok(response)
}
