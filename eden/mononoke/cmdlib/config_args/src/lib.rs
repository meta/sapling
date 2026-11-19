/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use cached_config::ConfigStore;
use clap::ArgGroup;
use clap::Args;
use fbinit::FacebookInit;
use metaconfig_parser::ConfigRoot;
use metaconfig_parser::DeploymentLocation;
use mononoke_configs::MononokeConfigs;
use mononoke_configs::tier_manifest_path;
use repos::MononokeService;
use tokio::runtime::Handle;

#[derive(Args, Debug)]
#[clap(group(ArgGroup::new("config_source").args(&["config_tier", "prod", "git_config", "deployment"]).required(true)))]
#[clap(group(ArgGroup::new("local_configs").args(&["config", "local_configerator_path"]).multiple(true)))]
// A group, as clap counts `requires = "deployment"` as met when another config_source flag is given.
#[clap(group(ArgGroup::new("deployment_name").args(&["deployment"])))]
pub struct ConfigArgs {
    /// Use configerator-based configuration for a specific tier
    #[clap(long)]
    pub config_tier: Option<String>,

    /// Use configerator-based configuration for production
    #[clap(long)]
    pub prod: bool,

    /// Use configerator-based configuration for git repos
    #[clap(long)]
    pub git_config: bool,

    /// Load this deployment, such as `prod`: from configerator, or from the
    /// directory `--config` names. A name is letters, digits, `_` and `-`
    #[clap(long, value_parser = parse_deployment_name)]
    #[cfg_attr(not(fbcode_build), clap(requires = "local_configs"))]
    pub deployment: Option<String>,

    /// Read the deployment and its repos' specs from this directory of `.json`
    /// files, laid out as `deployments/<DEPLOYMENT>.json` and
    /// `repos/<SPEC PATH>.json`, instead of from configerator
    #[clap(long, value_name = "DIR", requires = "deployment_name", value_parser = parse_config_dir)]
    pub config: Option<PathBuf>,

    /// The service whose view of the deployment to load, such as SCS_SERVER.
    /// Defaults to the program's own service, or to COMMAND for a program
    /// that serves no single service
    #[clap(long, requires = "deployment_name")]
    pub service: Option<MononokeService>,

    /// Local path to fetch configerator configs from: JSON files laid out by
    /// config path, or a configerator checkout, whose materialized configs are
    /// read
    #[clap(long)]
    pub local_configerator_path: Option<PathBuf>,
}

fn parse_deployment_name(name: &str) -> Result<String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("'{name}' is not a deployment name: use letters, digits, '_' and '-'");
    }
    Ok(name.to_owned())
}

fn parse_config_dir(dir: &str) -> Result<PathBuf> {
    Ok(std::path::absolute(dir)?)
}

impl ConfigArgs {
    /// Where configs come from; clap guarantees one of the flags is set.
    pub fn source(&self) -> ConfigSource {
        if self.prod {
            ConfigSource::Tier("prod".to_owned())
        } else if self.git_config {
            ConfigSource::Tier("gitimport_content".to_owned())
        } else if let Some(tier) = &self.config_tier {
            ConfigSource::Tier(tier.clone())
        } else {
            let name = self
                .deployment
                .clone()
                .expect("clap requires one of --config-tier, --prod, --git-config or --deployment");
            let root = match &self.config {
                Some(dir) => ConfigRoot::Directory(dir.clone()),
                None => ConfigRoot::ConfigStore,
            };
            ConfigSource::Deployment(DeploymentLocation { root, name })
        }
    }

    /// Where configs come from, for display: the tier manifest's configerator
    /// path, or the deployment's location.
    pub fn config_path(&self) -> String {
        match self.source() {
            ConfigSource::Tier(tier) => tier_manifest_path(&tier),
            ConfigSource::Deployment(location) => location.to_string(),
        }
    }

    /// Production means live configerator; `--local-configerator-path` and
    /// `--config` are development setups.
    pub fn mode(&self) -> ConfigMode {
        if self.local_configerator_path.is_none() && self.config.is_none() {
            ConfigMode::Production
        } else {
            ConfigMode::Development
        }
    }

    pub fn create_config_store(&self, fb: FacebookInit) -> Result<ConfigStore> {
        const CONFIGERATOR_POLL_INTERVAL: Duration = Duration::from_secs(1);
        const CONFIGERATOR_REFRESH_TIMEOUT: Duration = Duration::from_secs(1);

        if let Some(path) = &self.local_configerator_path {
            Ok(local_config_store(path, CONFIGERATOR_POLL_INTERVAL))
        } else {
            ConfigStore::configerator(fb, true, true, CONFIGERATOR_REFRESH_TIMEOUT)
        }
    }

