/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use context::CoreContext;
use source_control as thrift;

use crate::source_control_impl::SourceControlServiceImpl;

fn org_storage_not_supported(method: &str) -> scs_errors::ServiceError {
    scs_errors::not_implemented(format!(
        "{method} is not supported in OSS mode: storage engines are defined in config"
    ))
    .into()
}

impl SourceControlServiceImpl {
    pub(crate) async fn create_org_storage_config(
        &self,
        _ctx: CoreContext,
        _params: thrift::CreateOrgStorageConfigParams,
    ) -> Result<thrift::CreateOrgStorageConfigToken, scs_errors::ServiceError> {
        Err(org_storage_not_supported("create_org_storage_config"))
    }

    pub(crate) async fn delete_org_storage_config(
        &self,
        _ctx: CoreContext,
        _params: thrift::DeleteOrgStorageConfigParams,
    ) -> Result<thrift::DeleteOrgStorageConfigToken, scs_errors::ServiceError> {
        Err(org_storage_not_supported("delete_org_storage_config"))
    }

    pub(crate) async fn org_storage_config_poll(
        &self,
        _ctx: CoreContext,
        _params: thrift::OrgStorageConfigPollParams,
    ) -> Result<thrift::OrgStorageConfigPollResponse, scs_errors::ServiceError> {
        Err(org_storage_not_supported("org_storage_config_poll"))
    }

    pub(crate) async fn org_storage_config(
        &self,
        _ctx: CoreContext,
        _params: thrift::OrgStorageConfigParams,
    ) -> Result<thrift::OrgStorageConfigResponse, scs_errors::ServiceError> {
        Err(org_storage_not_supported("org_storage_config"))
    }
}
