/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use commit_compare::operations;
use context::CoreContext;
use diff_service_if as thrift;
use futures_stats::TimedFutureExt;
use mononoke_api::ChangesetContext;
use mononoke_api::ChangesetSpecifier;
use mononoke_api::RepoContext;
use mononoke_api::RestrictedPathsPolicy;
use mononoke_types::ChangesetId;
use scuba_ext::MononokeScubaSampleBuilder;
use source_control as source_control_thrift;

use crate::errors::ServerError;
use crate::repo::Repo;
use crate::service::AddScubaParams;
use crate::service::DiffServiceImpl;
use crate::utils::admission::InflightBytesGuard;
use crate::utils::admission::try_reserve_memory;
use crate::utils::diff_conversion::commit_id_to_specifier;
use crate::utils::scuba::log_estimated_input_size;

const COMMIT_COMPARE_INTERNAL_PAGE_SIZE_JK: &str =
    "scm/mononoke:diff_service_commit_compare_internal_page_size";

enum CommitCompareMode {
    Ordered {
        ordered_params: source_control_thrift::CommitCompareOrderedParams,
    },
    Unordered {
        max_paths: usize,
    },
}

#[derive(Clone, Copy)]
enum CommitCompareAdmissionMode {
    Full,
    Rolling { page_size: usize },
}

struct CommitCompareAdmission {
    estimated_bytes_per_path: u64,
    estimated_input_size: u64,
    initial_estimated_size: u64,
    mode: CommitCompareAdmissionMode,
    inflight_bytes: Arc<AtomicU64>,
    _request_inflight_guard: InflightBytesGuard,
    start_rss_bytes: Option<usize>,
}

impl CommitCompareAdmission {
    fn new(
        ctx: &CoreContext,
        estimated_bytes_per_path: u64,
        estimated_path_count: u64,
        rolling_page_size: Option<usize>,
        inflight_bytes: Arc<AtomicU64>,
    ) -> Result<Self, ServerError> {
        let estimated_input_size = estimated_bytes_per_path.saturating_mul(estimated_path_count);
        let mode = rolling_page_size.map_or(CommitCompareAdmissionMode::Full, |page_size| {
            CommitCompareAdmissionMode::Rolling { page_size }
        });
        let initial_estimated_size = match mode {
            CommitCompareAdmissionMode::Full => estimated_input_size,
            CommitCompareAdmissionMode::Rolling { page_size } => {
                estimated_bytes_per_path.saturating_mul(page_size as u64)
            }
        };
        let start_rss_bytes = memory::get_stats()
            .ok()
            .map(|stats| stats.total_rss_bytes.saturating_sub(stats.rss_free_bytes));
        let request_inflight_guard =
            try_reserve_memory(ctx, "initial", initial_estimated_size, &inflight_bytes)?;

        Ok(Self {
            estimated_bytes_per_path,
            estimated_input_size,
            initial_estimated_size,
            mode,
            inflight_bytes,
            _request_inflight_guard: request_inflight_guard,
            start_rss_bytes,
        })
    }

    fn reserve_later_page(
        &self,
        ctx: &CoreContext,
        page_limit: usize,
    ) -> Result<Option<InflightBytesGuard>, ServerError> {
        match self.mode {
            CommitCompareAdmissionMode::Full => Ok(None),
            CommitCompareAdmissionMode::Rolling { .. } => {
                let estimated_size = self
                    .estimated_bytes_per_path
                    .saturating_mul(page_limit as u64);
                try_reserve_memory(ctx, "later_page", estimated_size, &self.inflight_bytes)
                    .map(Some)
            }
        }
    }

