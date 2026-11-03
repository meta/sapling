/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use permission_checker::AclProvider;

use crate::model::RepositoryRequestContext;

/// How a push should be processed with respect to RL Land Service diversion.
pub enum PushDiversionMode {
    /// Not a diverted repo — process through the normal git server path.
    NoDiversion,
    /// Diverted repo, normal flow — one synchronous `submit_manifest_land`
    /// to the Multi-Repo Land Service.
    #[cfg_attr(
        not(fbcode_build),
        expect(dead_code, reason = "Only internal builds divert pushes")
    )]
    RlLandServiceDiversion,
    /// Diverted repo, emergency push — process directly through the normal
    /// git server path. Stale manifest pins are repaired by the reconciler
    /// within its interval.
    #[cfg_attr(
        not(fbcode_build),
        expect(dead_code, reason = "Only internal builds divert pushes")
    )]
    EmergencyPush,
}

impl PushDiversionMode {
    pub async fn resolve(
        request_context: &RepositoryRequestContext,
        _acl_provider: &Arc<dyn AclProvider>,
    ) -> anyhow::Result<Self> {
        #[cfg(fbcode_build)]
        {
            if super::rl_land_service_diversion::should_divert_to_rl_land_service(request_context)?
            {
                return match super::rl_land_service_diversion::check_emergency_push(
                    _acl_provider,
                    request_context,
                )
                .await?
                {
                    super::rl_land_service_diversion::EmergencyPushStatus::NotRequested => {
                        Ok(Self::RlLandServiceDiversion)
                    }
                    super::rl_land_service_diversion::EmergencyPushStatus::Authorized => {
                        Ok(Self::EmergencyPush)
                    }
                };
            }
        }
        let _ = request_context;
        Ok(Self::NoDiversion)
    }
}
