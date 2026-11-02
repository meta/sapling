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
    pub(crate) async fn repo_stack_git_bundle_store(
        &self,
        _ctx: CoreContext,
        _repo: thrift::RepoSpecifier,
        _params: thrift::RepoStackGitBundleStoreParams,
    ) -> Result<thrift::RepoStackGitBundleStoreResponse, scs_errors::ServiceError> {
        Err(scs_errors::not_implemented(
            "repo_stack_git_bundle_store is not available in OSS builds".to_string(),
        )
        .into())
    }
}
