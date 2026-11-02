/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use metadata::Metadata;
use mononoke_api::CoreContext;
use mononoke_app::MononokeApp;
use mononoke_configs::MononokeConfigs;
use mononoke_thrift_server::RequestContext;
use permission_checker::MononokeIdentity;
use repo_authorization::AuthorizationContext;
use scuba_ext::MononokeScubaSampleBuilder;

use crate::request_observability::RequestEvent;
use crate::request_observability::RequestOutcome;
use crate::request_observability::RequestType;
use crate::specifiers::SpecifierExt;

/// No-op: the fbcode implementation feeds an ods3 schematized logger, which
/// has no OSS counterpart. Scuba request logging is unaffected and still works
/// via `--scuba-log-file`.
pub(crate) fn observe_request(
    _method: &str,
    _repo: Option<String>,
    _request_type: RequestType,
    _event: RequestEvent,
    _outcome: Option<RequestOutcome>,
    _duration: Option<std::time::Duration>,
) {
}

#[derive(Clone, Default)]
pub(crate) struct RequestState;

impl RequestState {
    pub(crate) async fn new(_app: &MononokeApp) -> anyhow::Result<Self> {
        Ok(Self)
    }

    pub(crate) fn always_log(
        &self,
        _req_ctxt: &RequestContext,
    ) -> Result<bool, scs_errors::ServiceError> {
        Ok(false)
    }

    pub(crate) fn add_scuba_params(
        &self,
        _req_ctxt: &RequestContext,
        _specifier: Option<&dyn SpecifierExt>,
        _configs: &MononokeConfigs,
        _scuba: &mut MononokeScubaSampleBuilder,
    ) -> Result<(), scs_errors::ServiceError> {
        Ok(())
    }

    pub(crate) async fn create_metadata(
        &self,
        _req_ctxt: &RequestContext,
    ) -> Result<Metadata, scs_errors::ServiceError> {
        let client_info = clientinfo::ClientInfo::default_with_entry_point(
            clientinfo::ClientEntryPoint::ScsServer,
        );
        let identities = [MononokeIdentity::oss_local()].into_iter().collect();
        let mut metadata = Metadata::new(None, identities, false, false, None, None).await;
        metadata.add_client_info(client_info);
        Ok(metadata)
    }
}

/// The OSS transport cannot authenticate callers, so requests are not
/// subject to access control.
pub(crate) fn request_authorization_context(_ctx: &CoreContext) -> AuthorizationContext {
    AuthorizationContext::new_oss_local()
}

/// Service writes need an authenticated service caller, which the OSS
/// transport cannot provide, so requests naming a service are refused.
pub(crate) fn service_authorization_context(
    ctx: &CoreContext,
    service_name: Option<String>,
) -> Result<AuthorizationContext, scs_errors::ServiceError> {
    if service_name.is_some() {
        return Err(scs_errors::invalid_request(
            "service_identity is not accepted by OSS builds".to_string(),
        )
        .into());
    }
    Ok(request_authorization_context(ctx))
}

pub(crate) fn set_nocache_header(_req_ctxt: &RequestContext) {}

#[derive(Clone, Default)]
pub(crate) struct ThriftCachePolicy;

impl ThriftCachePolicy {
    pub(crate) fn new(_app: &MononokeApp) -> Self {
        Self
    }

    pub(crate) fn apply(
        &self,
        _ctx: &CoreContext,
        _repo: &mononoke_api::RepoContext<mononoke_api::Repo>,
    ) -> Result<(), scs_errors::ServiceError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::fbinit_test]
    fn requests_are_not_subject_to_access_control(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        assert_eq!(
            request_authorization_context(&ctx),
            AuthorizationContext::new_oss_local()
        );
        assert_eq!(
            service_authorization_context(&ctx, None)
                .expect("a request without a service identity should be accepted"),
            AuthorizationContext::new_oss_local()
        );
    }

    #[mononoke::fbinit_test]
    fn requests_naming_a_service_are_refused(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        assert!(service_authorization_context(&ctx, Some("some_service".to_string())).is_err());
    }

    #[mononoke::test]
    async fn requests_carry_the_oss_local_identity() {
        let Ok(metadata) = RequestState.create_metadata(&RequestContext::new()).await else {
            panic!("OSS request metadata should be created");
        };
        assert_eq!(
            metadata.identities().iter().cloned().collect::<Vec<_>>(),
            vec![MononokeIdentity::oss_local()]
        );
    }
}
