/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use context::CoreContext;
use diff as diff_feature;
use diff_service_if as thrift;
use futures_stats::TimedFutureExt;
use scuba_ext::MononokeScubaSampleBuilder;

use crate::errors::ServerError;
use crate::service::AddScubaParams;
use crate::service::DiffServiceImpl;
use crate::utils::admission::estimate_diff_size;
use crate::utils::admission::try_reserve_memory;
use crate::utils::diff_conversion::convert_metadata_diff_to_thrift;
use crate::utils::scuba::log_estimated_input_size;
use crate::utils::scuba::log_input_ids;
use crate::utils::scuba::log_single_input_scuba;

impl DiffServiceImpl {
    pub async fn metadata_diff(
        &self,
        ctx: CoreContext,
        request: thrift::MetadataDiffRequest,
    ) -> Result<thrift::MetadataDiffResponse, ServerError> {
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
            scuba.add("has_lines_count", false);
            scuba.log_with_msg("Computed metadata diff", None);
            return Ok(thrift::MetadataDiffResponse {
                base_file_info: thrift::DiffFileInfo::default(),
                other_file_info: thrift::DiffFileInfo::default(),
                lines_count: None,
                partial_info: crate::service::partial_response_info(&policy),
                ..Default::default()
            });
        }

        let estimated_input_size = estimate_diff_size(&resolved, &ctx).await?;
        // Keep the request's reservation through response logging as well as
        // retaining a share in CPU work that can outlive request cancellation.
        let inflight_guard = Arc::new(try_reserve_memory(
            &ctx,
            "initial",
            estimated_input_size,
            &self.inflight_bytes,
        )?);

        // Call the diff feature metadata operation
        let (stats, metadata_result) = async {
            diff_feature::operations::metadata::metadata_with_guard(
                &ctx,
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
                request.ignore_whitespace,
                inflight_guard.clone(),
            )
            .await
        }
        .timed()
        .await;

        let metadata_diff = metadata_result?;

        // Log scuba metrics
        scuba
            .add_future_stats(&stats)
            .add("has_lines_count", metadata_diff.lines_count.is_some());

        log_estimated_input_size(&mut scuba, estimated_input_size, &self.inflight_bytes);

        scuba.log_with_msg("Computed metadata diff", None);

        let mut response = convert_metadata_diff_to_thrift(metadata_diff);
        response.partial_info = crate::service::partial_response_info(&policy);
        Ok(response)
    }
}

impl AddScubaParams for thrift::MetadataDiffRequest {
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
