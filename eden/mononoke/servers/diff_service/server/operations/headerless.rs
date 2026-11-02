/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use context::CoreContext;
use diff as diff_feature;
use diff_service_if::*;
use diff_service_if_services::errors::diff_service::*;
use futures::stream::BoxStream;
use futures_stats::TimedFutureExt;
use mononoke_api::RestrictedPathsPolicy;
use scuba_ext::MononokeScubaSampleBuilder;

use crate::errors::ServerError;
use crate::service::AddScubaParams;
use crate::service::DiffServiceImpl;
use crate::utils::admission::InflightBytesGuard;
use crate::utils::admission::estimate_diff_size;
use crate::utils::admission::try_reserve_memory;
use crate::utils::scuba::log_estimated_input_size;
use crate::utils::scuba::log_input_ids;
use crate::utils::scuba::log_single_input_scuba;

mod defaults {
    pub const CONTEXT_LINES: i32 = 3;
}

const DIFF_CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4MB

/// Result of the shared work performed by `compute_headerless_unified_diff`.
/// Holds the computed diff, the inflight-memory guard the caller must keep
/// alive until the response is built, and a scuba builder pre-populated
/// with all metrics common to both transports (input ids, future stats,
/// is_binary, output_size, context_lines, inflight/estimated input bytes).
/// The caller only needs to add response-shape-specific stats (e.g.
/// `num_chunks` for the streamed transport) and call `log_with_msg`.
struct ComputedHeaderlessDiff {
    diff: diff_feature::types::HeaderlessUnifiedDiff,
    inflight_guard: InflightBytesGuard,
    scuba: MononokeScubaSampleBuilder,
    policy: RestrictedPathsPolicy,
}

impl DiffServiceImpl {
    /// Shared steps for both `diff_unified_headerless` and
    /// `diff_unified_headerless_unary`: validate inputs, log input IDs,
    /// resolve inputs, reserve inflight memory, build `HeaderlessDiffOpts`,
    /// call the feature crate, time it, and accumulate every metric common
    /// to both transports onto the returned scuba builder. The caller is
    /// responsible for the response shape (chunked vs unary), any
    /// shape-specific scuba additions, and the final `log_with_msg`.
    async fn compute_headerless_unified_diff(
        &self,
        ctx: &CoreContext,
        request: DiffUnifiedHeaderlessRequest,
    ) -> Result<ComputedHeaderlessDiff, ServerError> {
        if request.input.base.is_none() && request.input.other.is_none() {
            return Err(ServerError::ThriftError(RequestErrorReason::diff_error(
                DiffError {
                    reason: "At least one of base or other input must be provided".to_string(),
                    ..Default::default()
                },
            )));
        }

        let mut scuba = ctx.scuba().clone();
        log_input_ids(&mut scuba, &request.input);

        // Build the policy once: it selects the omission path in
        // `resolve_diff_inputs` and the stamped verdict, so the two cannot
        // disagree.
        let policy = RestrictedPathsPolicy::for_scs_request(ctx, &request.repo_name);
        let resolved = self
            .resolve_diff_inputs(ctx, &request.repo_name, request.input, &policy)
            .await?;

        let denied = policy.omitted_count().unwrap_or(0);
        if denied > 0 {
            scuba.add("omitted_restricted_paths_count", denied);
        }
        policy.set_partial_if_omitted(ctx);

        let estimated_input_size = estimate_diff_size(&resolved, ctx).await?;
        let inflight_guard =
            try_reserve_memory(ctx, "initial", estimated_input_size, &self.inflight_bytes)?;

        let context_lines = request
            .options
            .as_ref()
            .map_or(defaults::CONTEXT_LINES, |o| o.context_lines)
            as usize;

        let ignore_whitespace = request
            .options
            .as_ref()
            .is_some_and(|o| o.ignore_whitespace);

        let options = diff_feature::types::HeaderlessDiffOpts {
            context: context_lines,
            ignore_whitespace,
        };

        if denied > 0 && resolved.base.input().is_none() && resolved.other.input().is_none() {
            // Neither side visible: return the natural empty shape with
            // the partial verdict stamped.
            let diff = diff_feature::types::HeaderlessUnifiedDiff {
                raw_diff: Vec::new(),
                is_binary: false,
            };
            scuba
                .add("is_binary", diff.is_binary)
                .add("output_size", diff.raw_diff.len())
                .add("context_lines", context_lines);
            log_estimated_input_size(&mut scuba, estimated_input_size, &self.inflight_bytes);
            return Ok(ComputedHeaderlessDiff {
                diff,
                inflight_guard,
                scuba,
                policy,
            });
        }

        let (stats, diff_result) = async {
            diff_feature::operations::headerless::headerless_unified(
                ctx,
                resolved.base_repo.repo(),
                resolved.other_repo.repo(),
                resolved.base.input().cloned(),
                resolved.other.input().cloned(),
                options,
            )
            .await
        }
        .timed()
        .await;

        let diff = diff_result?;

        scuba
            .add_future_stats(&stats)
            .add("is_binary", diff.is_binary)
            .add("output_size", diff.raw_diff.len())
            .add("context_lines", context_lines);
        log_estimated_input_size(&mut scuba, estimated_input_size, &self.inflight_bytes);

        Ok(ComputedHeaderlessDiff {
            diff,
            inflight_guard,
            scuba,
            policy,
        })
    }