    /// The configs for this program. Without `--service`, a deployment is seen
    /// as `default_service`, or as `COMMAND` if that is `None`.
    pub fn create_mononoke_configs(
        &self,
        handle: Handle,
        config_store: &ConfigStore,
        default_service: Option<MononokeService>,
    ) -> Result<Arc<MononokeConfigs>> {
        let configs = match self.source() {
            ConfigSource::Tier(tier) => MononokeConfigs::new(&tier, config_store, handle)?,
            ConfigSource::Deployment(location) => MononokeConfigs::new_from_deployment(
                &location,
                self.service
                    .or(default_service)
                    .unwrap_or(MononokeService::COMMAND),
                config_store,
                handle,
            )?,
        };
        Ok(Arc::new(configs))
    }
}

/// Where a program's configs come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigSource {
    /// A tier's manifest in configerator.
    Tier(String),
    /// A deployment, seen as one service.
    Deployment(DeploymentLocation),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ConfigMode {
    Production,
    Development,
}

/// A config store that reads configs from `path`: JSON files laid out by config
/// path, or, if `path` is a configerator checkout, its materialized configs.
pub fn local_config_store(path: &Path, poll_interval: Duration) -> ConfigStore {
    let materialized = path.join("materialized_configs");
    if materialized.is_dir() {
        ConfigStore::file(
            true,
            materialized,
            ".materialized_JSON".to_owned(),
            poll_interval,
        )
    } else {
        ConfigStore::file(true, path.to_path_buf(), String::new(), poll_interval)
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use mononoke_macros::mononoke;

    use super::*;

    #[derive(Parser)]
    struct TestArgs {
        #[clap(flatten)]
        config: ConfigArgs,
    }

    fn parse(args: &[&str]) -> Result<ConfigArgs, clap::Error> {
        TestArgs::try_parse_from(std::iter::once("test").chain(args.iter().copied()))
            .map(|args| args.config)
    }

    #[mononoke::test]
    fn test_config_sources() {
        let tier = parse(&["--config-tier", "scs"]).expect("parses");
        assert_eq!(tier.source(), ConfigSource::Tier("scs".to_owned()));
        assert_eq!(tier.mode(), ConfigMode::Production);
        assert_eq!(
            parse(&["--git-config"]).expect("parses").source(),
            ConfigSource::Tier("gitimport_content".to_owned())
        );

        // OSS builds have no configerator, so there the deployment is read
        // from a local configerator path.
        #[cfg(fbcode_build)]
        let (deployment, mode) = (
            parse(&["--deployment", "prod", "--service", "GIT_SERVER"]).expect("parses"),
            ConfigMode::Production,
        );
        #[cfg(not(fbcode_build))]
        let (deployment, mode) = (
            parse(&[
                "--deployment",
                "prod",
                "--service",
                "GIT_SERVER",
                "--local-configerator-path",
                "/cfg",
            ])
            .expect("parses"),
            ConfigMode::Development,
        );
        assert_eq!(
            deployment.source(),
            ConfigSource::Deployment(DeploymentLocation {
                root: ConfigRoot::ConfigStore,
                name: "prod".to_owned(),
            })
        );
        assert_eq!(deployment.service, Some(MononokeService::GIT_SERVER));
        assert_eq!(deployment.mode(), mode);
        assert_eq!(deployment.config_path(), "scm/mononoke/deployments/prod");

        let local =
            parse(&["--config", "/home/me/configs", "--deployment", "test"]).expect("parses");
        assert_eq!(
            local.source(),
            ConfigSource::Deployment(DeploymentLocation {
                root: ConfigRoot::Directory("/home/me/configs".into()),
                name: "test".to_owned(),
            })
        );
        assert_eq!(local.mode(), ConfigMode::Development);
        assert_eq!(local.service, None);
    }

    #[mononoke::test]
    fn test_invalid_config_arguments() {
        for args in [
            &[][..],
            &["--config-tier", "scs", "--deployment", "prod"],
            // A directory holds deployments; it does not name one.
            &["--config", "/home/me/configs"],
            &["--config-tier", "scs", "--config", "/home/me/configs"],
            // A service only selects a view of a deployment.
            &["--config-tier", "scs", "--service", "SCS_SERVER"],
            &["--deployment", "prod", "--service", "NOT_A_SERVICE"],
            // A deployment name is not a path.
            &["--deployment", "../repos/tiers/scs_manifest"],
            &["--deployment", "prod/v2"],
            &["--deployment", ""],
        ] {
            assert!(parse(args).is_err(), "{args:?} should be refused");
        }
        #[cfg(not(fbcode_build))]
        assert!(
            parse(&["--deployment", "prod"]).is_err(),
            "without configerator a deployment needs --config or --local-configerator-path"
        );
    }
}
