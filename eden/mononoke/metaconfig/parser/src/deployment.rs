/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Deployments: everything one fleet of Mononoke jobs loads, seen by one
//! service, and where deployments are read from.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use cached_config::ConfigHandle;
use cached_config::ConfigStore;
use metaconfig_types::CommonConfig;
use repos::Deployment;
use repos::DeploymentRepo;
use repos::MononokeService;
use repos::RawCommonConfig;
use repos::RawStorageConfig;
use repos::TierManifest;
use repos::TierRepoEntry;

use crate::Presets;
use crate::config::StorageConfigs;
use crate::config::parse_common_and_storage;

/// The repos one service loads and the common and storage config they share:
/// a tier manifest, or one service's view of a deployment.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceManifest {
    /// The repos, each with the config path of its `RepoSpec` in the store the
    /// manifest was read from.
    pub repos: Vec<TierRepoEntry>,
    /// Common config for every repo.
    pub common: RawCommonConfig,
    /// Named storage configs, which repo configs refer to by name.
    pub storage: HashMap<String, RawStorageConfig>,
    /// Named presets, which repo configs refer to by name. Only a deployment
    /// has them.
    pub presets: Option<Presets>,
}

impl ServiceManifest {
    /// The parsed common and storage config.
    pub fn parse_common_and_storage(&self) -> Result<(CommonConfig, StorageConfigs)> {
        parse_common_and_storage(&self.common, &self.storage)
    }
}

impl From<TierManifest> for ServiceManifest {
    fn from(manifest: TierManifest) -> Self {
        Self {
            repos: manifest.repos,
            common: manifest.common,
            storage: manifest.storage,
            presets: None,
        }
    }
}

/// A handle on the deployment config at `path` in `config_store`.
pub fn deployment_config_handle(
    path: &str,
    config_store: &ConfigStore,
) -> Result<ConfigHandle<Deployment>> {
    config_store.get_config_handle::<Deployment>(path.to_owned())
}

/// One service's view of a deployment.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceView {
    /// The service.
    pub service: MononokeService,
    /// The repos that service serves, and the config they share.
    pub manifest: ServiceManifest,
    /// The tier whose `tier_overrides` resolve the service's repo configs.
    pub legacy_tier: String,
}

/// `deployment` as `service` sees it. Checks only what this service sees, so a
/// deployment can add a service before every binary knows it.
pub fn service_view(
    deployment: &Deployment,
    service: MononokeService,
    location: &DeploymentLocation,
) -> Result<ServiceView> {
    let Some(service_config) = deployment.services.get(&service) else {
        bail!("The deployment at {location} has no service {service}");
    };

    let mut common = deployment.common.clone();
    if let Some(category) = &service_config.loadlimiter_category {
        common.loadlimiter_category = Some(category.clone());
    }
    if let Some(allowlist) = &service_config.global_allowlist {
        common.global_allowlist = Some(allowlist.clone());
    }

    let mut names = HashSet::new();
    let mut ids = HashSet::new();
    let mut repos = Vec::new();
    for repo in &deployment.repos {
        let services = repo_services(deployment, repo)
            .with_context(|| format!("Invalid repo in the deployment at {location}"))?;
        if !services.contains(&service) {
            continue;
        }
        if !names.insert(repo.repo_name.as_str()) {
            bail!(
                "The deployment at {location} lists repo {} more than once",
                repo.repo_name
            );
        }
        if !ids.insert(repo.repo_id) {
            bail!(
                "The deployment at {location} gives repo id {} to more than one repo",
                repo.repo_id
            );
        }
        let config_path = location
            .spec_config_path(&repo.spec_path)
            .with_context(|| {
                format!(
                    "Invalid spec path for repo {} in the deployment at {location}",
                    repo.repo_name
                )
            })?;
        repos.push(TierRepoEntry {
            repo_name: repo.repo_name.clone(),
            repo_id: repo.repo_id,
            config_path,
            is_deep_sharded: repo.is_deep_sharded,
            enabled: repo.enabled,
            ..Default::default()
        });
    }
    Ok(ServiceView {
        service,
        manifest: ServiceManifest {
            repos,
            common,
            storage: deployment.storage.clone().into_iter().collect(),
            presets: deployment.presets.clone(),
        },
        legacy_tier: service_config.legacy_tier.clone(),
    })
}

