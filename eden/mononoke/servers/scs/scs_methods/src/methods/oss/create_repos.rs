/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use context::CoreContext;
use source_control as thrift;

use crate::source_control_impl::SourceControlServiceImpl;

impl SourceControlServiceImpl {
    pub(crate) async fn create_repos(
        &self,
        _ctx: CoreContext,
        _params: thrift::CreateReposParams,
    ) -> Result<thrift::CreateReposToken, scs_errors::ServiceError> {
        Err(scs_errors::not_implemented(
            "create_repos is not supported in OSS mode: repos are defined in config, not created on the fly"
                .to_string(),
        )
        .into())
    }

    pub(crate) async fn create_repos_poll(
        &self,
        _ctx: CoreContext,
        _token: thrift::CreateReposToken,
    ) -> Result<thrift::CreateReposPollResponse, scs_errors::ServiceError> {
        Err(
            scs_errors::not_implemented("create_repos is not supported in OSS mode".to_string())
                .into(),
        )
    }
}
