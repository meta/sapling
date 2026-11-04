/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::future::Future;
use std::io::Write;

use anyhow::Result;
use futures::future;
use futures::stream;
use futures::stream::Stream;
use futures::stream::TryStreamExt;
use scs_client_raw::thrift;
use serde::Serialize;

use super::super::RestrictionRootInfo;
use super::super::path_coverage_to_string;
use super::super::render_restriction_root_line;
use crate::render::Render;

mod page;
#[cfg(test)]
mod tests;

use page::ValidatedPage;

#[derive(Serialize)]
pub(super) struct ChangesOutput {
    are_restricted: String,
    has_access: String,
    #[serde(flatten)]
    paths: ComparedPaths,
}

/// Changed paths whose references index their own policy and source tables.
#[derive(Serialize)]
struct ComparedPaths {
    changed_paths: Vec<String>,
    authorized_paths: Vec<String>,
    compact: CompactOutput,
}

#[derive(Serialize)]
struct CompactOutput {
    policies: Vec<CheckedPolicy>,
    restriction_roots: BTreeMap<String, Vec<PolicyIndex>>,
    permission_changes: BTreeMap<String, PermissionChange>,
    subtree_sources: Vec<SubtreeSource>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct CheckedPolicy {
    path: String,
    acls: Vec<String>,
    permission_request_group_identity: String,
    has_access: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct SubtreeSource {
    commit_id: String,
    source_prefix: String,
    destination_prefix: String,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
struct PolicyIndex(usize);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
struct SourceIndex(usize);

#[derive(Serialize)]
struct PathPolicy {
    restrictions: Vec<PolicyIndex>,
}

#[derive(Serialize)]
struct CopySource {
    commit_id: String,
    path: String,
    info: PathPolicy,
}

#[derive(Serialize)]
struct SubtreePolicy {
    subtree_source_index: SourceIndex,
    info: PathPolicy,
}

#[derive(Serialize)]
struct PermissionChange {
    base: PathPolicy,
    target: PathPolicy,
    copy_sources: Vec<CopySource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtree_source: Option<SubtreePolicy>,
}

/// Merges successive pages, interning policy and source rows in first-seen
/// order. `finish` sorts both tables and every reference set.
#[derive(Default)]
struct ComparisonBuilder {
    changed_paths: Vec<String>,
    authorized_paths: Vec<String>,
    policies: BTreeMap<CheckedPolicy, usize>,
    sources: BTreeMap<SubtreeSource, usize>,
    restriction_roots: BTreeMap<String, Vec<PolicyIndex>>,
    permission_changes: BTreeMap<String, PermissionChange>,
}

impl ComparisonBuilder {
    /// Every path in `paths` must follow every path already added.
    fn add(&mut self, paths: ComparedPaths) {
        let policies: Vec<_> = paths
            .compact
            .policies
            .into_iter()
            .map(|row| intern(&mut self.policies, row))
            .collect();
        let sources: Vec<_> = paths
            .compact
            .subtree_sources
            .into_iter()
            .map(|row| intern(&mut self.sources, row))
            .collect();
        let remap = Remap {
            policies: &policies,
            sources: &sources,
        };
        self.restriction_roots.extend(
            paths
                .compact
                .restriction_roots
                .into_iter()
                .map(|(path, roots)| (path, remap.restrictions(roots))),
        );
        self.permission_changes.extend(
            paths
                .compact
                .permission_changes
                .into_iter()
                .map(|(path, change)| (path, remap.change(change))),
        );
        self.changed_paths.extend(paths.changed_paths);
        self.authorized_paths.extend(paths.authorized_paths);
    }

    fn finish(self) -> ComparedPaths {
        let (policies, policy_positions) = sorted(self.policies);
        let (subtree_sources, source_positions) = sorted(self.sources);
        let remap = Remap {
            policies: &policy_positions,
            sources: &source_positions,
        };
        ComparedPaths {
            changed_paths: self.changed_paths,
            authorized_paths: self.authorized_paths,
            compact: CompactOutput {
                policies,
                restriction_roots: self
                    .restriction_roots
                    .into_iter()
                    .map(|(path, roots)| (path, remap.restrictions(roots)))
                    .collect(),
                permission_changes: self
                    .permission_changes
                    .into_iter()
                    .map(|(path, change)| (path, remap.change(change)))
                    .collect(),
                subtree_sources,
            },
        }
    }
}

fn intern<T: Ord>(table: &mut BTreeMap<T, usize>, row: T) -> usize {
    let next = table.len();
    *table.entry(row).or_insert(next)
}

/// Returns the rows in order and, for each first-seen position, the row's
/// sorted position.
fn sorted<T>(table: BTreeMap<T, usize>) -> (Vec<T>, Vec<usize>) {
    let mut positions = vec![0; table.len()];
    let rows = table
        .into_iter()
        .enumerate()
        .map(|(position, (row, first_seen))| {
            positions[first_seen] = position;
            row
        })
        .collect();
    (rows, positions)
}

/// New table positions, indexed by the old ones.
struct Remap<'a> {
    policies: &'a [usize],
    sources: &'a [usize],
}

impl Remap<'_> {
    fn restrictions(&self, indices: Vec<PolicyIndex>) -> Vec<PolicyIndex> {
        indices
            .into_iter()
            .map(|PolicyIndex(index)| PolicyIndex(self.policies[index]))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn policy(&self, policy: PathPolicy) -> PathPolicy {
        PathPolicy {
            restrictions: self.restrictions(policy.restrictions),
        }
    }

    fn change(&self, change: PermissionChange) -> PermissionChange {
        PermissionChange {
            base: self.policy(change.base),
            target: self.policy(change.target),
            copy_sources: change
                .copy_sources
                .into_iter()
                .map(|source| CopySource {
                    info: self.policy(source.info),
                    ..source
                })
                .collect(),
            subtree_source: change.subtree_source.map(|source| SubtreePolicy {
                subtree_source_index: SourceIndex(self.sources[source.subtree_source_index.0]),
                info: self.policy(source.info),
            }),
        }
    }
}

/// Fetches and validates pages serially, each request continuing from the
/// previous validated page. The stream ends after the first cursor-free page
/// or the first error.
fn validated_pages<F, Fut>(limit: i64, mut fetch: F) -> impl Stream<Item = Result<ValidatedPage>>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<thrift::CommitRestrictedPathsChangesResponse>>,
{
    // The state is the cursor to request next, or `None` after completion.
    stream::try_unfold(Some(None), move |after: Option<Option<String>>| {
        let request = after.map(|after| (fetch(after.clone()), after));
        async move {
            let Some((response, after)) = request else {
                return anyhow::Ok(None);
            };
            let page = ValidatedPage::try_new(response.await?, after.as_deref(), limit)?;
            let next = page.continuation().map(|cursor| Some(cursor.to_owned()));
            anyhow::Ok(Some((page, next)))
        }
    })
}

/// Consumes every page before returning the complete comparison.
pub(super) async fn complete<F, Fut>(limit: i64, fetch: F) -> Result<ChangesOutput>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<thrift::CommitRestrictedPathsChangesResponse>>,
{
    let builder = validated_pages(limit, fetch)
        .try_fold(ComparisonBuilder::default(), |mut builder, page| {
            builder.add(page.into_paths());
            future::ok(builder)
        })
        .await?;
    Ok(ChangesOutput::new(builder.finish()))
}

impl ChangesOutput {
    fn new(paths: ComparedPaths) -> Self {
        let total = paths.changed_paths.len();
        Self {
            are_restricted: path_coverage_to_string(coverage(
                paths.compact.restriction_roots.len(),
                total,
                thrift::PathCoverage::NONE,
            )),
            has_access: path_coverage_to_string(coverage(
                paths.authorized_paths.len(),
                total,
                thrift::PathCoverage::ALL,
            )),
            paths,
        }
    }
}

fn coverage(count: usize, total: usize, empty: thrift::PathCoverage) -> thrift::PathCoverage {
    if total == 0 {
        empty
    } else if count == 0 {
        thrift::PathCoverage::NONE
    } else if count == total {
        thrift::PathCoverage::ALL
    } else {
        thrift::PathCoverage::SOME
    }
}

impl Render for ChangesOutput {
    type Args = ();
    fn render(&self, _: &(), w: &mut dyn Write) -> Result<()> {
        writeln!(
            w,
            "Restricted: {}\nHas access: {}",
            self.are_restricted, self.has_access
        )?;
        let paths = &self.paths;
        if !paths.changed_paths.is_empty() {
            writeln!(w, "\nChanged paths:")?;
            for path in &paths.changed_paths {
                writeln!(w, "  {path}")?;
            }
        }
        if !paths.compact.restriction_roots.is_empty() {
            writeln!(w, "\nRestriction roots:")?;
            for (path, roots) in &paths.compact.restriction_roots {
                writeln!(w, "  {path}:")?;
                let display: BTreeSet<_> = roots
                    .iter()
                    .map(|index| {
                        let policy = &paths.compact.policies[index.0];
                        let group = policy
                            .permission_request_group_identity
                            .split_once(':')
                            .map(|(_, data)| data);
                        (
                            policy.path.clone(),
                            policy.acls.clone(),
                            group.map(str::to_owned),
                        )
                    })
                    .collect();
                for (root_path, acls, permission_request_group) in display {
                    render_restriction_root_line(
                        w,
                        &RestrictionRootInfo {
                            root_path,
                            acls,
                            permission_request_group,
                        },
                    )?;
                }
            }
        }
        if !paths.authorized_paths.is_empty() {
            writeln!(w, "\nAuthorized paths:")?;
            for path in &paths.authorized_paths {
                writeln!(w, "  {path}")?;
            }
        }
        Ok(())
    }
    fn render_json(&self, _: &(), w: &mut dyn Write) -> Result<()> {
        Ok(serde_json::to_writer(w, self)?)
    }
}
