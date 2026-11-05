/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::anyhow;
use async_runtime::block_on;
use configmodel::ConfigExt;
use eagerepo::EagerRepo;
use edenapi::Builder;
use edenapi::SaplingRemoteApi;
use edenapi::SaplingRemoteApiError;
use repourl::RepoUrl;
use storemodel::StoreInfo;
use try_once_lock::OnceLock;

const DEFAULT_CAPABILITIES: [&str; 1] = ["sapling-common"];

type Capabilities = HashSet<String>;

#[derive(Debug, Default, Clone)]
pub struct LazyCapabilities {
    caps: Arc<OnceLock<Capabilities>>,
}

impl LazyCapabilities {
    pub fn get(
        &self,
        eden_api: Arc<dyn SaplingRemoteApi>,
    ) -> Result<&Capabilities, SaplingRemoteApiError> {
        self.caps
            .get_or_try_init(|| block_on(eden_api.capabilities()).map(|c| c.into_iter().collect()))
    }
}

pub type OnceSlapi = OnceLock<(LazyCapabilities, Arc<dyn SaplingRemoteApi>)>;

/// Force construct the SaplingRemoteAPI client, caching the result in the provided OnceCell.
/// This bypasses checks about whether SaplingRemoteAPI should be used or not.
pub(crate) fn force_construct_eden_api(
    config: &dyn configmodel::Config,
    once_slapi: &OnceSlapi,
) -> Result<(LazyCapabilities, Arc<dyn SaplingRemoteApi>), SaplingRemoteApiError> {
    let (caps, eden_api) = once_slapi.get_or_try_init(
        || -> Result<(LazyCapabilities, Arc<dyn SaplingRemoteApi>), SaplingRemoteApiError> {
            tracing::trace!(target: "repo::eden_api", "creating edenapi");
            let eden_api = Builder::from_config(config)?.build()?;
            tracing::info!(url = eden_api.url(), "SaplingRemoteApi built");
            Ok((LazyCapabilities::default(), eden_api))
        },
    )?;
    Ok((caps.clone(), eden_api.clone()))
}

/// Constructs SaplingRemoteAPI client if it should be constructed and fetches its capabilities.
/// Returns `None` if SaplingRemoteAPI should not be used.
pub(crate) fn optional_eden_api_from_config(
    config: &dyn configmodel::Config,
    once_slapi: &OnceSlapi,
) -> Result<Option<(LazyCapabilities, Arc<dyn SaplingRemoteApi>)>, SaplingRemoteApiError> {
    if matches!(config.get_opt::<bool>("edenapi", "enable"), Ok(Some(false))) {
        tracing::trace!(target: "repo::eden_api", "disabled because edenapi.enable is false");
        return Ok(None);
    }
    match config.get_nonempty_opt::<RepoUrl>("paths", "default") {
        Err(err) => {
            tracing::warn!(target: "repo::eden_api", ?err, "disabled because error parsing paths.default");
            Ok(None)
        }
        Ok(None) => {
            tracing::trace!(target: "repo::eden_api", "disabled because paths.default is not set");
            Ok(None)
        }
        Ok(Some(path)) => {
            // EagerRepo URLs (test:, eager: file path, dummyssh).
            if EagerRepo::url_to_dir(&path).is_some() {
                tracing::trace!(target: "repo::eden_api", "using EagerRepo at {}", &path);
                let (caps, edenapi) = force_construct_eden_api(config, once_slapi)?;
                return Ok(Some((caps, edenapi)));
            }
            // Legacy tests are incompatible with SaplingRemoteAPI.
            // They use None or file or ssh scheme with dummyssh.
            if path.scheme() == "file" {
                tracing::trace!(target: "repo::eden_api", "disabled because paths.default is not set");
                return Ok(None);
            } else if path.scheme() == "ssh" {
                if let Some(ssh) = config.get("ui", "ssh") {
                    if ssh.contains("dummyssh") {
                        tracing::trace!(target: "repo::eden_api", "disabled because paths.default uses ssh scheme and dummyssh is in use");
                        return Ok(None);
                    }
                }
            }
            // Explicitly set SaplingRemoteAPI URLs.
            // Ideally we can make paths.default derive the edenapi URLs. But "push" is not on
            // SaplingRemoteAPI yet. So we have to wait.
            if config.get_nonempty("edenapi", "url").is_none()
                || config.get_nonempty("remotefilelog", "reponame").is_none()
            {
                tracing::trace!(target: "repo::eden_api", "disabled because edenapi.url or remotefilelog.reponame is not set");
                return Ok(None);
            }

            tracing::trace!(target: "repo::eden_api", "proceeding with path {}, reponame {:?}", path, config.get("remotefilelog", "reponame"));
            let (supported_capabilities, edenapi) = force_construct_eden_api(config, once_slapi)?;

            Ok(Some((supported_capabilities, edenapi)))
        }
    }
}

