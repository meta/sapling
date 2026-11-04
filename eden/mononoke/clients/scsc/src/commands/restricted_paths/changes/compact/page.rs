/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! The only boundary from a raw compact response to comparison data.

use std::collections::BTreeSet;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use scs_client_raw::thrift;

use super::CheckedPolicy;
use super::CompactOutput;
use super::ComparedPaths;
use super::ComparisonBuilder;
use super::CopySource;
use super::PathPolicy;
use super::PermissionChange;
use super::PolicyIndex;
use super::SourceIndex;
use super::SubtreePolicy;
use super::SubtreeSource;
use super::coverage;
use crate::args::commit_id::map_commit_id;

/// One page that satisfied every response invariant, with canonical
/// page-local policy and source tables.
pub(super) struct ValidatedPage {
    paths: ComparedPaths,
    continuation: Option<String>,
}

impl ValidatedPage {
    /// Validates `response` as the page that follows `after` for a request
    /// of at most `limit` paths.
    pub(super) fn try_new(
        response: thrift::CommitRestrictedPathsChangesResponse,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Self> {
        ensure!(
            response.checked_both_sides == Some(true),
            "SCS server does not support complete restriction comparisons"
        );
        ensure!(
            (1..=thrift::COMMIT_RESTRICTED_PATHS_CHANGES_MAX_LIMIT).contains(&limit)
                && response.changed_paths.len() <= usize::try_from(limit)?,
            "SCS restriction page exceeds the requested limit"
        );
        let mut previous = after;
        for path in &response.changed_paths {
            ensure!(
                valid_path(path, false)
                    && previous.is_none_or(|previous| path_after(path, previous)),
                "SCS restriction page did not advance in path order"
            );
            previous = Some(path);
        }
        if let Some(last) = &response.last_path {
            ensure!(
                valid_path(last, false)
                    && after.is_none_or(|after| path_after(last, after))
                    && response
                        .changed_paths
                        .last()
                        .is_none_or(|path| path == last),
                "SCS restriction continuation must advance and match the page boundary"
            );
        }
        let paths: BTreeSet<_> = response.changed_paths.iter().collect();
        ensure!(
            response
                .authorized_paths
                .iter()
                .all(|path| paths.contains(path))
                && response
                    .authorized_paths
                    .windows(2)
                    .all(|pair| path_after(&pair[1], &pair[0])),
            "SCS authorized paths must be an ordered subset of changed paths"
        );
        ensure!(
            response.restriction_roots.is_empty(),
            "SCS compact response also contains expanded roots"
        );
        let compact = response
            .compact
            .context("SCS server does not support compact restriction comparisons")?;
        ensure!(
            compact
                .restriction_roots
                .iter()
                .all(|(path, roots)| paths.contains(path) && !roots.is_empty())
                && compact
                    .permission_changes
                    .keys()
                    .all(|path| paths.contains(path)),
            "SCS compact maps contain invalid or extraneous paths"
        );
        validate_references(&compact)?;
        let expected_authorized: Vec<_> = response
            .changed_paths
            .iter()
            .filter(|path| {
                compact.restriction_roots.get(*path).is_none_or(|roots| {
                    roots
                        .iter()
                        .all(|index| compact.policies[*index as usize].has_access)
                })
            })
            .collect();
        ensure!(
            expected_authorized
                .iter()
                .copied()
                .eq(&response.authorized_paths)
                && response.are_restricted
                    == coverage(
                        compact.restriction_roots.len(),
                        paths.len(),
                        thrift::PathCoverage::NONE
                    )
                && response.has_access
                    == coverage(
                        expected_authorized.len(),
                        paths.len(),
                        thrift::PathCoverage::ALL
                    ),
            "SCS compact verdicts do not match actual-content policy references"
        );

        let policy_count = compact.policies.len();
        let source_count = compact.subtree_sources.len();
        let page_paths = ComparedPaths {
            changed_paths: response.changed_paths,
            authorized_paths: response.authorized_paths,
            compact: CompactOutput {
                policies: compact
                    .policies
                    .into_iter()
                    .map(policy)
                    .collect::<Result<_>>()?,
                restriction_roots: compact
                    .restriction_roots
                    .into_iter()
                    .map(|(path, roots)| Ok((path, restrictions(roots, policy_count)?)))
                    .collect::<Result<_>>()?,
                permission_changes: compact
                    .permission_changes
                    .into_iter()
                    .map(|(path, change)| {
                        Ok((path, permission_change(change, policy_count, source_count)?))
                    })
                    .collect::<Result<_>>()?,
                subtree_sources: compact
                    .subtree_sources
                    .into_iter()
                    .map(source)
                    .collect::<Result<_>>()?,
            },
        };
        let mut builder = ComparisonBuilder::default();
        builder.add(page_paths);
        Ok(Self {
            paths: builder.finish(),
            continuation: response.last_path,
        })
    }

    /// The cursor for the next page, or `None` when this page completes the
    /// comparison.
    pub(super) fn continuation(&self) -> Option<&str> {
        self.continuation.as_deref()
    }

    pub(super) fn into_paths(self) -> ComparedPaths {
        self.paths
    }
}

fn path_after(path: &str, after: &str) -> bool {
    path.split('/').cmp(after.split('/')).is_gt()
}

fn valid_path(path: &str, allow_root: bool) -> bool {
    (allow_root && path.is_empty())
        || (!path.is_empty()
            && !path.contains('\0')
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."))
}

fn identity(identity: &str) -> bool {
    identity
        .split_once(':')
        .is_some_and(|(kind, data)| !kind.is_empty() && !data.is_empty())
}

fn bonsai(id: &thrift::CommitId) -> Result<String> {
    let (scheme, hash) =
        map_commit_id(id).context("SCS restriction source has an unsupported commit ID")?;
    ensure!(
        scheme == "bonsai" && hash.len() == 64,
        "SCS restriction source must have a 32-byte Bonsai commit ID"
    );
    Ok(hash)
}

fn policy(value: thrift::CheckedPathRestrictionPolicy) -> Result<CheckedPolicy> {
    ensure!(
        valid_path(&value.path, true)
            && !value.acls.is_empty()
            && value.acls.iter().all(|acl| identity(acl))
            && identity(&value.permission_request_group_identity),
        "SCS compact policy has invalid identity metadata"
    );
    Ok(CheckedPolicy {
        path: value.path,
        acls: value
            .acls
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        permission_request_group_identity: value.permission_request_group_identity,
        has_access: value.has_access,
    })
}

fn source(value: thrift::SubtreeSource) -> Result<SubtreeSource> {
    ensure!(
        valid_path(&value.source_prefix, true) && valid_path(&value.destination_prefix, true),
        "SCS compact subtree source has an invalid prefix"
    );
    Ok(SubtreeSource {
        commit_id: bonsai(&value.commit_id)?,
        source_prefix: value.source_prefix,
        destination_prefix: value.destination_prefix,
    })
}

fn row(index: i32, count: usize) -> Result<usize> {
    let index = usize::try_from(index).context("SCS compact index is negative")?;
    ensure!(index < count, "SCS compact index is out of range");
    Ok(index)
}

fn restrictions(indices: Vec<i32>, policy_count: usize) -> Result<Vec<PolicyIndex>> {
    indices
        .into_iter()
        .map(|index| Ok(PolicyIndex(row(index, policy_count)?)))
        .collect()
}

fn path_policy(value: thrift::PathPolicyInfoCompact, policy_count: usize) -> Result<PathPolicy> {
    Ok(PathPolicy {
        restrictions: restrictions(value.restrictions, policy_count)?,
    })
}

fn permission_change(
    value: thrift::PathPermissionChangeInfoCompact,
    policy_count: usize,
    source_count: usize,
) -> Result<PermissionChange> {
    Ok(PermissionChange {
        base: path_policy(value.base, policy_count)?,
        target: path_policy(value.target, policy_count)?,
        copy_sources: value
            .copy_sources
            .into_iter()
            .map(|copy| {
                ensure!(
                    valid_path(&copy.path, false),
                    "SCS restriction copy source has an invalid path"
                );
                Ok(CopySource {
                    commit_id: bonsai(&copy.commit_id)?,
                    path: copy.path,
                    info: path_policy(copy.info, policy_count)?,
                })
            })
            .collect::<Result<_>>()?,
        subtree_source: value
            .subtree_source
            .map(|source| {
                Ok::<_, anyhow::Error>(SubtreePolicy {
                    subtree_source_index: SourceIndex(row(
                        source.subtree_source_index,
                        source_count,
                    )?),
                    info: path_policy(source.info, policy_count)?,
                })
            })
            .transpose()?,
    })
}

fn all_rows_referenced(indices: impl Iterator<Item = i32>, count: usize) -> Result<()> {
    let used = indices
        .map(|index| usize::try_from(index).context("SCS compact index is negative"))
        .collect::<Result<BTreeSet<_>>>()?;
    ensure!(
        used.into_iter().eq(0..count),
        "SCS compact table has unreferenced rows or invalid indices"
    );
    Ok(())
}

fn validate_references(compact: &thrift::CommitRestrictedPathsChangesCompact) -> Result<()> {
    let policies = compact.permission_changes.values().flat_map(|change| {
        change
            .base
            .restrictions
            .iter()
            .chain(&change.target.restrictions)
            .chain(
                change
                    .copy_sources
                    .iter()
                    .flat_map(|source| &source.info.restrictions),
            )
            .chain(
                change
                    .subtree_source
                    .iter()
                    .flat_map(|source| &source.info.restrictions),
            )
            .copied()
    });
    all_rows_referenced(
        compact
            .restriction_roots
            .values()
            .flatten()
            .copied()
            .chain(policies),
        compact.policies.len(),
    )?;
    all_rows_referenced(
        compact.permission_changes.values().filter_map(|change| {
            change
                .subtree_source
                .as_ref()
                .map(|source| source.subtree_source_index)
        }),
        compact.subtree_sources.len(),
    )?;
    for (path, change) in &compact.permission_changes {
        if let Some(source) = &change.subtree_source {
            let mapping = compact
                .subtree_sources
                .get(usize::try_from(source.subtree_source_index)?)
                .context("SCS compact subtree source index is out of range")?;
            let prefix = &mapping.destination_prefix;
            ensure!(
                prefix.is_empty()
                    || path == prefix
                    || path
                        .strip_prefix(prefix)
                        .is_some_and(|suffix| suffix.starts_with('/')),
                "SCS subtree source does not cover its changed path"
            );
        }
    }
    Ok(())
}