    fn add_scuba_fields(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba
            .add(
                "commit_compare_admission_mode",
                match self.mode {
                    CommitCompareAdmissionMode::Full => "full",
                    CommitCompareAdmissionMode::Rolling { .. } => "rolling",
                },
            )
            .add(
                "commit_compare_initial_reservation_estimated_bytes",
                self.initial_estimated_size,
            );
        if let Some(start_rss_bytes) = self.start_rss_bytes {
            scuba.add("commit_compare_start_rss_bytes", start_rss_bytes);
        }
        if let Some(end_rss_bytes) = memory::get_stats()
            .ok()
            .map(|stats| stats.total_rss_bytes.saturating_sub(stats.rss_free_bytes))
        {
            scuba.add("commit_compare_end_rss_bytes", end_rss_bytes);
        }
        log_estimated_input_size(scuba, self.estimated_input_size, &self.inflight_bytes);
    }
}

impl DiffServiceImpl {
    pub async fn commit_compare(
        &self,
        ctx: CoreContext,
        request: thrift::CommitCompareRequest,
    ) -> Result<source_control_thrift::CommitCompareResponse, ServerError> {
        let repo_name = &request.repo_name;
        let mut params = request.params;
        let specifier = commit_id_to_specifier(&request.commit_id)?;
        let other_specifier = params
            .other_commit_id
            .as_ref()
            .map(commit_id_to_specifier)
            .transpose()?;

        let mode = match params.ordered_params.take() {
            Some(ordered_params) => CommitCompareMode::Ordered { ordered_params },
            None => CommitCompareMode::Unordered {
                max_paths: operations::unordered_max_paths()?,
            },
        };
        let internal_page_size = select_internal_page_size(&params, &mode);

        // Create separate repo contexts for each changeset to support cross-bubble diffs.
        // Each ephemeral changeset may live in a different bubble, so we need
        // independent repo contexts with per-changeset bubble resolution.
        let base_cs_id = filter_bonsai(&specifier);
        let other_cs_id = other_specifier.as_ref().and_then(filter_bonsai);

        let (base_repo, other_repo) = futures::try_join!(
            self.get_repo_context(ctx.clone(), repo_name, base_cs_id),
            self.get_repo_context(ctx.clone(), repo_name, other_cs_id),
        )?;

        let base_changeset = base_repo.changeset(specifier).await?.ok_or_else(|| {
            ServerError::RequestError(anyhow::anyhow!("commit not found: {:?}", request.commit_id))
        })?;

        let other_changeset = match other_specifier {
            Some(spec) => {
                let cs = other_repo
                    .changeset(spec)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("Other commit not found"))
                    .map_err(ServerError::RequestError)?;
                Some(cs)
            }
            None => None,
        };

        // The memo caches failures too, so the diff resurfaces this error without re-deriving.
        let (prewarm_stats, prewarm_result) =
            futures::future::try_join(base_changeset.prewarm_root_content_manifest(), async {
                match other_changeset.as_ref() {
                    Some(other_changeset) => other_changeset.prewarm_root_content_manifest().await,
                    None => Ok(()),
                }
            })
            .timed()
            .await;

        // OOM protection: reserve memory before doing any work. Requests
        // without internal paging retain the full request reservation. Paged
        // requests keep one internal page for the request lifetime.
        let estimated_bytes_per_path = justknobs::get_as::<u64>(
            "scm/mononoke:diff_service_commit_compare_estimated_bytes_per_path",
            None,
        );
        let max_limit = source_control_thrift::consts::COMMIT_COMPARE_ORDERED_MAX_LIMIT as u64;
        // Build the policy once: it selects the omission path below and
        // the stamped verdict, so the two cannot disagree.
        let policy = RestrictedPathsPolicy::for_scs_request(&ctx, repo_name);
        let estimated_path_count = match &mode {
            CommitCompareMode::Ordered { ordered_params } => {
                (ordered_params.limit as u64).min(max_limit)
            }
            CommitCompareMode::Unordered { max_paths } => *max_paths as u64,
        };
        let admission = CommitCompareAdmission::new(
            &ctx,
            estimated_bytes_per_path,
            estimated_path_count,
            internal_page_size,
            self.inflight_bytes.clone(),
        )?;
        let mut completed_path_count = 0usize;
        let mut page_count = 0usize;