/// Constructs SaplingRemoteAPI client if it should be constructed and has the basic sapling capabilities.
/// Returns `None` if SaplingRemoteAPI should not be used or does not support the default capabilities.
pub(crate) fn get_optional_eden_api(
    info: &dyn StoreInfo,
    once_slapi: &OnceSlapi,
) -> Result<Option<Arc<dyn SaplingRemoteApi>>, SaplingRemoteApiError> {
    let config = info.config();

    // We know a priori that git repos (currently) never support the common facilities. This
    // avoids the eager "capabilities()" remote call.
    if info.has_requirement("git") && !info.has_requirement("remotefilelog") {
        return Ok(None);
    }

    if let Some((caps, edenapi)) = optional_eden_api_from_config(config, once_slapi)? {
        if info.has_requirement("remotefilelog") {
            // We know a priori that if we can construct a SLAPI client in a "remotefilelog"
            // repo, the client supports the "common" facilities. This avoids the eager
            // "capabilities()" remote call.
            return Ok(Some(edenapi));
        }

        let caps = caps.get(edenapi.clone())?;
        let supports_caps = DEFAULT_CAPABILITIES.iter().all(|&r| caps.contains(r));
        if !supports_caps
            && !config
                .must_get::<bool>("edenapi", "ignore-capabilities")
                .unwrap_or_default()
        {
            tracing::trace!(target: "repo::eden_api", "disabled because required capabilities {:?} are not supported within {:?}", DEFAULT_CAPABILITIES, caps);
            return Ok(None);
        }

        Ok(Some(edenapi))
    } else {
        Ok(None)
    }
}

/// Constructs the SaplingRemoteAPI client. Errors out if the SaplingRemoteAPI should not be
/// constructed.
pub(crate) fn get_eden_api(
    info: &dyn StoreInfo,
    once_slapi: &OnceSlapi,
) -> Result<Arc<dyn SaplingRemoteApi>, SaplingRemoteApiError> {
    match optional_eden_api_from_config(info.config(), once_slapi)? {
        Some((_, edenapi)) => Ok(edenapi),
        None => Err(SaplingRemoteApiError::Other(anyhow!(
            "SaplingRemoteAPI is requested but not available for this repo"
        ))),
    }
}

/// Constructs the SaplingRemoteAPI client. Errors out if the SaplingRemoteAPI should not be
/// constructed or doesn't meet the required capabilities.
pub(crate) fn get_eden_api_with_capabilities(
    info: &dyn StoreInfo,
    once_slapi: &OnceSlapi,
    capabilities: HashSet<String>,
) -> Result<Arc<dyn SaplingRemoteApi>, SaplingRemoteApiError> {
    let config = info.config();
    match optional_eden_api_from_config(config, once_slapi)? {
        Some((caps, edenapi)) => {
            if !config
                .must_get::<bool>("edenapi", "ignore-capabilities")
                .unwrap_or_default()
                && !capabilities.is_subset(caps.get(edenapi.clone())?)
            {
                return Err(SaplingRemoteApiError::Other(anyhow!(
                    "SaplingRemoteAPI is requested but capabilities {capabilities:?} are not supported within {caps:?}"
                )));
            }
            Ok(edenapi)
        }
        None => Err(SaplingRemoteApiError::Other(anyhow!(
            "SaplingRemoteAPI is not available"
        ))),
    }
}