/// The services that serve `repo`: those of its service groups, adjusted by
/// its own `services`.
pub fn repo_services(
    deployment: &Deployment,
    repo: &DeploymentRepo,
) -> Result<BTreeSet<MononokeService>> {
    let mut services = BTreeSet::new();
    for name in repo.service_groups.iter().flatten() {
        let Some(group) = deployment
            .service_groups
            .as_ref()
            .and_then(|groups| groups.get(name))
        else {
            bail!("Repo {} names unknown service group {name}", repo.repo_name);
        };
        services.extend(group.iter().copied());
    }
    for (service, serves) in repo.services.iter().flatten() {
        if *serves {
            services.insert(*service);
        } else {
            services.remove(service);
        }
    }
    Ok(services)
}

/// Where deployments and their repos' specs are read from: a config store,
/// and the prefix of their paths in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigRoot {
    /// The program's config store, configerator or `--local-configerator-path`,
    /// under `scm/mononoke/`.
    ConfigStore,
    /// A directory of `.json` files, laid out as `deployments/<name>.json` and
    /// `repos/<spec path>.json`.
    Directory(PathBuf),
}

impl ConfigRoot {
    /// The file extension of configs in a directory root.
    pub const DIRECTORY_EXTENSION: &'static str = ".json";

    fn prefix(&self) -> &'static str {
        match self {
            Self::ConfigStore => "scm/mononoke/",
            Self::Directory(_) => "",
        }
    }
}

/// A deployment, by name, in a config root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeploymentLocation {
    /// The store and path prefix.
    pub root: ConfigRoot,
    /// Such as `prod`.
    pub name: String,
}

impl DeploymentLocation {
    /// The deployment's path in its root's store.
    pub fn config_path(&self) -> String {
        format!("{}deployments/{}", self.root.prefix(), self.name)
    }

    /// The path in the same store of the spec a deployment names as
    /// `spec_path`.
    pub fn spec_config_path(&self, spec_path: &str) -> Result<String> {
        ensure!(
            !spec_path.is_empty()
                && spec_path
                    .split('/')
                    .all(|part| !matches!(part, "" | "." | "..")),
            "A spec path is a path within repos/, such as git/7e/acra, got '{spec_path}'"
        );
        Ok(format!("{}repos/{spec_path}", self.root.prefix()))
    }
}

