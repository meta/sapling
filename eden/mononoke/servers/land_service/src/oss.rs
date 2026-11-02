/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use clap::Args;
use context::CoreContext;
use metadata::Metadata;
use mononoke_app::MononokeApp;
use mononoke_thrift_server::RequestContext;
use mononoke_thrift_server::ThriftServerBuilder;
use permission_checker::MononokeIdentity;
use repo_authorization::AuthorizationContext;
use scuba_ext::MononokeScubaSampleBuilder;

use crate::errors::LandChangesetsError;

#[derive(Debug, Args)]
pub(crate) struct ServerArgs {}

#[derive(Clone)]
pub(crate) struct ServiceState;

impl ServiceState {
    pub(crate) async fn new(_app: &MononokeApp, _args: &ServerArgs) -> anyhow::Result<Self> {
        Ok(Self)
    }

    pub(crate) async fn caller_admission(
        &self,
        _ctx: &CoreContext,
    ) -> Result<Option<&'static str>, LandChangesetsError> {
        Ok(None)
    }

    pub(crate) fn add_scuba_params(
        &self,
        _req_ctxt: &RequestContext,
        _scuba: &mut MononokeScubaSampleBuilder,
    ) -> Result<(), LandChangesetsError> {
        Ok(())
    }

    pub(crate) async fn create_metadata(
        &self,
        _req_ctxt: &RequestContext,
    ) -> Result<Metadata, LandChangesetsError> {
        let identities = [MononokeIdentity::oss_local()].into_iter().collect();
        Ok(Metadata::new(None, identities, false, false, None, None).await)
    }
}

pub(crate) fn configure_server(
    builder: ThriftServerBuilder,
    _app: &MononokeApp,
) -> ThriftServerBuilder {
    builder
}

/// The OSS transport cannot authenticate callers, so requests are not
/// subject to access control, and requests that name a service identity for
/// service writes are refused.
pub(crate) fn authorization_context(
    _ctx: &CoreContext,
    service_identity: Option<String>,
) -> Result<AuthorizationContext, LandChangesetsError> {
    if service_identity.is_some() {
        return Err(LandChangesetsError::AuthorizationError(
            "service_identity is not accepted by OSS builds".to_string(),
        ));
    }
    Ok(AuthorizationContext::new_oss_local())
}

#[cfg(test)]
mod tests {
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::fbinit_test]
    fn requests_are_not_subject_to_access_control(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let Ok(authz) = authorization_context(&ctx, None) else {
            panic!("a request without a service identity should be accepted");
        };
        assert_eq!(authz, AuthorizationContext::new_oss_local());
    }

    #[mononoke::fbinit_test]
    fn requests_naming_a_service_are_refused(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        assert!(authorization_context(&ctx, Some("some_service".to_string())).is_err());
    }

    #[mononoke::test]
    async fn requests_carry_the_oss_local_identity() {
        let Ok(metadata) = ServiceState.create_metadata(&RequestContext::new()).await else {
            panic!("OSS request metadata should be created");
        };
        assert_eq!(
            metadata.identities().iter().cloned().collect::<Vec<_>>(),
            vec![MononokeIdentity::oss_local()]
        );
    }
}
