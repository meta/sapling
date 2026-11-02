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
use scuba_ext::MononokeScubaSampleBuilder;

use crate::errors::ServerError;
use crate::service::AddScubaParams;
use crate::service::DiffServiceImpl;
use crate::utils::admission::estimate_diff_size;
use crate::utils::admission::try_reserve_memory;
use crate::utils::scuba::log_estimated_input_size;
use crate::utils::scuba::log_input_ids;
use crate::utils::scuba::log_single_input_scuba;

impl DiffServiceImpl {
    pub async fn diff_hunks(
        &self,
        ctx: CoreContext,
        request: thrift::DiffHunksRequest,
    ) -> Result<
        (
            thrift::DiffHunksResponse,
            BoxStream<'static, Result<thrift::DiffHunksData, thrift_errors::DiffHunksStreamExn>>,
        ),
        ServerError,
    > {
        // Validate that at least one input is provided
        if request.input.base.is_none() && request.input.other.is_none() {
            return Err(ServerError::ThriftError(
                thrift::RequestErrorReason::diff_error(thrift::DiffError {
                    reason: "At least one of base or other input must be provided".to_string(),
                    ..Default::default()
                }),
            ));
        }

        // Log input IDs before consuming the input
        let mut scuba = ctx.scuba().clone();
        log_input_ids(&mut scuba, &request.input);

        // Build the policy once: it selects the omission path in
        // `resolve_diff_inputs` and the stamped verdict, so the two cannot
        // disagree.
        let policy = mononoke_api::RestrictedPathsPolicy::for_scs_request(&ctx, &request.repo_name);
        let resolved = self
            .resolve_diff_inputs(&ctx, &request.repo_name, request.input, &policy)
            .await?;

        let omitted_restricted_paths_count = policy.omitted_count();
        let denied = omitted_restricted_paths_count.unwrap_or(0);
        if denied > 0 {
            scuba.add("omitted_restricted_paths_count", denied);
        }
        policy.set_partial_if_omitted(&ctx);
        if denied > 0 && resolved.base.input().is_none() && resolved.other.input().is_none() {
            // Neither side visible: return the natural empty shape with
            // the partial verdict stamped.
            scuba.add("num_hunks", 0);
            scuba.add("files_identical", true);
            scuba.log_with_msg("Computed diff hunks", None);
            let response = thrift::DiffHunksResponse {
                num_hunks: 0,
                partial_info: crate::service::partial_response_info(&policy),
                ..Default::default()
            };
            return Ok((response, Box::pin(futures::stream::empty())));
        }

        let estimated_input_size = estimate_diff_size(&resolved, &ctx).await?;
        let _inflight_guard =
            try_reserve_memory(&ctx, "initial", estimated_input_size, &self.inflight_bytes)?;

        let (stats, hunks_result) = async {
            diff_feature::operations::hunks::hunks(
                &ctx,
                resolved.base_repo.repo(),
                resolved.other_repo.repo(),
                resolved.base.input().cloned(),
                resolved.other.input().cloned(),
                request.ignore_whitespace,
            )
            .await
        }
        .timed()
        .await;

        let hunks = hunks_result?;
        let num_hunks = hunks.len() as i64;

        // Log scuba metrics about the diff results
        scuba
            .add_future_stats(&stats)
            .add("num_hunks", num_hunks)
            .add("files_identical", num_hunks == 0);

        log_estimated_input_size(&mut scuba, estimated_input_size, &self.inflight_bytes);

        scuba.log_with_msg("Computed diff hunks", None);

        let hunk_stream = hunks.into_iter().map(|hunk| {
            let hunk_data = thrift::DiffHunksData {
                add_range: thrift::HunkRange {
                    start: hunk.add_range.start as i64,
                    end: hunk.add_range.end as i64,
                    ..Default::default()
                },
                remove_range: thrift::HunkRange {
                    start: hunk.delete_range.start as i64,
                    end: hunk.delete_range.end as i64,
                    ..Default::default()
                },
                ..Default::default()
            };
            Ok(hunk_data)
        });

        let response = thrift::DiffHunksResponse {
            num_hunks,
            partial_info: crate::service::partial_response_info(&policy),
            ..Default::default()
        };
        let stream = futures::stream::iter(hunk_stream);
        Ok((response, Box::pin(stream)))
    }
}

impl AddScubaParams for thrift::DiffHunksRequest {
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