        let (stats, result) = paged_commit_compare(
            &ctx,
            &base_repo,
            base_changeset,
            other_changeset,
            params,
            mode,
            internal_page_size,
            &admission,
            &mut completed_path_count,
            &mut page_count,
            &policy,
        )
        .timed()
        .await;

        let mut scuba = ctx.scuba().clone();
        scuba
            .add_future_stats(&stats)
            .add("commit_compare_internal_page_count", page_count)
            .add("commit_compare_completed_path_count", completed_path_count)
            .add(
                "commit_compare_partial_work_failure",
                result.is_err() && page_count > 0,
            );
        scuba
            .add(
                "commit_compare_prewarm_completion_time_us",
                prewarm_stats.completion_time.as_micros() as u64,
            )
            .add("commit_compare_prewarm_failed", prewarm_result.is_err());
        admission.add_scuba_fields(&mut scuba);
        let omitted_restricted_paths_count = policy.omitted_count();
        if let Ok(result) = result.as_ref() {
            scuba.add("commit_compare_path_count", result.diff_count);
            scuba.add(
                "omitted_restricted_paths_count",
                omitted_restricted_paths_count.unwrap_or(0),
            );
            policy.set_partial_if_omitted(&ctx);
        }
        scuba.log_with_msg("Computed commit compare", None);

        let result = result.map_err(map_commit_compare_error)?;
        let mut response = result.response;
        response.partial_info = crate::service::partial_response_info(&policy);
        Ok(response)
    }
}

