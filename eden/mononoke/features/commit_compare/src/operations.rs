/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;

use anyhow::Result;
use context::CoreContext;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures_watchdog::WatchdogExt;
use itertools::Either;
use itertools::Itertools;
use maplit::btreeset;
use mononoke_api::ChangesetContext;
use mononoke_api::ChangesetDiffItem;
use mononoke_api::ChangesetFileOrdering;
use mononoke_api::MononokeError;
use mononoke_api::RepoContext;
use mononoke_api::RestrictedPathsPolicy;
use mononoke_types::path::MPath;
use source_control as source_control_thrift;

use crate::commit_compare_path::CommitComparePath;
use crate::commit_compare_path::add_mutable_renames;
use crate::commit_compare_path::find_commit_compare_parent;
use crate::identity::map_commit_identity;

// Magic number used when we want to limit concurrency with buffered.
const CONCURRENCY_LIMIT: usize = 100;

/// Result of `commit_compare` including both the thrift response and
/// the number of diff entries, which callers can use for admission
/// control or observability.
pub struct CommitCompareResult {
    pub response: source_control_thrift::CommitCompareResponse,
    pub diff_count: usize,
}

/// Returns the maximum number of paths allowed in an unordered commit_compare,
/// read from JustKnobs (`scm/mononoke:commit_compare_unordered_max_paths`).
pub fn unordered_max_paths() -> Result<usize> {
    Ok(justknobs::get_as::<usize>(
        "scm/mononoke:commit_compare_unordered_max_paths",
        None,
    ))
}

/// Core commit_compare logic, shared between SCS and diff_service.
///
/// Takes resolved changesets and params, performs the diff, and returns
/// the thrift response along with the number of diff entries.
/// The caller is responsible for resolving changesets from whatever input
/// format they receive (CommitSpecifier, CommitId, etc.).
///
/// If `other_changeset` is `None`, the function will attempt to find the
/// parent changeset using `find_commit_compare_parent`.
///
/// How restricted-path denials are handled is governed by `policy`:
/// `Strict` fails the whole diff on any denial; `SkipAndCollect` omits or
/// downgrades denied entries per the per-side absent rule, recording them
/// against their denying restriction.
///
/// Limit semantics: when the policy retains omitted paths, the page
/// budget covers scanned entries, so denied entries consume
/// `limit`/`max_paths`; otherwise filtering happens before the budget and
/// pages fill with visible entries. An all-denied page still advances. A
/// partial page may therefore contain fewer visible entries than the limit
/// without implying the end of the diff.
pub async fn commit_compare<R: crate::Repo>(
    ctx: &CoreContext,
    repo: &RepoContext<R>,
    base_changeset: ChangesetContext<R>,
    other_changeset: Option<ChangesetContext<R>>,
    params: &source_control_thrift::CommitCompareParams,
    policy: &RestrictedPathsPolicy,
) -> Result<CommitCompareResult> {
    commit_compare_with_generation_logging(
        ctx,
        repo,
        base_changeset,
        other_changeset,
        params,
        true,
        policy,
    )
    .await
}

