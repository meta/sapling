/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use edenfs_error::EdenFsError;
use edenfs_error::Result;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::time;
use tracing::Level;
use tracing::event;

use crate::client::Client;
use crate::client::EdenFsClient;
use crate::methods::EdenThriftMethod;
use crate::types::DaemonInfo;
use crate::types::Fb303Status;

pub trait DaemonHealthy {
    fn is_healthy(&self) -> bool;
}

impl DaemonHealthy for DaemonInfo {
    fn is_healthy(&self) -> bool {
        self.status
            .map_or_else(|| false, |val| val == Fb303Status::Alive)
    }
}

impl EdenFsClient {
    pub async fn get_health(&self, timeout: Option<Duration>) -> Result<DaemonInfo> {
        event!(Level::DEBUG, "connected to EdenFS daemon");
        self.with_thrift_with_timeouts(
            timeout.or_else(|| Some(Duration::from_secs(3))),
            None,
            |thrift| (thrift.getDaemonInfo(), EdenThriftMethod::GetDaemonInfo),
        )
        .await
        .with_context(|| "failed to get default eden daemon info")
        .map(|daemon_info| daemon_info.into())
        .map_err(EdenFsError::from)
    }

    /// The daemon's fb303 exported values whose names start with `build_`,
    /// such as `build_package_version` and `build_package_release`. Gives up
    /// after `timeout`, which is 3 seconds by default.
    pub async fn get_build_info(
        &self,
        timeout: Option<Duration>,
    ) -> Result<BTreeMap<String, String>> {
        let timeout = timeout.unwrap_or(Duration::from_secs(3));
        let build_info = self.with_thrift_with_timeouts(Some(timeout), None, |thrift| {
            (
                thrift.getRegexExportedValues("^build_.*"),
                EdenThriftMethod::GetRegexExportedValues,
            )
        });
        time::timeout(timeout, build_info)
            .await
            .map_err(EdenFsError::RequestTimeout)?
            .with_context(|| "failed to get EdenFS build info")
            .map_err(EdenFsError::from)
    }

    pub async fn get_health_with_startup_updates_included(
        &self,
        timeout: Duration,
    ) -> Result<(DaemonInfo, BoxStream<'static, Result<Vec<u8>>>)> {
        let (daemon_info, stream) = self
            .with_thrift_with_timeouts(Some(timeout), None, |thrift| {
                (
                    thrift.streamStartStatus(),
                    EdenThriftMethod::StreamStartStatus,
                )
            })
            .await
            .with_context(|| "failed to get start status stream")
            .map(|(daemon_info, stream)| (daemon_info.into(), stream))
            .map_err(EdenFsError::from)?;

        let stream = stream
            .map(|item| item.map_err(|e| EdenFsError::Other(e.into())))
            .boxed();

        Ok((daemon_info, stream))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use futures::FutureExt;
    use futures::future;

    use super::*;
    use crate::client::mock_client::make_boxed_future_result;
    use crate::client::mock_service::MockEdenFsService;
    use crate::use_case::UseCase;
    use crate::use_case::UseCaseId;
    use crate::utils::get_config_dir;

    fn client_of(mock_service: MockEdenFsService) -> anyhow::Result<EdenFsClient> {
        let config_dir = get_config_dir(&None, &None)?;
        let use_case = Arc::new(UseCase::new(&config_dir, UseCaseId::EdenFsTests));
        let mut client = EdenFsClient::new(use_case, PathBuf::new());
        client.set_thrift_service(Arc::new(mock_service));
        Ok(client)
    }

    #[fbinit::test]
    async fn test_get_build_info() -> anyhow::Result<()> {
        let mut mock_service = MockEdenFsService::new();
        let build_info =
            BTreeMap::from([("build_package_version".to_owned(), "20261104".to_owned())]);

        let returned = build_info.clone();
        mock_service
            .expect_getRegexExportedValues()
            .withf(|regex| regex == "^build_.*")
            .returning(move |_| make_boxed_future_result(Ok(returned.clone())));
        let client = client_of(mock_service)?;

        assert_eq!(client.get_build_info(None).await?, build_info);
        Ok(())
    }

    #[fbinit::test]
    async fn test_get_build_info_gives_up_on_a_silent_daemon() -> anyhow::Result<()> {
        let mut mock_service = MockEdenFsService::new();
        mock_service
            .expect_getRegexExportedValues()
            .returning(|_| future::pending().boxed());
        let client = client_of(mock_service)?;

        let result = client.get_build_info(Some(Duration::from_millis(10))).await;
        assert!(
            matches!(result, Err(EdenFsError::RequestTimeout(_))),
            "{result:?}"
        );
        Ok(())
    }
}
