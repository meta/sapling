/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use context::CoreContext;
use metadata::Metadata;
use mononoke_app::MononokeApp;
use mononoke_thrift_server::RequestContext;
use mononoke_thrift_server::ThriftServerBuilder;
use permission_checker::MononokeIdentity;
use repo_authorization::AuthorizationContext;
use scuba_ext::MononokeScubaSampleBuilder;

use crate::errors::ServerError;

#[derive(Clone)]
pub(crate) struct ServiceState;

impl ServiceState {
    pub(crate) async fn new(_app: &MononokeApp) -> anyhow::Result<Self> {
        Ok(Self)
    }

    pub(crate) fn add_scuba_params(
        &self,
        _req_ctxt: &RequestContext,
        _scuba: &mut MononokeScubaSampleBuilder,
    ) -> Result<(), ServerError> {
        Ok(())
    }

    pub(crate) async fn create_metadata(
        &self,
        _req_ctxt: &RequestContext,
    ) -> Result<Metadata, ServerError> {
        let identities = [MononokeIdentity::oss_local()].into_iter().collect();
        Ok(Metadata::new(None, identities, false, false, None, None).await)
    }
}

/// The OSS transport cannot authenticate callers, so requests are not
/// subject to access control.
pub(crate) fn authorization_context(_ctx: &CoreContext) -> AuthorizationContext {
    AuthorizationContext::new_oss_local()
}

pub(crate) fn set_nocache_header(_req_ctxt: &RequestContext) {}

pub(crate) fn configure_server(
    builder: ThriftServerBuilder,
    _app: &MononokeApp,
) -> ThriftServerBuilder {
    builder
}

pub(crate) struct ServerFactory;

impl ServerFactory {
    pub(crate) async fn new(
        _fb: fbinit::FacebookInit,
        _service_name: &'static str,
    ) -> anyhow::Result<Self> {
        Ok(Self)
    }

    pub(crate) fn executor(&self) {}

    pub(crate) async fn shutdown(self) {}
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
            authorization_context(&ctx),
            AuthorizationContext::new_oss_local()
        );
    }

    #[mononoke::test]
    async fn requests_carry_the_oss_local_identity() -> anyhow::Result<()> {
        let req_ctxt = RequestContext::new();
        let metadata = ServiceState.create_metadata(&req_ctxt).await?;
        assert_eq!(
            metadata.identities().iter().cloned().collect::<Vec<_>>(),
            vec![MononokeIdentity::oss_local()]
        );
        Ok(())
    }
}
