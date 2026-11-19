/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! The deployment that `config minimise` and `config convert` rewrite specs
//! of.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use cached_config::ConfigStore;
use metaconfig_parser::DeploymentLocation;
use metaconfig_parser::configerator_repo_spec_handle;
use metaconfig_parser::repo_services;
use mononoke_configs::MononokeConfigs;
use repos::Deployment;
use repos::DeploymentRepo;
use repos::MononokeService;
use repos::RawStorageConfig;
use repos::RepoSpec;

pub(super) struct DeploymentSpecs<'a> {
    pub(super) location: DeploymentLocation,
    pub(super) deployment: Arc<Deployment>,
    store: &'a ConfigStore,
}

impl<'a> DeploymentSpecs<'a> {
    /// The deployment `configs` loaded; `command` names the command for the
    /// error when there is none.
    pub(super) fn new(configs: &'a MononokeConfigs, command: &str) -> Result<Self> {
        let (Some((location, deployment)), Some(store)) =
            (configs.deployment(), configs.repo_config_store())
        else {
            bail!("config {command} needs a deployment, such as --deployment prod");
        };
        Ok(Self {
            location,
            deployment,
            store,
        })
    }

    /// The repos named, or every repo with `all`.
    pub(super) fn select(&self, names: &[String], all: bool) -> Result<Vec<&DeploymentRepo>> {
        let wanted: BTreeSet<&String> = names.iter().collect();
        let repos: Vec<&DeploymentRepo> = self
            .deployment
            .repos
            .iter()
            .filter(|repo| all || wanted.contains(&repo.repo_name))
            .collect();
        if let Some(missing) = wanted
            .iter()
            .find(|name| !repos.iter().any(|repo| &&repo.repo_name == *name))
        {
            bail!(
                "repo {missing} is not in the deployment at {}",
                self.location
            );
        }
        Ok(repos)
    }

    /// `repo`'s spec and its config path.
    pub(super) fn spec(&self, repo: &DeploymentRepo) -> Result<(String, Arc<RepoSpec>)> {
        let config_path = self.location.spec_config_path(&repo.spec_path)?;
        let spec = configerator_repo_spec_handle(&config_path, self.store)
            .with_context(|| format!("loading the spec of {}", repo.repo_name))?
            .get();
        Ok((config_path, spec))
    }

    /// The services that serve `repo`, each with its legacy tier.
    pub(super) fn services(&self, repo: &DeploymentRepo) -> Result<Vec<(MononokeService, String)>> {
        Ok(repo_services(&self.deployment, repo)?
            .into_iter()
            .filter_map(|service| {
                let config = self.deployment.services.get(&service)?;
                Some((service, config.legacy_tier.clone()))
            })
            .collect())
    }

    pub(super) fn storage(&self) -> HashMap<String, RawStorageConfig> {
        self.deployment.storage.clone().into_iter().collect()
    }
}