impl fmt::Display for DeploymentLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.root {
            ConfigRoot::ConfigStore => write!(f, "{}", self.config_path()),
            ConfigRoot::Directory(dir) => write!(
                f,
                "{}/{}{}",
                dir.display(),
                self.config_path(),
                ConfigRoot::DIRECTORY_EXTENSION
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use maplit::btreemap;
    use mononoke_macros::mononoke;
    use repos::DeploymentRepo;
    use repos::RawAllowlistIdentity;
    use repos::ServiceConfig;

    use super::*;

    fn prod() -> DeploymentLocation {
        DeploymentLocation {
            root: ConfigRoot::ConfigStore,
            name: "prod".to_owned(),
        }
    }

    #[mononoke::test]
    fn test_paths() -> Result<()> {
        let prod = prod();
        assert_eq!(prod.config_path(), "scm/mononoke/deployments/prod");
        assert_eq!(
            prod.spec_config_path("git/7e/acra")?,
            "scm/mononoke/repos/git/7e/acra"
        );
        assert_eq!(prod.to_string(), "scm/mononoke/deployments/prod");

        let local = DeploymentLocation {
            root: ConfigRoot::Directory("/home/me/configs".into()),
            name: "test".to_owned(),
        };
        assert_eq!(local.config_path(), "deployments/test");
        assert_eq!(local.spec_config_path("git/7e/acra")?, "repos/git/7e/acra");
        assert_eq!(local.to_string(), "/home/me/configs/deployments/test.json");

        for invalid in [
            "",
            "/git/acra",
            "../tiers/scs_manifest",
            "git//acra",
            "git/./acra",
        ] {
            assert!(
                prod.spec_config_path(invalid).is_err(),
                "{invalid:?} should be refused"
            );
        }
        Ok(())
    }

    fn deployment() -> Deployment {
        let repo = |name: &str, id, services: Vec<MononokeService>| DeploymentRepo {
            repo_name: name.to_owned(),
            repo_id: id,
            spec_path: name.to_owned(),
            is_deep_sharded: id == 2,
            services: Some(
                services
                    .into_iter()
                    .map(|service| (service, true))
                    .collect(),
            ),
            ..Default::default()
        };
        Deployment {
            common: RawCommonConfig {
                trusted_parties_hipster_tier: Some("mononoke".to_owned()),
                ..Default::default()
            },
            storage: btreemap! {"blobstore".to_owned() => RawStorageConfig::default()},
            services: btreemap! {
                MononokeService::SCS_SERVER => ServiceConfig {
                    legacy_tier: "scs".to_owned(),
                    loadlimiter_category: Some("mononoke-apiserver".to_owned()),
                    ..Default::default()
                },
                MononokeService::GIT_SERVER => ServiceConfig {
                    legacy_tier: "gitimport_content".to_owned(),
                    global_allowlist: Some(vec![RawAllowlistIdentity {
                        identity_type: "SERVICE_IDENTITY".to_owned(),
                        identity_data: "scm_service_identity".to_owned(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
            },
            repos: vec![
                repo("hg", 1, vec![MononokeService::SCS_SERVER]),
                repo(
                    "git",
                    2,
                    vec![MononokeService::SCS_SERVER, MononokeService::GIT_SERVER],
                ),
            ],
            ..Default::default()
        }
    }

    #[mononoke::test]
    fn test_service_view_keeps_disabled_repos() -> Result<()> {
        let mut deployment = deployment();
        deployment.repos[0].enabled = Some(false);
        let scs = service_view(&deployment, MononokeService::SCS_SERVER, &prod())?;
        assert_eq!(
            scs.manifest
                .repos
                .iter()
                .map(|entry| (entry.repo_name.as_str(), entry.enabled))
                .collect::<Vec<_>>(),
            vec![("hg", Some(false)), ("git", None)],
            "a disabled repo stays in the view, marked disabled"
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_service_view() -> Result<()> {
        let mut deployment = deployment();
        deployment.presets = Some([("standard".to_owned(), Default::default())].into());
        let location = prod();

        let git = service_view(&deployment, MononokeService::GIT_SERVER, &location)?;
        assert_eq!(git.service, MononokeService::GIT_SERVER);
        assert_eq!(git.legacy_tier, "gitimport_content");
        assert_eq!(
            git.manifest.repos,
            vec![TierRepoEntry {
                repo_name: "git".to_owned(),
                repo_id: 2,
                config_path: "scm/mononoke/repos/git".to_owned(),
                is_deep_sharded: true,
                ..Default::default()
            }],
        );
        assert_eq!(
            git.manifest.storage,
            deployment.storage.clone().into_iter().collect()
        );
        assert_eq!(git.manifest.presets, deployment.presets);
        // The service's field replaces the deployment's; the rest is shared.
        assert_eq!(
            git.manifest.common.global_allowlist.as_ref().map(Vec::len),
            Some(1)
        );
        assert_eq!(git.manifest.common.loadlimiter_category, None);
        assert_eq!(
            git.manifest.common.trusted_parties_hipster_tier.as_deref(),
            Some("mononoke")
        );

        let scs = service_view(&deployment, MononokeService::SCS_SERVER, &location)?;
        assert_eq!(scs.service, MononokeService::SCS_SERVER);
        assert_eq!(scs.legacy_tier, "scs");
        assert_eq!(
            scs.manifest
                .repos
                .iter()
                .map(|repo| repo.repo_name.as_str())
                .collect::<Vec<_>>(),
            vec!["hg", "git"],
        );
        assert_eq!(
            scs.manifest.common.loadlimiter_category.as_deref(),
            Some("mononoke-apiserver")
        );
        assert_eq!(scs.manifest.common.global_allowlist, None);

        let error = service_view(&deployment, MononokeService::LFS_SERVER, &location)
            .expect_err("the deployment has no LFS server");
        assert!(
            error.to_string().contains("has no service LFS_SERVER"),
            "{error}"
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_service_view_checks_only_its_own_repos() {
        let location = prod();
        let mut deployment = deployment();
        // Another service's repo may repeat a name: this service never sees it.
        let mut other = deployment.repos[1].clone();
        other.services = Some(btreemap! {MononokeService::LFS_SERVER => true});
        deployment.repos.push(other);
        service_view(&deployment, MononokeService::GIT_SERVER, &location)
            .expect("the git server sees repo git once");
        service_view(&deployment, MononokeService::SCS_SERVER, &location)
            .expect("the SCS server sees repo git once");

        let mut deployment = self::deployment();
        let mut duplicate = deployment.repos[1].clone();
        duplicate.repo_name = "other".to_owned();
        duplicate.services = Some(btreemap! {MononokeService::GIT_SERVER => true});
        deployment.repos.push(duplicate);
        let error = service_view(&deployment, MononokeService::GIT_SERVER, &location)
            .expect_err("two git server repos with one id");
        assert!(error.to_string().contains("repo id 2"), "{error}");
        service_view(&deployment, MononokeService::SCS_SERVER, &location)
            .expect("the SCS server sees each repo once");

        let mut deployment = self::deployment();
        deployment.repos[0].spec_path = String::new();
        let error = service_view(&deployment, MononokeService::SCS_SERVER, &location)
            .expect_err("empty spec path");
        assert!(format!("{error:#}").contains("within repos/"), "{error:#}");
        service_view(&deployment, MononokeService::GIT_SERVER, &location)
            .expect("the git server does not see repo hg");
    }

    #[mononoke::test]
    fn test_service_groups() -> Result<()> {
        let mut deployment = deployment();
        deployment.service_groups = Some(btreemap! {
            "git".to_owned() => vec![MononokeService::GIT_SERVER],
        });
        // hg is served by its own service and by its group's.
        deployment.repos[0].service_groups = Some(vec!["git".to_owned()]);
        let git = service_view(&deployment, MononokeService::GIT_SERVER, &prod())?;
        assert_eq!(
            git.manifest
                .repos
                .iter()
                .map(|repo| repo.repo_name.as_str())
                .collect::<Vec<_>>(),
            vec!["hg", "git"],
        );
        let scs = service_view(&deployment, MononokeService::SCS_SERVER, &prod())?;
        assert_eq!(scs.manifest.repos.len(), 2);

        // A repo's own entry removes a service its groups give it.
        deployment.repos[1].service_groups = Some(vec!["git".to_owned()]);
        deployment.repos[1].services = Some(btreemap! {MononokeService::GIT_SERVER => false});
        let git = service_view(&deployment, MononokeService::GIT_SERVER, &prod())?;
        assert_eq!(git.manifest.repos.len(), 1);
        assert_eq!(git.manifest.repos[0].repo_name, "hg");

        deployment.repos[0].service_groups = Some(vec!["missing".to_owned()]);
        let error = service_view(&deployment, MononokeService::SCS_SERVER, &prod())
            .expect_err("a repo names an unknown group");
        assert!(
            format!("{error:#}").contains("unknown service group missing"),
            "{error:#}"
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_service_view_accepts_unknown_services() -> Result<()> {
        // A deployment that already configures a service this binary does not
        // know (99), with a repo that lists it.
        let deployment: Deployment = serde_json::from_value(serde_json::json!({
            "common": {},
            "storage": {},
            "services": {
                "3": {"legacy_tier": "scs"},
                "99": {"legacy_tier": "new_tier"},
            },
            "repos": [
                {"repo_name": "a", "repo_id": 1, "spec_path": "a",
                 "is_deep_sharded": false, "services": {"3": true, "99": true}},
                {"repo_name": "b", "repo_id": 2, "spec_path": "b",
                 "is_deep_sharded": false, "services": {"99": true}},
            ],
        }))?;
        let view = service_view(&deployment, MononokeService::SCS_SERVER, &prod())?;
        assert_eq!(view.manifest.repos.len(), 1);
        assert_eq!(view.manifest.repos[0].repo_name, "a");
        Ok(())
    }

    #[mononoke::test]
    fn test_service_view_from_a_directory() -> Result<()> {
        let view = service_view(
            &deployment(),
            MononokeService::GIT_SERVER,
            &DeploymentLocation {
                root: ConfigRoot::Directory("/home/me/configs".into()),
                name: "test".to_owned(),
            },
        )?;
        assert_eq!(view.manifest.repos[0].config_path, "repos/git");
        Ok(())
    }
}
