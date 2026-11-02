/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use context::CoreContext;
use diff as diff_feature;
use diff_service_if as thrift;
use diff_service_if_services::errors::diff_service as thrift_errors;
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
use crate::utils::diff_conversion::convert_copy_info;
use crate::utils::diff_conversion::convert_file_type;
use crate::utils::scuba::log_estimated_input_size;
use crate::utils::scuba::log_input_ids;
use crate::utils::scuba::log_single_input_scuba;

const DIFF_CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4MB

/// Result of the shared work performed by `compute_unified_diff`. Holds the
/// computed diff, the inflight-memory guard the caller must keep alive
/// until the response is built, and a scuba builder pre-populated with all
/// metrics common to both transports (input ids, future stats, is_binary,
/// output_size, context_lines, inflight/estimated input bytes). The caller
/// only needs to add response-shape-specific stats (e.g. `num_chunks` for
/// the streamed transport) and call `log_with_msg`.
struct ComputedUnifiedDiff {
    diff: diff_feature::types::UnifiedDiff,
    inflight_guard: InflightBytesGuard,
    scuba: MononokeScubaSampleBuilder,
    policy: RestrictedPathsPolicy,
}

impl DiffServiceImpl {
    /// Shared steps for both `diff_unified` and `diff_unified_unary`:
    /// validate inputs, log input IDs, resolve inputs, reserve inflight
    /// memory, build `UnifiedDiffOpts`, call the feature crate, time it,
    /// and accumulate every metric common to both transports onto the
    /// returned scuba builder. The caller is responsible for the response
    /// shape (chunked vs unary), any shape-specific scuba additions, and
    /// the final `log_with_msg`.
    async fn compute_unified_diff(
        &self,
        ctx: &CoreContext,
        request: thrift::DiffUnifiedRequest,
    ) -> Result<ComputedUnifiedDiff, ServerError> {
        if request.input.base.is_none() && request.input.other.is_none() {
            return Err(ServerError::ThriftError(
                thrift::RequestErrorReason::diff_error(thrift::DiffError {
                    reason: "At least one of base or other input must be provided".to_string(),
                    ..Default::default()
                }),
            ));
        }

        let mut scuba = ctx.scuba().clone();
        log_input_ids(&mut scuba, &request.input);

        let context_lines = request.options.context_lines;

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

        let options = diff_feature::types::UnifiedDiffOpts {
            context: context_lines as usize,
            copy_info: convert_copy_info(request.options.copy_info),
            file_type: convert_file_type(request.options.file_type),
            inspect_lfs_pointers: request.options.inspect_lfs_pointers,
            omit_content: matches!(request.options.diff_mode, thrift::DiffMode::OMIT_CONTENT),
            ignore_whitespace: request.options.ignore_whitespace,
        };

        if denied > 0 && resolved.base.input().is_none() && resolved.other.input().is_none() {
            // Neither side visible: return the natural empty shape with
            // the partial verdict stamped.
            let diff = diff_feature::types::UnifiedDiff {
                raw_diff: Vec::new(),
                is_binary: false,
            };
            scuba
                .add("is_binary", diff.is_binary)
                .add("output_size", diff.raw_diff.len())
                .add("context_lines", context_lines);
            log_estimated_input_size(&mut scuba, estimated_input_size, &self.inflight_bytes);
            return Ok(ComputedUnifiedDiff {
                diff,
                inflight_guard,
                scuba,
                policy,
            });
        }

        let (stats, diff_result) = async {
            diff_feature::operations::unified::unified(
                ctx,
                resolved
                    .base
                    .input()
                    .cloned()
                    .map(|input| (input, resolved.base_repo.repo())),
                resolved
                    .other
                    .input()
                    .cloned()
                    .map(|input| (input, resolved.other_repo.repo())),
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

        Ok(ComputedUnifiedDiff {
            diff,
            inflight_guard,
            scuba,
            policy,
        })
    }

    pub async fn diff_unified(
        &self,
        ctx: CoreContext,
        request: thrift::DiffUnifiedRequest,
    ) -> Result<
        (
            thrift::DiffUnifiedResponse,
            BoxStream<
                'static,
                Result<thrift::DiffUnifiedChunk, thrift_errors::DiffUnifiedStreamExn>,
            >,
        ),
        ServerError,
    > {
        let ComputedUnifiedDiff {
            diff,
            inflight_guard: _inflight_guard,
            mut scuba,
            policy,
        } = self.compute_unified_diff(&ctx, request).await?;

        let chunks: Vec<_> = diff
            .raw_diff
            .chunks(DIFF_CHUNK_SIZE)
            .map(|chunk| {
                Ok(thrift::DiffUnifiedChunk {
                    content: chunk.to_vec(),
                    ..Default::default()
                })
            })
            .collect();

        scuba.add("num_chunks", chunks.len());
        scuba.log_with_msg("Computed unified diff", None);

        let response = thrift::DiffUnifiedResponse {
            is_binary: diff.is_binary,
            num_chunks: chunks.len() as i64,
            partial_info: crate::service::partial_response_info(&policy),
            ..Default::default()
        };

        let stream = futures::stream::iter(chunks);
        Ok((response, Box::pin(stream)))
    }

    pub async fn diff_unified_unary(
        &self,
        ctx: CoreContext,
        request: thrift::DiffUnifiedRequest,
    ) -> Result<thrift::DiffUnifiedUnaryResponse, ServerError> {
        let ComputedUnifiedDiff {
            diff,
            inflight_guard: _inflight_guard,
            mut scuba,
            policy,
        } = self.compute_unified_diff(&ctx, request).await?;

        // Distinct log_tag so Scuba can split unary vs streamed rows.
        scuba.log_with_msg("Computed unified diff (unary)", None);

        Ok(thrift::DiffUnifiedUnaryResponse {
            is_binary: diff.is_binary,
            content: diff.raw_diff,
            partial_info: crate::service::partial_response_info(&policy),
            ..Default::default()
        })
    }
}

impl AddScubaParams for thrift::DiffUnifiedRequest {
    fn add_scuba_params(&self, scuba: &mut MononokeScubaSampleBuilder) {
        scuba.add("repo_name", self.repo_name.clone());

        // Log base input parameters
        if let Some(base_input) = &self.input.base {
            log_single_input_scuba(
                scuba,
                base_input,
                "path",
                "param_base_string",
                "param_base_lfs_pointer",
            );
        }

        // Log other input parameters
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