async fn paged_commit_compare(
    ctx: &CoreContext,
    repo: &RepoContext<Repo>,
    base_changeset: ChangesetContext<Repo>,
    other_changeset: Option<ChangesetContext<Repo>>,
    mut params: source_control_thrift::CommitCompareParams,
    mode: CommitCompareMode,
    internal_page_size: Option<usize>,
    admission: &CommitCompareAdmission,
    completed_path_count: &mut usize,
    page_count: &mut usize,
    policy: &RestrictedPathsPolicy,
) -> anyhow::Result<operations::CommitCompareResult> {
    match internal_page_size {
        None => {
            params.ordered_params = match mode {
                CommitCompareMode::Ordered { ordered_params } => Some(ordered_params),
                CommitCompareMode::Unordered { .. } => None,
            };
            // Under Strict, denials fail the whole request.
            let result = operations::commit_compare(
                ctx,
                repo,
                base_changeset,
                other_changeset,
                &params,
                policy,
            )
            .await;
            if let Ok(result) = result.as_ref() {
                *page_count = 1;
                *completed_path_count = result.diff_count;
            }
            result
        }
        Some(internal_page_size) => {
            let (requested_limit, ordered_params, unordered_max_paths) = match mode {
                CommitCompareMode::Ordered { ordered_params } => {
                    let requested_limit = usize::try_from(ordered_params.limit).map_err(|_| {
                        anyhow::anyhow!(
                            "internally paged ordered commit_compare limit {} does not fit in usize",
                            ordered_params.limit
                        )
                    })?;
                    (requested_limit, ordered_params, None)
                }
                CommitCompareMode::Unordered { max_paths } => {
                    let requested_limit = max_paths.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!(
                            "unordered commit_compare maximum {max_paths} is too large to probe"
                        )
                    })?;
                    (
                        requested_limit,
                        source_control_thrift::CommitCompareOrderedParams::default(),
                        Some(max_paths),
                    )
                }
            };
            let mut response = source_control_thrift::CommitCompareResponse::default();
            let mut diff_count = 0;
            let mut remaining = requested_limit;
            let mut after_path = ordered_params.after_path.clone();

            loop {
                let page_limit = remaining.min(internal_page_size);
                let _page_inflight_guard = if *page_count > 0 {
                    admission.reserve_later_page(ctx, page_limit)?
                } else {
                    None
                };
                params.ordered_params = Some(source_control_thrift::CommitCompareOrderedParams {
                    after_path,
                    limit: page_limit.try_into().map_err(|_| {
                        anyhow::anyhow!(
                            "internal commit_compare page limit {page_limit} does not fit in i64"
                        )
                    })?,
                    ..ordered_params.clone()
                });

                let page = operations::commit_compare_with_generation_logging(
                    ctx,
                    repo,
                    base_changeset.clone(),
                    other_changeset.clone(),
                    &params,
                    *page_count == 0,
                    policy,
                )
                .await?;

                if page.diff_count > page_limit {
                    return Err(anyhow::anyhow!(
                        "internal commit_compare page returned {} entries for limit {page_limit}",
                        page.diff_count
                    ));
                }

                let page_diff_count = page.diff_count;
                let source_control_thrift::CommitCompareResponse {
                    diff_files,
                    other_commit_ids,
                    diff_trees,
                    last_path,
                    ..
                } = page.response;
                response.diff_files.extend(diff_files);
                response.diff_trees.extend(diff_trees);
                if response.other_commit_ids.is_none() {
                    response.other_commit_ids = other_commit_ids;
                }
                *page_count += 1;
                *completed_path_count += page_diff_count;
                diff_count += page_diff_count;
                remaining -= page_diff_count;

                if remaining == 0 {
                    if let Some(max_paths) = unordered_max_paths {
                        return Err(mononoke_api::MononokeError::InvalidRequest(format!(
                            "commit_compare: unordered diff has more than {max_paths} entries. \
                             Use ordered_params with pagination instead.",
                        ))
                        .into());
                    }
                    response.last_path = Some(last_path.ok_or_else(|| {
                        anyhow::anyhow!(
                            "full internal commit_compare page did not return a continuation path"
                        )
                    })?);
                    break;
                }

                if page_diff_count < page_limit {
                    response.last_path = None;
                    break;
                }

                after_path = Some(last_path.ok_or_else(|| {
                    anyhow::anyhow!(
                        "full internal commit_compare page did not return a continuation path"
                    )
                })?);
            }

            Ok(operations::CommitCompareResult {
                response,
                diff_count,
            })
        }
    }
}

fn select_internal_page_size(
    params: &source_control_thrift::CommitCompareParams,
    mode: &CommitCompareMode,
) -> Option<usize> {
    if !has_stable_paging_cursor(params) {
        return None;
    }

    let configured_page_size =
        justknobs::get_as::<usize>(COMMIT_COMPARE_INTERNAL_PAGE_SIZE_JK, None);
    let ordered_page_max = source_control_thrift::consts::COMMIT_COMPARE_ORDERED_MAX_LIMIT as usize;
    match mode {
        CommitCompareMode::Ordered { ordered_params } => {
            let requested_limit = usize::try_from(ordered_params.limit).ok()?;
            (configured_page_size > 0
                && configured_page_size < requested_limit
                && requested_limit <= ordered_page_max)
                .then_some(configured_page_size)
        }
        CommitCompareMode::Unordered { max_paths } => (configured_page_size > 0).then(|| {
            // Keep the extra over-limit probe on a later page so the
            // request-lifetime floor remains bounded by max_paths.
            configured_page_size
                .min(ordered_page_max)
                .min((*max_paths).max(1))
        }),
    }
}

#[cold]
#[inline(never)]
fn map_commit_compare_error(error: anyhow::Error) -> ServerError {
    let error = match error.downcast::<ServerError>() {
        Ok(error) => return error,
        Err(error) => error,
    };
    match error.downcast::<mononoke_api::MononokeError>() {
        Ok(error) => ServerError::from(error),
        Err(error) => ServerError::InternalError(error),
    }
}