/// Variant that lets internal paging emit generation telemetry only once.
pub async fn commit_compare_with_generation_logging<R: crate::Repo>(
    ctx: &CoreContext,
    repo: &RepoContext<R>,
    mut base_changeset: ChangesetContext<R>,
    other_changeset: Option<ChangesetContext<R>>,
    params: &source_control_thrift::CommitCompareParams,
    log_generation_difference: bool,
    policy: &RestrictedPathsPolicy,
) -> Result<CommitCompareResult> {
    add_mutable_renames(&mut base_changeset, params).await?;

    let other_changeset = match other_changeset {
        Some(cs) => Some(cs),
        None => find_commit_compare_parent(repo, &mut base_changeset, params).await?,
    };

    if log_generation_difference {
        let base_generation = base_changeset.generation().await?.value();
        let other_generation = match other_changeset {
            Some(ref cs) => cs.generation().await?.value(),
            None => base_generation,
        };
        let generation_diff = base_generation.abs_diff(other_generation);
        let mut scuba = ctx.scuba().clone();
        scuba.log_with_msg(
            "Commit compare generation difference",
            format!("{generation_diff}"),
        );
    }

    // Parse diff items
    let mut last_path = None;
    let mut diff_items: BTreeSet<_> = params
        .compare_items
        .iter()
        .filter_map(|item| match *item {
            source_control_thrift::CommitCompareItem::FILES => Some(ChangesetDiffItem::FILES),
            source_control_thrift::CommitCompareItem::TREES => Some(ChangesetDiffItem::TREES),
            _ => None,
        })
        .collect();

    if diff_items.is_empty() {
        diff_items = btreeset! { ChangesetDiffItem::FILES };
    }

    let paths: Option<Vec<MPath>> = match &params.paths {
        None => None,
        Some(paths) => Some(
            paths
                .iter()
                .map(MPath::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| MononokeError::InvalidRequest(error.to_string()))?,
        ),
    };

    let (diff_count, diff_files, diff_trees) = match params.ordered_params {
        None => {
            let max_paths = unordered_max_paths()?;
            let diff = match other_changeset {
                Some(ref other_changeset) => {
                    base_changeset
                        .diff(
                            other_changeset,
                            !params.skip_copies_renames,
                            params.compare_with_subtree_copy_sources.unwrap_or_default(),
                            paths,
                            diff_items,
                            ChangesetFileOrdering::Unordered,
                            Some(max_paths + 1),
                            policy,
                        )
                        .watched()
                        .await?
                }
                None => {
                    base_changeset
                        .diff_root(
                            paths,
                            diff_items,
                            ChangesetFileOrdering::Unordered,
                            Some(max_paths + 1),
                            policy,
                        )
                        .watched()
                        .await?
                }
            };

            let diff_count = diff.len();
            let scanned = diff_count + policy.omitted_entry_count().unwrap_or(0);
            if scanned > max_paths {
                return Err(MononokeError::InvalidRequest(format!(
                    "commit_compare: unordered diff has more than {max_paths} entries. \
                     Use ordered_params with pagination instead.",
                ))
                .into());
            }

            let (files, trees) = stream::iter(diff)
                .map(|diff| CommitComparePath::from_path_diff(diff, &params.identity_schemes))
                .buffered(CONCURRENCY_LIMIT)
                .try_collect::<Vec<_>>()
                .await?
                .into_iter()
                .partition_map(|diff| match diff {
                    CommitComparePath::File(entry) => Either::Left(entry),
                    CommitComparePath::Tree(entry) => Either::Right(entry),
                });
            (diff_count, files, trees)
        }
        Some(ref ordered_params) => {
            let limit: usize = ordered_params.limit.try_into().map_err(|_| {
                MononokeError::InvalidRequest(format!("invalid limit: {}", ordered_params.limit))
            })?;

            if limit > source_control_thrift::consts::COMMIT_COMPARE_ORDERED_MAX_LIMIT as usize {
                return Err(MononokeError::InvalidRequest(format!(
                    "limit {} exceeds maximum {}",
                    limit,
                    source_control_thrift::consts::COMMIT_COMPARE_ORDERED_MAX_LIMIT
                ))
                .into());
            }

            let after = ordered_params
                .after_path
                .as_ref()
                .map(|after| {
                    MPath::try_from(after).map_err(|e| {
                        MononokeError::InvalidRequest(format!(
                            "invalid continuation path '{after}': {e}"
                        ))
                    })
                })
                .transpose()?;

            let diff = match other_changeset {
                Some(ref other_changeset) => {
                    base_changeset
                        .diff(
                            other_changeset,
                            !params.skip_copies_renames,
                            params.compare_with_subtree_copy_sources.unwrap_or_default(),
                            paths,
                            diff_items,
                            ChangesetFileOrdering::Ordered { after },
                            Some(limit),
                            policy,
                        )
                        .await?
                }
                None => {
                    base_changeset
                        .diff_root(
                            paths,
                            diff_items,
                            ChangesetFileOrdering::Ordered { after },
                            Some(limit),
                            policy,
                        )
                        .await?
                }
            };

            let diff_count = diff.len();
            let scanned = diff_count + policy.omitted_entry_count().unwrap_or(0);

            let diff_items: Vec<CommitComparePath> = stream::iter(diff)
                .map(|diff| CommitComparePath::from_path_diff(diff, &params.identity_schemes))
                .buffered(CONCURRENCY_LIMIT)
                .try_collect()
                .await?;

            if scanned >= limit {
                // Resume after the last scanned path, whether visible or
                // omitted, so the next page continues past both.
                let visible_last = diff_items.last().map(|item| item.path()).transpose()?;
                last_path = policy.last_scanned_path(visible_last);
            }

            let (files, trees) = diff_items.into_iter().partition_map(|diff| match diff {
                CommitComparePath::File(entry) => Either::Left(entry),
                CommitComparePath::Tree(entry) => Either::Right(entry),
            });
            (diff_count, files, trees)
        }
    };

    let other_commit_ids = match other_changeset {
        None => None,
        Some(other_changeset) => {
            let is_snapshot = other_changeset.bonsai_changeset().await?.is_snapshot();
            let schemes = if is_snapshot {
                BTreeSet::from([source_control_thrift::CommitIdentityScheme::BONSAI])
            } else {
                params.identity_schemes.clone()
            };
            Some(map_commit_identity(&other_changeset, &schemes).await?)
        }
    };

    Ok(CommitCompareResult {
        response: source_control_thrift::CommitCompareResponse {
            diff_files,
            diff_trees,
            other_commit_ids,
            last_path,
            ..Default::default()
        },
        diff_count,
    })
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use fbinit::FacebookInit;
    use futures::FutureExt;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs_async;
    use maplit::hashmap;
    use mononoke_api::Mononoke;
    use mononoke_api::Repo;
    use mononoke_macros::mononoke;
    use tests_utils::CreateCommitContext;

    use super::*;

    /// Helper: create a repo with a root changeset and a child changeset that adds
    /// `file_count` new files (file_0 .. file_{n-1}).
    async fn setup_repo_with_diff(
        fb: FacebookInit,
        file_count: usize,
    ) -> Result<(
        RepoContext<Repo>,
        ChangesetContext<Repo>,
        ChangesetContext<Repo>,
    )> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let root = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("base", "content")
            .commit()
            .await?;
        let mut child_builder = CreateCommitContext::new(&ctx, &repo, vec![root]);
        for i in 0..file_count {
            child_builder = child_builder.add_file(format!("file_{i}").as_str(), format!("c{i}"));
        }
        let child = child_builder.commit().await?;

        let mononoke = Mononoke::new_test(vec![("test".to_string(), repo)]).await?;
        let repo_ctx = mononoke
            .repo(ctx, "test")
            .await?
            .expect("repo exists")
            .build()
            .await?;
        let base_cs = repo_ctx
            .changeset(root)
            .await?
            .ok_or_else(|| anyhow!("root changeset not found"))?;
        let other_cs = repo_ctx
            .changeset(child)
            .await?
            .ok_or_else(|| anyhow!("child changeset not found"))?;
        Ok((repo_ctx, base_cs, other_cs))
    }

    #[mononoke::fbinit_test]
    async fn test_unordered_diff_rejects_over_limit(fb: FacebookInit) -> Result<()> {
        let (repo_ctx, base_cs, other_cs) = setup_repo_with_diff(fb, 8).await?;
        let params = source_control_thrift::CommitCompareParams::default();

        let result = with_just_knobs_async(
            JustKnobsInMemory::new(hashmap! {
                "scm/mononoke:commit_compare_unordered_max_paths".to_string() => KnobVal::Int(5),
            }),
            async {
                commit_compare(
                    repo_ctx.ctx(),
                    &repo_ctx,
                    base_cs,
                    Some(other_cs),
                    &params,
                    &RestrictedPathsPolicy::Strict,
                )
                .await
            }
            .boxed(),
        )
        .await;

        match result {
            Ok(_) => panic!("should fail when diff exceeds limit"),
            Err(err) => {
                let err_str = format!("{err:#}");
                assert!(
                    err_str.contains("has more than"),
                    "expected 'has more than' in error, got: {err_str}"
                );
            }
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_unordered_diff_succeeds_at_limit(fb: FacebookInit) -> Result<()> {
        let (repo_ctx, other_cs, base_cs) = setup_repo_with_diff(fb, 5).await?;
        let params = source_control_thrift::CommitCompareParams::default();

        let result = with_just_knobs_async(
            JustKnobsInMemory::new(hashmap! {
                "scm/mononoke:commit_compare_unordered_max_paths".to_string() => KnobVal::Int(5),
            }),
            async {
                commit_compare(
                    repo_ctx.ctx(),
                    &repo_ctx,
                    base_cs,
                    Some(other_cs),
                    &params,
                    &RestrictedPathsPolicy::Strict,
                )
                .await
            }
            .boxed(),
        )
        .await;

        assert!(result.is_ok(), "should succeed when diff is at the limit");
        Ok(())
    }

    /// Proves early termination: with 20 file diffs and a limit of 5, the
    /// underlying diff() call receives limit=Some(6) and returns at most 6
    /// entries — NOT 20.
    #[mononoke::fbinit_test]
    async fn test_unordered_diff_truncates_at_limit_plus_one(fb: FacebookInit) -> Result<()> {
        let (_repo_ctx, other_cs, base_cs) = setup_repo_with_diff(fb, 20).await?;

        let max_paths: usize = 5;
        let diff = with_just_knobs_async(
            JustKnobsInMemory::new(hashmap! {
                "scm/mononoke:commit_compare_unordered_max_paths".to_string() => KnobVal::Int(max_paths as i64),
            }),
            async {
                base_cs
                    .diff(
                        &other_cs,
                        false,
                        false,
                        None,
                        btreeset! { ChangesetDiffItem::FILES },
                        ChangesetFileOrdering::Unordered,
                        Some(max_paths + 1),
                        &RestrictedPathsPolicy::Strict,
                    )
                    .await
            }
            .boxed(),
        )
        .await?;

        // The stream should have been truncated at max_paths + 1 = 6,
        // not returning all 20 file diffs.
        assert_eq!(
            diff.len(),
            max_paths + 1,
            "diff should be truncated at limit (max_paths + 1 = {}), got {}",
            max_paths + 1,
            diff.len()
        );

        // Also verify commit_compare itself rejects it
        let params = source_control_thrift::CommitCompareParams::default();
        let (repo_ctx2, other_cs2, base_cs2) = setup_repo_with_diff(fb, 20).await?;
        let result = with_just_knobs_async(
            JustKnobsInMemory::new(hashmap! {
                "scm/mononoke:commit_compare_unordered_max_paths".to_string() => KnobVal::Int(max_paths as i64),
            }),
            async {
                commit_compare(repo_ctx2.ctx(), &repo_ctx2, base_cs2, Some(other_cs2), &params, &RestrictedPathsPolicy::Strict)
                    .await
            }
            .boxed(),
        )
        .await;

        assert!(
            result.is_err(),
            "commit_compare should reject oversized diff"
        );
        Ok(())
    }

    /// Verifies that commit_compare also applies early termination and
    /// limit checking on the diff_root path (root commit with no parent).
    #[mononoke::fbinit_test]
    async fn test_unordered_diff_root_rejects_over_limit(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;

        // Create a root commit with 8 files (no parent)
        let mut builder = CreateCommitContext::new_root(&ctx, &repo);
        for i in 0..8 {
            builder = builder.add_file(format!("file_{i}").as_str(), format!("c{i}"));
        }
        let root = builder.commit().await?;

        let mononoke = Mononoke::new_test(vec![("test".to_string(), repo)]).await?;
        let repo_ctx = mononoke
            .repo(ctx, "test")
            .await?
            .expect("repo exists")
            .build()
            .await?;
        let base_cs = repo_ctx
            .changeset(root)
            .await?
            .ok_or_else(|| anyhow!("root changeset not found"))?;

        let params = source_control_thrift::CommitCompareParams::default();
        let result = with_just_knobs_async(
            JustKnobsInMemory::new(hashmap! {
                "scm/mononoke:commit_compare_unordered_max_paths".to_string() => KnobVal::Int(5),
            }),
            async {
                // Pass None as other_changeset to trigger the diff_root path.
                // find_commit_compare_parent returns None for root commits.
                commit_compare(
                    repo_ctx.ctx(),
                    &repo_ctx,
                    base_cs,
                    None,
                    &params,
                    &RestrictedPathsPolicy::Strict,
                )
                .await
            }
            .boxed(),
        )
        .await;

        match result {
            Ok(_) => panic!("diff_root path should reject when root commit exceeds limit"),
            Err(err) => {
                let err_str = format!("{err:#}");
                assert!(
                    err_str.contains("has more than"),
                    "expected 'has more than' in error, got: {err_str}"
                );
            }
        }
        Ok(())
    }

    /// The diff is probed with a limit of `max_paths + 1`, so the rejection message cannot
    /// know the size of the diff; it says that the diff has more than `max_paths` entries.
    #[mononoke::fbinit_test]
    async fn test_unordered_diff_over_limit_message_does_not_report_probe_limit(
        fb: FacebookInit,
    ) -> Result<()> {
        let (repo_ctx, base_cs, other_cs) = setup_repo_with_diff(fb, 8).await?;
        let params = source_control_thrift::CommitCompareParams::default();

        let err = with_just_knobs_async(
            JustKnobsInMemory::new(hashmap! {
                "scm/mononoke:commit_compare_unordered_max_paths".to_string() => KnobVal::Int(5),
            }),
            async {
                commit_compare(
                    repo_ctx.ctx(),
                    &repo_ctx,
                    base_cs,
                    Some(other_cs),
                    &params,
                    &RestrictedPathsPolicy::Strict,
                )
                .await
            }
            .boxed(),
        )
        .await
        .err()
        .expect("a diff of 8 entries must be rejected with a limit of 5");
        let err_str = format!("{err:#}");
        assert!(
            err_str.contains("unordered diff has more than 5 entries"),
            "got: {err_str}"
        );
        assert!(!err_str.contains("6 entries"), "got: {err_str}");
        Ok(())
    }
}
