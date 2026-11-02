/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::Path;
use std::path::PathBuf;

use remote_loader::get_remote_configs;
use remote_loader::read_local_config;
use remote_loader::should_fetch_configs;
use strum::EnumString;
use strum::IntoStaticStr;

use crate::facebook;
use crate::use_case::helpers;
use crate::use_case::remote_config_snapshot::REMOTE_CONFIG_SNAPSHOT;
use crate::use_case::thrift_types::scm_usecases_types::ScmUseCase;
use crate::use_case::thrift_types::scm_usecases_types::ScmUseCases;

// Used for fallback cases only.
const DEFAULT_ON_CALL: &str = "unknown";
const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 32;

const CONFIG_HASH_INDEX: &str = "scm_usecases_current_config_hash";
const CONFIG_UP_TO_DATE_INDEX: &str = "scm_usecases_uptodate";
const CONFIG_INDEX: &str = "scm_usecases_config";

#[derive(Clone, Copy, Debug, Eq, EnumString, IntoStaticStr, Hash, PartialEq)]
#[strum(serialize_all = "kebab_case")]
#[repr(u32)]
pub enum UseCaseId {
    #[strum(serialize = "buck2")]
    Buck2,
    Debugging,
    #[strum(serialize = "edensearch")]
    EdenSearch,
    #[strum(serialize = "edenfsctl")]
    EdenFsCtl,
    EdenFsTests,
    ExampleUseCase,
    Flow,
    Hack,
    IslServerNode,
    MeerkatCli,
    NestCli,
    NodeClient,
    ReloadCoordinator,
    ScmEdenContainer,
    RedirectFfi,
    #[strum(serialize = "testifyd")]
    TestifyDaemon,
    WatchActiveCommit,
    StarlarkMcp,
    #[strum(serialize = "vscode")]
    VSCode,
    #[strum(serialize = "vscode-buck")]
    VSCodeBuck,
    #[strum(serialize = "vscode-doctor")]
    VSCodeDoctor,
    #[strum(serialize = "vscode-eslint")]
    VSCodeEslint,
    #[strum(serialize = "vscode-filewatcher")]
    VSCodeFilewatcher,
    #[strum(serialize = "vscode-hg")]
    VSCodeHg,
    #[strum(serialize = "vscode-ios-component-kit")]
    VSCodeIOSComponentKit,
    #[strum(serialize = "vscode-tests")]
    VSCodeTests,
    #[strum(serialize = "vscode-update-tracker")]
    VSCodeUpdateTracker,
    #[strum(serialize = "vscode-extension")]
    VSCodeExtension,
    Unknown,
}

pub struct UseCase {
    config_dir: PathBuf,
    id: UseCaseId,
}

impl UseCase {
    pub fn new(config_dir: &Path, id: UseCaseId) -> Self {
        Self {
            config_dir: config_dir.to_path_buf(),
            id,
        }
    }

    pub fn id(&self) -> &UseCaseId {
        &self.id
    }

    pub fn name(&self) -> &'static str {
        self.id.into()
    }

    pub fn oncall(&self) -> String {
        self.get_use_case()
            .map_or(DEFAULT_ON_CALL.to_string(), |use_case| {
                use_case.oncall.clone()
            })
    }

    pub fn max_concurrent_requests(&self) -> usize {
        self.get_use_case()
            .map_or(DEFAULT_MAX_CONCURRENT_REQUESTS, |use_case| {
                // A 0 value is prevent in configerator, so we can assume that
                // try_into will never return 0.
                use_case
                    .config
                    .edenfs_limits
                    .max_concurrent_requests
                    .try_into()
                    .unwrap_or(DEFAULT_MAX_CONCURRENT_REQUESTS)
            })
    }

    fn get_use_case(&self) -> Option<ScmUseCase> {
        let is_pub = facebook::supports_vpnless();
        let config_url = helpers::config_url(is_pub);
        let proxy_url = facebook::proxy_url_http1();
        let proxy_sock_path: Option<&str> = if proxy_url.is_empty() {
            None
        } else {
            Some(&proxy_url)
        };
        let http_config = helpers::get_http_config(is_pub, proxy_sock_path).ok()?;
        let cache_path = self.config_dir.join("scm_use_cases");

        let local_config: Option<ScmUseCases> = read_local_config(&cache_path);

        let handle = self.maybe_update_config::<ScmUseCases>(
            is_pub,
            Some(config_url),
            300, // 5 minutes
            http_config,
            &cache_path,
            REMOTE_CONFIG_SNAPSHOT,
            CONFIG_HASH_INDEX,
            CONFIG_UP_TO_DATE_INDEX,
            CONFIG_INDEX,
        );

        // If no local configs is available, wait on the remote results.
        match local_config {
            Some(local_config) => local_config.use_cases.get(self.name()).cloned(),
            None => match handle {
                Some(handle) => match handle.join() {
                    Ok(remote_config) => remote_config.ok()?.use_cases.get(self.name()).cloned(),
                    Err(panic_msg) => {
                        tracing::error!("Thread panicked: {:?}", panic_msg);
                        None
                    }
                },
                None => {
                    // This shouldn't happen usually, this means that there was no local config, but also
                    // that the local config existed when trying to read the timestamp.
                    // Might happen if somebody deleted the local config between the two checks.
                    // Return None to read the default values
                    tracing::error!(
                        "Didn't find a local config, but also should_fetch_config returned False. Using default values."
                    );
                    None
                }
            },
        }
    }

    // Creates and detaches a thread that will update the config if it is stale
    fn maybe_update_config<C>(
        &self,
        is_pub: bool,
        remote_url: Option<String>,
        limit: u64,
        http_config: http_client::Config,
        cache_path: &Path,
        remote_config_snapshot: &str,
        config_hash_index: &str,
        config_up_to_date_index: &str,
        config_index: &str,
    ) -> Option<std::thread::JoinHandle<std::result::Result<C, anyhow::Error>>>
    where
        C: std::fmt::Debug + serde::de::DeserializeOwned + Send + 'static,
    {
        // Clone or convert all references to owned types so they can be moved into the thread
        let cache_path = cache_path.to_path_buf();
        let remote_config_snapshot = remote_config_snapshot.to_owned();
        let config_hash_index = config_hash_index.to_owned();
        let config_up_to_date_index = config_up_to_date_index.to_owned();
        let config_index = config_index.to_owned();

        if should_fetch_configs(limit, &cache_path) {
            return Some(std::thread::spawn(move || {
                get_remote_configs::<C>(
                    is_pub,
                    // Use-case fetching doesn't run on cloud environments, so we'll never add a
                    // service user to the x2p headers
                    false,
                    remote_url,
                    limit,
                    http_config,
                    &cache_path,
                    Some(&remote_config_snapshot),
                    &config_hash_index,
                    &config_up_to_date_index,
                    &config_index,
                )
            }));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn edensearch_round_trips_through_its_string() {
        assert_eq!(
            UseCaseId::from_str("edensearch").unwrap(),
            UseCaseId::EdenSearch
        );
        assert_eq!(<&'static str>::from(UseCaseId::EdenSearch), "edensearch");
    }

    #[test]
    fn nest_cli_round_trips_through_its_string() {
        assert_eq!(UseCaseId::from_str("nest-cli").unwrap(), UseCaseId::NestCli);
        assert_eq!(<&'static str>::from(UseCaseId::NestCli), "nest-cli");
    }
}