fn has_stable_paging_cursor(params: &source_control_thrift::CommitCompareParams) -> bool {
    // Tree responses do not always have a unique path cursor, and mutable
    // history can change between internal calls. Keep both modes one-shot.
    !params.follow_mutable_file_history.unwrap_or(false)
        && !params
            .compare_items
            .contains(&source_control_thrift::CommitCompareItem::TREES)
}

fn filter_bonsai(specifier: &ChangesetSpecifier) -> Option<ChangesetId> {
    match specifier {
        ChangesetSpecifier::Bonsai(cs_id) | ChangesetSpecifier::EphemeralBonsai(cs_id, _) => {
            Some(*cs_id)
        }
        ChangesetSpecifier::Hg(_)
        | ChangesetSpecifier::Globalrev(_)
        | ChangesetSpecifier::GitSha1(_)
        | ChangesetSpecifier::Svnrev(_)
        | ChangesetSpecifier::P4Changelist(_) => None,
    }
}

impl AddScubaParams for thrift::CommitCompareRequest {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("repo_name", self.repo_name.clone());
    }
}

#[cfg(test)]
mod tests {
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn internal_paging_requires_a_stable_cursor() {
        let mut params = source_control_thrift::CommitCompareParams::default();

        assert!(has_stable_paging_cursor(&params));

        params
            .compare_items
            .insert(source_control_thrift::CommitCompareItem::TREES);
        assert!(!has_stable_paging_cursor(&params));

        params.compare_items.clear();
        params.follow_mutable_file_history = Some(true);
        assert!(!has_stable_paging_cursor(&params));
    }

    #[mononoke::fbinit_test]
    fn full_admission_uses_the_full_request_estimate(fb: FacebookInit) {
        // Given a three-path request without rolling admission.
        let ctx = CoreContext::new_with_scuba(fb, MononokeScubaSampleBuilder::with_discard());
        let admission =
            CommitCompareAdmission::new(&ctx, 1_024, 3, None, Arc::new(AtomicU64::new(0)))
                .expect("full admission should reserve its initial estimate");

        // When a later page is considered, then the full request remains the
        // only reservation.
        let later_page_guard = admission
            .reserve_later_page(&ctx, 1)
            .expect("full admission should not reject a later page");
        assert_eq!(admission.estimated_input_size, 3_072);
        assert_eq!(admission.initial_estimated_size, 3_072);
        assert!(matches!(&admission.mode, CommitCompareAdmissionMode::Full));
        assert!(later_page_guard.is_none());
    }

    #[mononoke::fbinit_test]
    fn rolling_admission_uses_one_page_and_reserves_later_pages(fb: FacebookInit) {
        // Given a three-path request with a one-path rolling floor.
        let ctx = CoreContext::new_with_scuba(fb, MononokeScubaSampleBuilder::with_discard());
        let admission =
            CommitCompareAdmission::new(&ctx, 1_024, 3, Some(1), Arc::new(AtomicU64::new(0)))
                .expect("rolling admission should reserve its initial floor");

        // When a later two-path page is considered, then it receives a
        // temporary reservation while the one-page floor stays held.
        let later_page_guard = admission
            .reserve_later_page(&ctx, 2)
            .expect("rolling admission should reserve a later page");
        assert_eq!(admission.estimated_input_size, 3_072);
        assert_eq!(admission.initial_estimated_size, 1_024);
        assert!(matches!(
            &admission.mode,
            CommitCompareAdmissionMode::Rolling { page_size: 1 }
        ));
        assert!(later_page_guard.is_some());
    }

    #[mononoke::test]
    fn structured_overload_survives_anyhow_mapping() {
        let error = ServerError::Overloaded("test overload".to_string());

        let mapped = map_commit_compare_error(anyhow::Error::new(error));
        assert!(matches!(
            mapped,
            ServerError::Overloaded(message) if message == "test overload"
        ));
    }
}