    pub async fn diff_unified_headerless(
        &self,
        ctx: CoreContext,
        request: DiffUnifiedHeaderlessRequest,
    ) -> Result<
        (
            DiffUnifiedHeaderlessResponse,
            BoxStream<'static, Result<DiffUnifiedHeaderlessChunk, DiffUnifiedHeaderlessStreamExn>>,
        ),
        ServerError,
    > {
        let ComputedHeaderlessDiff {
            diff,
            inflight_guard: _inflight_guard,
            mut scuba,
            policy,
        } = self.compute_headerless_unified_diff(&ctx, request).await?;

        // Split the diff into chunks for streaming
        let mut chunks = Vec::new();
        let total_size = diff.raw_diff.len();
        let mut offset = 0;

        while offset < total_size {
            let chunk_end = std::cmp::min(offset + DIFF_CHUNK_SIZE, total_size);
            let chunk_content = diff.raw_diff[offset..chunk_end].to_vec();

            chunks.push(Ok(DiffUnifiedHeaderlessChunk {
                content: chunk_content,
                ..Default::default()
            }));

            offset = chunk_end;
        }

        let num_chunks = chunks.len() as i64;

        scuba.add("num_chunks", num_chunks);
        scuba.log_with_msg("Computed unified headerless diff", None);

        let stream = futures::stream::iter(chunks);

        let response = DiffUnifiedHeaderlessResponse {
            is_binary: diff.is_binary,
            num_chunks,
            partial_info: crate::service::partial_response_info(&policy),
            ..Default::default()
        };
        Ok((response, Box::pin(stream)))
    }

    pub async fn diff_unified_headerless_unary(
        &self,
        ctx: CoreContext,
        request: DiffUnifiedHeaderlessRequest,
    ) -> Result<DiffUnifiedHeaderlessUnaryResponse, ServerError> {
        let ComputedHeaderlessDiff {
            diff,
            inflight_guard: _inflight_guard,
            mut scuba,
            policy,
        } = self.compute_headerless_unified_diff(&ctx, request).await?;

        // Distinct log_tag so Scuba can split unary vs streamed rows.
        scuba.log_with_msg("Computed unified headerless diff (unary)", None);

        Ok(DiffUnifiedHeaderlessUnaryResponse {
            is_binary: diff.is_binary,
            content: diff.raw_diff,
            partial_info: crate::service::partial_response_info(&policy),
            ..Default::default()
        })
    }
}

impl AddScubaParams for DiffUnifiedHeaderlessRequest {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("repo_name", self.repo_name.clone());

        if let Some(base_input) = &self.input.base {
            log_single_input_scuba(
                scuba,
                base_input,
                "path",
                "param_base_string",
                "param_base_lfs_pointer",
            );
        }

        if let Some(other_input) = &self.input.other {
            log_single_input_scuba(
                scuba,
                other_input,
                "other_path",
                "param_other_string",
                "param_other_lfs_pointer",
            );
        }
    }
}
