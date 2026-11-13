/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroU64;
use std::str::FromStr;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use bookmarks_types::BookmarkKey;
use metaconfig_types::AclManifestMode;
use metaconfig_types::Address;
use metaconfig_types::BlameVersion;
use metaconfig_types::BookmarkOrRegex;
use metaconfig_types::BookmarkParams;
use metaconfig_types::CacheWarmupParams;
use metaconfig_types::CommitCloudConfig;
use metaconfig_types::CommitGraphConfig;
use metaconfig_types::CommitIdentityScheme;
use metaconfig_types::CommitRateLimitCacheConfig;
use metaconfig_types::CommitRateLimitConfig;
use metaconfig_types::CommitRateLimitEligibilityCheck;
use metaconfig_types::CommitRateLimitRuleConfig;
use metaconfig_types::CommitRateLimitWindow;
use metaconfig_types::ComparableRegex;
use metaconfig_types::CrossRepoCommitValidation;
use metaconfig_types::DerivationPipelineConfig;
use metaconfig_types::DerivationPipelineStageConfig;
use metaconfig_types::DerivedDataConfig;
use metaconfig_types::DerivedDataTypesConfig;
use metaconfig_types::DirectoryBranchClusterConfig;
use metaconfig_types::DirectoryBranchClusterFixedCluster;
use metaconfig_types::DirectoryBranchClusterFixedConfig;
use metaconfig_types::EnforcementConditionSet;
use metaconfig_types::EnforcementExemptionSet;
use metaconfig_types::GitBundleUriConfig;
use metaconfig_types::GitConcurrencyParams;
use metaconfig_types::GitConfigs;
use metaconfig_types::GitDeltaManifestV2Config;
use metaconfig_types::GitDeltaManifestV3Config;
use metaconfig_types::GitDeltaManifestVersion;
use metaconfig_types::GlobalrevConfig;
use metaconfig_types::HookBypassActor;
use metaconfig_types::HookBypassCondition;
use metaconfig_types::HookConfig;
use metaconfig_types::HookManagerParams;
use metaconfig_types::HookParams;
use metaconfig_types::InferredCopyFromConfig;
use metaconfig_types::InfinitepushNamespace;
use metaconfig_types::InfinitepushParams;
use metaconfig_types::LazyLoadingConfig;
use metaconfig_types::LfsParams;
use metaconfig_types::LoggingDestination;
use metaconfig_types::MergeResolutionOverride;
use metaconfig_types::MetadataCacheConfig;
use metaconfig_types::MetadataCacheUpdateMode;
use metaconfig_types::MetadataLoggerConfig;
use metaconfig_types::ModernSyncChannelConfig;
use metaconfig_types::ModernSyncConfig;
use metaconfig_types::MononokeCasSyncConfig;
use metaconfig_types::P4SourceConfig;
use metaconfig_types::PathRestrictionMetadata;
use metaconfig_types::PushParams;
use metaconfig_types::PushrebaseFlags;
use metaconfig_types::PushrebaseParams;
use metaconfig_types::PushrebaseRemoteMode;
use metaconfig_types::RemoteDerivationConfig;
use metaconfig_types::RemoteDiffConfig;
use metaconfig_types::RequestMatchers;
use metaconfig_types::RestrictedPathsConfig;
use metaconfig_types::RestrictedPathsManifestIdStoreConfig;
use metaconfig_types::ServiceWriteRestrictions;
use metaconfig_types::ShardedService;
use metaconfig_types::ShardingModeConfig;
use metaconfig_types::SoftRestrictedPathConfig;
use metaconfig_types::SourceControlServiceMonitoring;
use metaconfig_types::SourceControlServiceParams;
use metaconfig_types::SparseProfilesConfig;
use metaconfig_types::UnodeVersion;
use metaconfig_types::UpdateLoggingConfig;
use metaconfig_types::UriGeneratorType;
use metaconfig_types::WalkerConfig;
use metaconfig_types::WalkerJobParams;
use metaconfig_types::WalkerJobType;
use metaconfig_types::XRepoSyncSourceConfig;
use metaconfig_types::XRepoSyncSourceConfigMapping;
use metaconfig_types::ZelosConfig;
use metaconfig_types::parse_bare_group_name;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::NonRootMPath;
use mononoke_types::PrefixTrie;
use mononoke_types::RepositoryId;
use mononoke_types::path::MPath;
use permission_checker::MononokeIdentity;
use repos::ModernSyncChannelConfig as RawModernSyncChannelConfig;
use repos::RawBookmarkConfig;
use repos::RawCacheWarmupConfig;
use repos::RawCasSyncConfig;
use repos::RawCommitCloudConfig;
use repos::RawCommitGraphConfig;
use repos::RawCommitIdentityScheme;
use repos::RawCommitRateLimitConfig;
use repos::RawCrossRepoCommitValidationConfig;
use repos::RawDerivationPipelineConfig;
use repos::RawDerivationPipelineStageConfig;
use repos::RawDerivedDataBlockedChangesetDerivation;
use repos::RawDerivedDataBlockedDerivation;
use repos::RawDerivedDataConfig;
use repos::RawDerivedDataTypesConfig;
use repos::RawDirectoryBranchClusterConfig;
use repos::RawDirectoryBranchClusterFixedCluster;
use repos::RawDirectoryBranchClusterFixedConfig;
use repos::RawEligibilityCheck;
use repos::RawEnforcementConditionSet;
use repos::RawGitBundleURIConfig;
use repos::RawGitConcurrencyParams;
use repos::RawGitConfigs;
use repos::RawGitDeltaManifestV2Config;
use repos::RawGitDeltaManifestV3Config;
use repos::RawHookBypassActor;
use repos::RawHookBypassCondition;
use repos::RawHookConfig;
use repos::RawHookManagerParams;
use repos::RawInferredCopyFromConfig;
use repos::RawInfinitepushParams;
use repos::RawLazyLoadingConfig;
use repos::RawLfsParams;
use repos::RawLoggingDestination;
use repos::RawLoggingDestinationScribe;
use repos::RawMetadataCacheConfig;
use repos::RawMetadataCacheUpdateMode;
use repos::RawMetadataLoggerConfig;
use repos::RawModernSyncConfig;
use repos::RawP4SourceConfig;
use repos::RawPathRestrictionMetadata;
use repos::RawPushParams;
use repos::RawPushrebaseParams;
use repos::RawPushrebaseRemoteMode;
use repos::RawPushrebaseRemoteModeRemote;
use repos::RawRemoteDerivationConfig;
use repos::RawRemoteDiffConfig;
use repos::RawRestrictedPathsConfig;
use repos::RawRestrictedPathsManifestIdStoreConfig;
use repos::RawServiceWriteRestrictions;
use repos::RawShardedService;
use repos::RawShardingModeConfig;
use repos::RawSoftRestrictedPathConfig;
use repos::RawSourceControlServiceMonitoring;
use repos::RawSourceControlServiceParams;
use repos::RawSparseProfilesConfig;
use repos::RawUpdateLoggingConfig;
use repos::RawUriGeneratorType;
use repos::RawWalkerConfig;
use repos::RawWalkerJobParams;
use repos::RawWalkerJobType;
use repos::RawXRepoSyncSourceConfig;
use repos::RawXRepoSyncSourceConfigMapping;
use repos::RawZelosConfig;

use crate::convert::Convert;
use crate::errors::ConfigurationError;

#[cfg(test)]
mod metadata_cache_tests;

impl Convert for RawCacheWarmupConfig {
    type Output = CacheWarmupParams;

    fn convert(self) -> Result<Self::Output> {
        Ok(CacheWarmupParams {
            bookmark: BookmarkKey::new(self.bookmark)?,
            commit_limit: self
                .commit_limit
                .map(|v| v.try_into())
                .transpose()?
                .unwrap_or(200000),
            microwave_preload: self.microwave_preload.unwrap_or(false),
        })
    }
}

impl Convert for RawHookManagerParams {
    type Output = HookManagerParams;

    fn convert(self) -> Result<Self::Output> {
        Ok(HookManagerParams {
            disable_acl_checker: self.disable_acl_checker,
            all_hooks_bypassed: self.all_hooks_bypassed,
            bypassed_commits_scuba_table: self.bypassed_commits_scuba_table,
        })
    }
}

fn parse_pushvar(pushvar: String) -> Result<(String, String)> {
    match pushvar.split('=').collect::<Vec<_>>().as_slice() {
        [name, value] => Ok((name.to_string(), value.to_string())),
        _ => Err(ConfigurationError::InvalidPushvar(pushvar).into()),
    }
}

/// The bypass conditions equivalent to a hook's `bypass_*` fields: the pushvar
/// and the commit message marker each bypass, and with
/// `bypass_implicit_for_group` so does membership of the permission group
/// alone. The permission group, when set, restricts every condition to its
/// members and to humans unless `allow_agent_bypass` is set. Without a group
/// anyone, including agents, may use the markers.
fn legacy_bypass_conditions(
    commit_message: Option<String>,
    pushvar: Option<String>,
    permission_group: Option<String>,
    implicit_for_group: bool,
    allow_agent: bool,
) -> Result<Vec<HookBypassCondition>> {
    let actor = if permission_group.is_none() || allow_agent {
        HookBypassActor::Any
    } else {
        HookBypassActor::Human
    };
    let restrict = |condition: HookBypassCondition| HookBypassCondition {
        permission_group: permission_group.clone(),
        actor,
        ..condition
    };
    let mut conditions = Vec::new();
    if let Some(pushvar) = pushvar {
        let (name, value) = parse_pushvar(pushvar)?;
        conditions.push(restrict(HookBypassCondition::pushvar(name, value)));
    }
    if let Some(marker) = commit_message {
        conditions.push(restrict(HookBypassCondition::commit_message(marker)));
    }
    // Without a group the flag is inert rather than a load error, so a config
    // mistake cannot stop the repo from initialising.
    if implicit_for_group && permission_group.is_some() {
        conditions.push(restrict(HookBypassCondition::default()));
    }
    Ok(conditions)
}

impl Convert for RawHookBypassCondition {
    type Output = HookBypassCondition;

    fn convert(self) -> Result<Self::Output> {
        let actor = match self.actor {
            None | Some(RawHookBypassActor::HUMAN) => HookBypassActor::Human,
            Some(RawHookBypassActor::AGENT) => HookBypassActor::Agent,
            Some(RawHookBypassActor::ANY) => HookBypassActor::Any,
            Some(v) => return Err(anyhow!("Invalid value {v} for enum HookBypassActor")),
        };
        let condition = HookBypassCondition {
            commit_message: self.commit_message,
            pushvar: self.pushvar.map(parse_pushvar).transpose()?,
            permission_group: self.permission_group,
            identities: self
                .identities
                .unwrap_or_default()
                .iter()
                .map(|identity| MononokeIdentity::from_str(identity))
                .collect::<Result<_>>()?,
            actor,
        };
        ensure!(
            !condition.is_empty(),
            "A bypass condition must set at least one of commit_message, pushvar, \
             permission_group and identities"
        );
        Ok(condition)
    }
}

impl Convert for RawHookConfig {
    type Output = HookParams;

    fn convert(self) -> Result<Self::Output> {
        let bypasses = match self.bypasses {
            Some(conditions) => {
                ensure!(
                    self.bypass_commit_string.is_none()
                        && self.bypass_pushvar.is_none()
                        && self.bypass_permission_group.is_none()
                        && self.bypass_implicit_for_group.is_none()
                        && self.allow_agent_bypass.is_none(),
                    "Hook {} sets bypasses, so it must not set bypass_commit_string, \
                     bypass_pushvar, bypass_permission_group, bypass_implicit_for_group or \
                     allow_agent_bypass",
                    self.name,
                );
                conditions
                    .into_iter()
                    .enumerate()
                    .map(|(index, condition)| {
                        condition.convert().with_context(|| {
                            format!("Invalid bypass condition {index} for hook {}", self.name)
                        })
                    })
                    .collect::<Result<_>>()?
            }
            None => legacy_bypass_conditions(
                self.bypass_commit_string,
                self.bypass_pushvar,
                self.bypass_permission_group,
                self.bypass_implicit_for_group.unwrap_or_default(),
                self.allow_agent_bypass.unwrap_or_default(),
            )?,
        };

        let config = HookConfig {
            bypasses,
            options: self.config_json,
            log_only: self.log_only.unwrap_or_default(),
            name: self.name.clone(),
            messages: self.messages.unwrap_or_default(),
            strings: self.config_strings.unwrap_or_default(),
            ints: self.config_ints.unwrap_or_default(),
            ints_64: self.config_ints_64.unwrap_or_default(),
            string_lists: self.config_string_lists.unwrap_or_default(),
            int_lists: self.config_int_lists.unwrap_or_default(),
            int_64_lists: self.config_int_64_lists.unwrap_or_default(),
        };

        Ok(HookParams {
            implementation: self.implementation.unwrap_or_else(|| self.name.clone()),
            name: self.name,
            config,
        })
    }
}

impl Convert for RawBookmarkConfig {
    type Output = BookmarkParams;

    fn convert(self) -> Result<Self::Output> {
        let bookmark_or_regex = match (self.regex, self.name, self.inverse_regex) {
            (None, Some(name), None) => BookmarkOrRegex::Bookmark(BookmarkKey::new(name).unwrap()),
            (Some(regex), None, None) => match ComparableRegex::new(regex) {
                Ok(comparable_regex) => BookmarkOrRegex::Regex(comparable_regex),
                Err(err) => {
                    return Err(ConfigurationError::InvalidConfig(format!(
                        "invalid bookmark regex: {err}"
                    ))
                    .into());
                }
            },
            (None, None, Some(inverse_regex)) => match ComparableRegex::new(inverse_regex) {
                Ok(comparable_regex) => BookmarkOrRegex::InverseRegex(comparable_regex),
                Err(err) => {
                    return Err(ConfigurationError::InvalidConfig(format!(
                        "invalid bookmark inverse regex: {err}"
                    ))
                    .into());
                }
            },
            _ => {
                return Err(ConfigurationError::InvalidConfig(
                    "bookmark's params need to specify exactly one of: name, regex, or inverse_regex".into(),
                )
                .into());
            }
        };

        let hooks = self.hooks.into_iter().map(|rbmh| rbmh.hook_name).collect();
        let only_fast_forward = self.only_fast_forward;
        let allowed_users = self.allowed_users.map(ComparableRegex::new).transpose()?;
        let allowed_hipster_group = self.allowed_hipster_group;
        let rewrite_dates = self.rewrite_dates;
        let hooks_skip_ancestors_of = self
            .hooks_skip_ancestors_of
            .unwrap_or_default()
            .into_iter()
            .map(BookmarkKey::new)
            .collect::<Result<Vec<_>, _>>()?;
        let ensure_ancestor_of = self.ensure_ancestor_of.map(BookmarkKey::new).transpose()?;

        Ok(BookmarkParams {
            bookmark: bookmark_or_regex,
            hooks,
            only_fast_forward,
            allowed_users,
            allowed_hipster_group,
            rewrite_dates,
            hooks_skip_ancestors_of,
            ensure_ancestor_of,
        })
    }
}

impl Convert for RawPushParams {
    type Output = PushParams;

    fn convert(self) -> Result<Self::Output> {
        let default = PushParams::default();
        Ok(PushParams {
            pure_push_allowed: self.pure_push_allowed.unwrap_or(default.pure_push_allowed),
            unbundle_commit_limit: self
                .unbundle_commit_limit
                .map(|limit| limit.try_into())
                .transpose()?,
        })
    }
}

impl Convert for RawCommitIdentityScheme {
    type Output = CommitIdentityScheme;

    fn convert(self) -> Result<Self::Output> {
        let converted = match self {
            RawCommitIdentityScheme::HG => CommitIdentityScheme::HG,
            RawCommitIdentityScheme::GIT => CommitIdentityScheme::GIT,
            RawCommitIdentityScheme::BONSAI => CommitIdentityScheme::BONSAI,
            RawCommitIdentityScheme::P4 => CommitIdentityScheme::P4,
            RawCommitIdentityScheme::UNKNOWN => CommitIdentityScheme::UNKNOWN,
            v => return Err(anyhow!("Invalid value {v} for enum CommitIdentityScheme")),
        };
        Ok(converted)
    }
}

impl Convert for RawPushrebaseRemoteModeRemote {
    type Output = Address;

    fn convert(self) -> Result<Self::Output> {
        match self {
            Self::tier(t) => Ok(Address::Tier(t)),
            Self::host_port(host) => Ok(Address::HostPort(host)),
            Self::UnknownField(e) => anyhow::bail!("Unknown field: {e}"),
        }
    }
}

impl Convert for RawPushrebaseRemoteMode {
    type Output = PushrebaseRemoteMode;

    fn convert(self) -> Result<Self::Output> {
        match self {
            Self::local(_) => Ok(PushrebaseRemoteMode::Local),
            Self::remote_land_service(addr) => {
                Ok(PushrebaseRemoteMode::RemoteLandService(addr.convert()?))
            }
            Self::remote_land_service_local_fallback(addr) => Ok(
                PushrebaseRemoteMode::RemoteLandServiceWithLocalFallback(addr.convert()?),
            ),
            Self::UnknownField(e) => anyhow::bail!("Unknown field: {e}"),
        }
    }
}

impl Convert for RawPushrebaseParams {
    type Output = PushrebaseParams;

    fn convert(self) -> Result<Self::Output> {
        let default = PushrebaseParams::default();
        Ok(PushrebaseParams {
            flags: PushrebaseFlags {
                rewritedates: self.rewritedates.unwrap_or(default.flags.rewritedates),
                recursion_limit: self
                    .recursion_limit
                    .map(|v| v.try_into())
                    .transpose()?
                    .or(default.flags.recursion_limit),
                forbid_p2_root_rebases: self
                    .forbid_p2_root_rebases
                    .unwrap_or(default.flags.forbid_p2_root_rebases),
                casefolding_check: self
                    .casefolding_check
                    .unwrap_or(default.flags.casefolding_check),
                casefolding_check_excluded_paths: self
                    .casefolding_check_excluded_paths
                    .map(|raw| {
                        raw.into_iter()
                            .map(|path| NonRootMPath::new_opt(path.as_bytes()).map(MPath::from))
                            .collect::<Result<PrefixTrie>>()
                    })
                    .transpose()?
                    .unwrap_or_default(),
                not_generated_filenodes_limit: 500,
                monitoring_bookmark: self.monitoring_bookmark,
                merge_resolution_excluded_path_prefixes: self
                    .merge_resolution_excluded_path_prefixes
                    .map(|raw| {
                        raw.into_iter()
                            .map(|path| NonRootMPath::new_opt(path.as_bytes()).map(MPath::from))
                            .collect::<Result<PrefixTrie>>()
                    })
                    .transpose()?
                    .unwrap_or_default(),
                // `NonRootMPath::new`, not `new_opt`: an empty string would
                // parse to the root prefix, which matches every path.
                merge_resolution_take_incoming_prefixes: self
                    .merge_resolution_take_incoming_prefixes
                    .map(|raw| {
                        raw.into_iter()
                            .map(|path| {
                                NonRootMPath::new(path.as_bytes())
                                    .map(MPath::from)
                                    .with_context(|| {
                                        format!(
                                            "merge_resolution_take_incoming_prefixes entry {path:?} must be a non-root path"
                                        )
                                    })
                            })
                            .collect::<Result<PrefixTrie>>()
                    })
                    .transpose()?
                    .unwrap_or_default(),
                merge_resolution_take_incoming: false, // request-scoped, not loaded from config
                merge_resolution_override: MergeResolutionOverride::UseJk, // request-scoped, not loaded from config
                land_instance_id: None, // request-scoped, not loaded from config
                phab_diff_id: None,     // request-scoped, not loaded from config
                mainline_parent_first: false, // request-scoped, not loaded from config
            },
            block_merges: self.block_merges.unwrap_or(default.block_merges),
            emit_obsmarkers: self.emit_obsmarkers.unwrap_or(default.emit_obsmarkers),
            globalrev_config: self
                .globalrevs_publishing_bookmark
                .map(|bookmark| {
                    anyhow::Ok(GlobalrevConfig {
                        publishing_bookmark: BookmarkKey::new(bookmark)?,
                        globalrevs_small_repo_id: self
                            .globalrevs_small_repo_id
                            .map(RepositoryId::new),
                    })
                })
                .transpose()?,
            populate_git_mapping: self
                .populate_git_mapping
                .unwrap_or(default.populate_git_mapping),
            allow_change_xrepo_mapping_extra: self
                .allow_change_xrepo_mapping_extra
                .unwrap_or(false),
            remote_mode: self
                .remote_mode
                .map_or(Ok(default.remote_mode), Convert::convert)?,
        })
    }
}

impl Convert for RawLfsParams {
    type Output = LfsParams;

    fn convert(self) -> Result<Self::Output> {
        Ok(LfsParams {
            threshold: self.threshold.map(|v| v.try_into()).transpose()?,
            rollout_percentage: self.rollout_percentage.unwrap_or(0).try_into()?,
            use_upstream_lfs_server: self.use_upstream_lfs_server.unwrap_or(false),
        })
    }
}

impl Convert for RawInfinitepushParams {
    type Output = InfinitepushParams;

    fn convert(self) -> Result<Self::Output> {
        Ok(InfinitepushParams {
            allow_writes: self.allow_writes,
            namespace: self.namespace_pattern.and_then(|ns| {
                ComparableRegex::new(ns)
                    .ok()
                    .map(InfinitepushNamespace::new)
            }),
        })
    }
}

impl Convert for RawSourceControlServiceParams {
    type Output = SourceControlServiceParams;

    fn convert(self) -> Result<Self::Output> {
        let service_write_restrictions = self
            .service_write_restrictions
            .unwrap_or_default()
            .into_iter()
            .map(|(name, raw)| Ok((name, raw.convert()?)))
            .collect::<Result<HashMap<_, _>>>()?;

        Ok(SourceControlServiceParams {
            permit_writes: self.permit_writes,
            permit_service_writes: self.permit_service_writes,
            service_write_hipster_acl: self.service_write_hipster_acl,
            permit_commits_without_parents: self.permit_commits_without_parents,
            service_write_restrictions,
        })
    }
}

impl Convert for RawServiceWriteRestrictions {
    type Output = ServiceWriteRestrictions;

    fn convert(self) -> Result<Self::Output> {
        let RawServiceWriteRestrictions {
            permitted_methods,
            permitted_path_prefixes,
            permitted_bookmarks,
            permitted_bookmark_regex,
            permit_create_commit_check_bypass,
            ..
        } = self;

        let permitted_methods = permitted_methods.into_iter().collect();

        let permitted_path_prefixes = permitted_path_prefixes
            .map(|raw| {
                raw.into_iter()
                    .map(|path| {
                        NonRootMPath::new_opt(path.as_bytes())
                            .map(mononoke_types::path::MPath::from)
                    })
                    .collect::<Result<PrefixTrie>>()
            })
            .transpose()?
            .unwrap_or_default();

        let permitted_bookmarks = permitted_bookmarks
            .unwrap_or_default()
            .into_iter()
            .collect();

        let permitted_bookmark_regex = permitted_bookmark_regex
            .as_deref()
            .map(ComparableRegex::new)
            .transpose()
            .context("invalid service write permitted bookmark regex")?;

        let permit_create_commit_check_bypass =
            permit_create_commit_check_bypass.unwrap_or_default();

        Ok(ServiceWriteRestrictions {
            permitted_methods,
            permitted_path_prefixes,
            permitted_bookmarks,
            permitted_bookmark_regex,
            permit_create_commit_check_bypass,
        })
    }
}

impl Convert for RawSourceControlServiceMonitoring {
    type Output = SourceControlServiceMonitoring;

    fn convert(self) -> Result<Self::Output> {
        let bookmarks_to_report_age = self
            .bookmarks_to_report_age
            .into_iter()
            .map(BookmarkKey::new)
            .collect::<Result<Vec<_>>>()?;
        Ok(SourceControlServiceMonitoring {
            bookmarks_to_report_age,
        })
    }
}

impl Convert for RawDerivedDataTypesConfig {
    type Output = DerivedDataTypesConfig;

    fn convert(self) -> Result<Self::Output> {
        let types = self
            .types
            .into_iter()
            .filter_map(|ty| match DerivableType::from_name(&ty) {
                Ok(dt) => Some(dt),
                // A config can enable a derived data type that this (older)
                // binary doesn't know yet while a new type is rolling out. Drop
                // it instead of failing to load the whole config: this binary
                // can't derive a type it doesn't know about, and the type will
                // start being derived once the binary is updated to know it.
                Err(_) => {
                    tracing::error!(
                        "Ignoring enabled derived data type {:?}: unknown to this binary",
                        ty
                    );
                    None
                }
            })
            .collect();
        let ephemeral_bubbles_disabled_types = self
            .ephemeral_bubbles_disabled_types
            .unwrap_or_default()
            .into_iter()
            .map(|ty| DerivableType::from_name(&ty))
            .collect::<Result<_>>()?;
        let mapping_key_prefixes: HashMap<DerivableType, String> = self
            .mapping_key_prefixes
            .into_iter()
            .map(|(k, _v)| Ok((DerivableType::from_name(&k)?, _v)))
            .collect::<Result<_>>()?;
        let unode_version = match self.unode_version {
            None => UnodeVersion::default(),
            Some(1) => return Err(anyhow!("unode version 1 has been deprecated")),
            Some(2) => UnodeVersion::V2,
            Some(version) => return Err(anyhow!("unknown unode version {version}")),
        };
        let blame_filesize_limit = self.blame_filesize_limit.map(|limit| limit as u64);
        let blame_version = match self.blame_version {
            None => BlameVersion::default(),
            Some(1) => return Err(anyhow!("blame version 1 has been deprecated")),
            Some(2) => BlameVersion::V2,
            Some(3) => BlameVersion::V3,
            Some(version) => return Err(anyhow!("unknown blame version {version}")),
        };
        let git_delta_manifest_version = match self.git_delta_manifest_version {
            None => GitDeltaManifestVersion::default(),
            Some(2) => GitDeltaManifestVersion::V2,
            Some(3) => GitDeltaManifestVersion::V3,
            Some(version) => return Err(anyhow!("unknown git delta manifest version {version}")),
        };
        let git_delta_manifest_v2_config = self
            .git_delta_manifest_v2_config
            .map(|raw| raw.convert())
            .transpose()?;
        let git_delta_manifest_v3_config = self
            .git_delta_manifest_v3_config
            .map(|raw| raw.convert())
            .transpose()?;

        let derivation_batch_sizes = self
            .derivation_batch_sizes
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| Ok((DerivableType::from_name(&k)?, v.try_into()?)))
            .collect::<Result<_>>()?;

        let inferred_copy_from_config = self
            .inferred_copy_from_config
            .map(|raw| raw.convert())
            .transpose()?;

        let xdb_mapping_shard_ids: HashMap<DerivableType, usize> = self
            .xdb_mapping_shard_ids
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(k, v)| match DerivableType::from_name(&k) {
                Ok(dt) => Some((dt, v)),
                // A config can name a derived data type that this (older) binary
                // doesn't know yet while a new type is rolling out. Drop the
                // entry instead of failing to load the whole config: the shard
                // id is only ever read when that type is actually derived, and
                // this binary can't derive a type it doesn't know about.
                Err(_) => {
                    tracing::error!(
                        "Ignoring xdb_mapping_shard_ids entry for unknown derived data type {:?}",
                        k
                    );
                    None
                }
            })
            .map(|(dt, v)| Ok((dt, v.try_into()?)))
            .collect::<Result<_>>()?;

        Ok(DerivedDataTypesConfig {
            types,
            ephemeral_bubbles_disabled_types,
            mapping_key_prefixes,
            unode_version,
            blame_filesize_limit,
            hg_set_committer_extra: self.hg_set_committer_extra.unwrap_or(false),
            blame_version,
            git_delta_manifest_version,
            git_delta_manifest_v2_config,
            git_delta_manifest_v3_config,
            derivation_batch_sizes,
            inferred_copy_from_config,
            xdb_mapping_shard_ids,
        })
    }
}

impl Convert for RawInferredCopyFromConfig {
    type Output = InferredCopyFromConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(InferredCopyFromConfig {
            dir_level_for_basename_lookup: self.dir_level_for_basename_lookup as usize,
            basename_match_max_candidates: self.basename_match_max_candidates as usize,
            partial_match_max_file_size: self.partial_match_max_file_size as u64,
            max_num_changed_files: self.max_num_changed_files as usize,
            partial_match_skip_file_extensions: self.partial_match_skip_file_extensions,
        })
    }
}

impl Convert for RawGitDeltaManifestV2Config {
    type Output = GitDeltaManifestV2Config;

    fn convert(self) -> Result<Self::Output> {
        Ok(GitDeltaManifestV2Config {
            max_inlined_object_size: self.max_inlined_object_size as usize,
            max_inlined_delta_size: self.max_inlined_delta_size as u64,
            delta_chunk_size: self.delta_chunk_size as u64,
        })
    }
}

impl Convert for RawGitDeltaManifestV3Config {
    type Output = GitDeltaManifestV3Config;

    fn convert(self) -> Result<Self::Output> {
        Ok(GitDeltaManifestV3Config {
            max_inlined_object_size: self.max_inlined_object_size as usize,
            max_inlined_delta_size: self.max_inlined_delta_size as u64,
            delta_chunk_size: self.delta_chunk_size as u64,
            entry_chunk_size: self.entry_chunk_size as usize,
        })
    }
}

impl Convert for RawDerivedDataConfig {
    type Output = DerivedDataConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(DerivedDataConfig {
            scuba_table: self.scuba_table,
            enabled_config_name: self.enabled_config_name.unwrap_or_default(),
            available_configs: self
                .available_configs
                .unwrap_or_default()
                .into_iter()
                .map(|(s, raw_config)| Ok((s, raw_config.convert()?)))
                .collect::<Result<_, anyhow::Error>>()?,
            bonsai_hg_mapping_scuba_table: self.bonsai_hg_mapping_scuba_table,
            remote_derivation_config: self
                .remote_derivation_config
                .map(|raw| raw.convert())
                .transpose()?,
            blocked_derivation: self
                .blocked_derivation
                .map(|blocked_derivation| blocked_derivation.convert())
                .transpose()?
                .unwrap_or_default(),
            extra_types_available_for_read: self
                .extra_types_available_for_read
                .unwrap_or_default()
                .into_iter()
                .map(|s| DerivableType::from_name(&s))
                .collect::<Result<_, _>>()?,
            wbc_excluded_types: self
                .wbc_excluded_types
                .unwrap_or_default()
                .into_iter()
                .map(|s| DerivableType::from_name(&s))
                .collect::<Result<_, _>>()?,
            // Reads thrift field `pipeline_config_v2` (renamed from
            // `pipeline_config` so old binaries' JSON deserializer skips
            // the new shape instead of crashing on it). The in-memory
            // Rust field stays named `pipeline_config`.
            pipeline_config: self
                .pipeline_config_v2
                .map(|raw| raw.convert())
                .transpose()?,
        })
    }
}

impl Convert for RawDerivedDataBlockedDerivation {
    type Output = HashMap<ChangesetId, Option<HashSet<DerivableType>>>;

    fn convert(self) -> Result<Self::Output> {
        self.changesets
            .into_iter()
            .map(|(csid, blocked_derivation)| {
                Ok((ChangesetId::from_str(&csid)?, blocked_derivation.convert()?))
            })
            .collect()
    }
}

impl Convert for RawDerivedDataBlockedChangesetDerivation {
    type Output = Option<HashSet<DerivableType>>;

    fn convert(self) -> Result<Self::Output> {
        self.blocked_derived_data_types
            .map(|types| {
                types
                    .into_iter()
                    .map(|ty| DerivableType::from_name(&ty))
                    .collect::<Result<HashSet<_>, _>>()
            })
            .transpose()
    }
}

impl Convert for RawRemoteDerivationConfig {
    type Output = RemoteDerivationConfig;

    fn convert(self) -> Result<Self::Output> {
        match self {
            RawRemoteDerivationConfig::shard_manager_tier(shard_manager_tier) => {
                Ok(RemoteDerivationConfig::ShardManagerTier(shard_manager_tier))
            }
            RawRemoteDerivationConfig::smc_tier(smc_tier) => {
                Ok(RemoteDerivationConfig::SmcTier(smc_tier))
            }
            RawRemoteDerivationConfig::host_port(host_port) => {
                Ok(RemoteDerivationConfig::HostPort(host_port))
            }
            RawRemoteDerivationConfig::UnknownField(e) => {
                anyhow::bail!("Unknown variant of RawRemoteDerivationConfig: {e}")
            }
        }
    }
}

impl Convert for RawRemoteDiffConfig {
    type Output = RemoteDiffConfig;

    fn convert(self) -> Result<Self::Output> {
        match self {
            RawRemoteDiffConfig::shard_manager_tier(shard_manager_tier) => {
                Ok(RemoteDiffConfig::ShardManagerTier(shard_manager_tier))
            }
            RawRemoteDiffConfig::smc_tier(smc_tier) => Ok(RemoteDiffConfig::SmcTier(smc_tier)),
            RawRemoteDiffConfig::host_port(host_port) => Ok(RemoteDiffConfig::HostPort(host_port)),
            RawRemoteDiffConfig::UnknownField(e) => {
                anyhow::bail!("Unknown variant of RawRemoteDiffConfig: {e}")
            }
        }
    }
}

impl Convert for RawCommitRateLimitConfig {
    type Output = CommitRateLimitConfig;

    fn convert(self) -> Result<Self::Output> {
        let cache_config = self
            .cache_config
            .map(|cc| -> Result<CommitRateLimitCacheConfig> {
                Ok(CommitRateLimitCacheConfig {
                    max_entries: cc
                        .max_entries
                        .try_into()
                        .context("cache max_entries must be non-negative")?,
                    ttl_secs: cc
                        .ttl_secs
                        .try_into()
                        .context("cache ttl_secs must be non-negative")?,
                })
            })
            .transpose()?;
        let rules = self
            .rules
            .into_iter()
            .map(|rule| {
                let eligibility_checks = rule
                    .eligibility_checks
                    .into_iter()
                    .map(|check| match check {
                        RawEligibilityCheck::commit_message_tag(tag) => {
                            Ok(CommitRateLimitEligibilityCheck::CommitMessageTag(tag))
                        }
                        RawEligibilityCheck::hg_extra_key(key) => {
                            Ok(CommitRateLimitEligibilityCheck::HgExtra(key))
                        }
                        RawEligibilityCheck::always_pass(_) => {
                            Ok(CommitRateLimitEligibilityCheck::AlwaysPass)
                        }
                        RawEligibilityCheck::UnknownField(id) => {
                            bail!("Unknown variant of RawEligibilityCheck: {id}")
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                let limits = rule
                    .limits
                    .into_iter()
                    .map(|limit| -> Result<CommitRateLimitWindow> {
                        Ok(CommitRateLimitWindow {
                            window_secs: limit
                                .window_secs
                                .try_into()
                                .context("window_secs must be non-negative")?,
                            max_commits: limit
                                .max_commits
                                .try_into()
                                .context("max_commits must be non-negative")?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(CommitRateLimitRuleConfig {
                    name: rule.name,
                    eligibility_checks,
                    limits,
                    directories: rule.directories,
                    per_user: rule.per_user,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(CommitRateLimitConfig {
            rules,
            cache_config,
        })
    }
}

impl Convert for RawDerivationPipelineStageConfig {
    type Output = DerivationPipelineStageConfig;

    fn convert(self) -> Result<Self::Output> {
        let dependencies = self
            .dependencies
            .into_iter()
            .map(|p| MPath::new(p.as_bytes()))
            .collect::<Result<Vec<_>>>()?;
        Ok(DerivationPipelineStageConfig { dependencies })
    }
}

impl Convert for RawDerivationPipelineConfig {
    type Output = DerivationPipelineConfig;

    fn convert(self) -> Result<Self::Output> {
        let types = self
            .types
            .into_iter()
            .map(|name| DerivableType::from_name(&name))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let bookmarks = self
            .bookmarks
            .into_iter()
            .map(BookmarkKey::new)
            .collect::<Result<Vec<_>>>()?;
        let stages = self
            .stages
            .into_iter()
            .map(|(path, raw_config)| Ok((MPath::new(path.as_bytes())?, raw_config.convert()?)))
            .collect::<Result<HashMap<_, _>>>()?;
        let batch_size = u64::try_from(self.batch_size)
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or_else(|| {
                anyhow!(
                    "pipeline_config.batch_size must be a positive integer, got {}",
                    self.batch_size,
                )
            })?;
        let config = DerivationPipelineConfig {
            types,
            bookmarks,
            stages,
            batch_size,
        };
        config
            .validate()
            .context("Invalid derivation pipeline config")?;
        Ok(config)
    }
}

impl Convert for RawWalkerJobType {
    type Output = WalkerJobType;

    fn convert(self) -> Result<Self::Output> {
        let job_type = match self {
            RawWalkerJobType::SCRUB_ALL_CHUNKED => WalkerJobType::ScrubAllChunked,
            RawWalkerJobType::SCRUB_DERIVED_CHUNKED => WalkerJobType::ScrubDerivedChunked,
            RawWalkerJobType::SCRUB_DERIVED_NO_CONTENT_META => {
                WalkerJobType::ScrubDerivedNoContentMeta
            }
            RawWalkerJobType::SCRUB_DERIVED_NO_CONTENT_META_CHUNKED => {
                WalkerJobType::ScrubDerivedNoContentMetaChunked
            }
            RawWalkerJobType::SCRUB_HG_ALL_CHUNKED => WalkerJobType::ScrubHgAllChunked,
            RawWalkerJobType::SCRUB_HG_FILE_CONTENT => WalkerJobType::ScrubHgFileContent,
            RawWalkerJobType::SCRUB_HG_FILE_NODE => WalkerJobType::ScrubHgFileNode,
            RawWalkerJobType::SCRUB_UNODE_ALL_CHUNKED => WalkerJobType::ScrubUnodeAllChunked,
            RawWalkerJobType::SCRUB_UNODE_BLAME => WalkerJobType::ScrubUnodeBlame,
            RawWalkerJobType::SCRUB_UNODE_FASTLOG => WalkerJobType::ScrubUnodeFastlog,
            RawWalkerJobType::SHALLOW_HG_SCRUB => WalkerJobType::ShallowHgScrub,
            RawWalkerJobType::VALIDATE_ALL => WalkerJobType::ValidateAll,
            RawWalkerJobType::UNKNOWN => WalkerJobType::Unknown,
            v => return Err(anyhow!("Invalid value {v} for enum WalkerJobType")),
        };
        Ok(job_type)
    }
}

impl Convert for RawWalkerJobParams {
    type Output = WalkerJobParams;

    fn convert(self) -> Result<Self::Output> {
        Ok(WalkerJobParams {
            scheduled_max_concurrency: self.scheduled_max_concurrency,
            qps_limit: self.qps_limit,
            exclude_node_type: self.exclude_node_type,
            allow_remaining_deferred: self.allow_remaining_deferred.is_some_and(|v| v),
            error_as_node_data_type: self.error_as_node_data_type,
        })
    }
}

impl Convert for RawWalkerConfig {
    type Output = WalkerConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(WalkerConfig {
            scrub_enabled: self.scrub_enabled,
            validate_enabled: self.validate_enabled,
            params: self
                .params
                .map(|p| {
                    p.into_iter()
                        .map(|(k, v)| anyhow::Ok((k.convert()?, v.convert()?)))
                        .collect::<Result<_, _>>()
                })
                .transpose()?,
        })
    }
}

impl Convert for RawCrossRepoCommitValidationConfig {
    type Output = CrossRepoCommitValidation;

    fn convert(self) -> Result<Self::Output> {
        let skip_bookmarks = self
            .skip_bookmarks
            .into_iter()
            .map(BookmarkKey::new)
            .collect::<Result<_, _>>()?;
        Ok(CrossRepoCommitValidation { skip_bookmarks })
    }
}

impl Convert for RawSparseProfilesConfig {
    type Output = SparseProfilesConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(SparseProfilesConfig {
            sparse_profiles_location: self.sparse_profiles_location,
            excluded_paths: self.excluded_paths.unwrap_or_default(),
            monitored_profiles: self.monitored_profiles.unwrap_or_default(),
        })
    }
}

impl Convert for RawCasSyncConfig {
    type Output = MononokeCasSyncConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(MononokeCasSyncConfig {
            main_bookmark_to_sync: self.main_bookmark_to_sync,
            sync_all_bookmarks: self.sync_all_bookmarks,
            use_case_public: self.use_case_public,
            use_case_draft: self.use_case_draft,
        })
    }
}

impl Convert for RawModernSyncConfig {
    type Output = ModernSyncConfig;

    fn convert(self) -> Result<Self::Output> {
        if self.max_blob_bytes < 1 {
            // Channels without an explicit max_bytes_in_flight fall back to
            // max_blob_bytes; clamping that fallback to 1 makes every batch
            // oversized, so the affected channels' flushes run exclusively.
            tracing::warn!(
                "modern_sync max_blob_bytes={} is not positive; max_bytes_in_flight fallbacks clamp to 1",
                self.max_blob_bytes,
            );
        }
        // Channels without an explicit max_bytes_in_flight fall back to
        // max_blob_bytes; store the clamped value so downstream users see
        // the same bound the fallback uses.
        let max_blob_bytes = self.max_blob_bytes.max(1);
        Ok(ModernSyncConfig {
            url: self.url,
            chunk_size: self.chunk_size,
            single_db_query_entries_limit: self.single_db_query_entries_limit,
            changeset_concurrency: self.changeset_concurrency,
            max_blob_bytes,
            content_channel_config: convert_channel_config(
                self.content_channel_config,
                max_blob_bytes,
            )?,
            filenodes_channel_config: convert_channel_config(
                self.filenodes_channel_config,
                max_blob_bytes,
            )?,
            trees_channel_config: convert_channel_config(
                self.trees_channel_config,
                max_blob_bytes,
            )?,
            changesets_channel_config: convert_channel_config(
                self.changesets_channel_config,
                max_blob_bytes,
            )?,
        })
    }
}

fn warn_if_nonpositive(field: &str, value: Option<i64>) {
    if let Some(v) = value {
        if v < 1 {
            tracing::warn!("modern_sync {field}={v} is not positive; clamping to 1");
        }
    }
}

fn convert_channel_config(
    raw: RawModernSyncChannelConfig,
    default_max_bytes_in_flight: i64,
) -> Result<ModernSyncChannelConfig> {
    // Returns Result to compose with the surrounding `Convert::convert`
    // via `?`; currently infallible since out-of-range bounds clamp.
    // Clamp instead of rejecting: this runs in the shared metaconfig parser,
    // so a hard error would make the whole repo config unloadable for every
    // binary, not just modern_sync. Non-positive values can only come from
    // explicit misconfiguration or a non-positive max_blob_bytes fallback.
    warn_if_nonpositive("max_concurrent_flushes", raw.max_concurrent_flushes);
    warn_if_nonpositive("max_bytes_in_flight", raw.max_bytes_in_flight);
    let max_concurrent_flushes = raw.max_concurrent_flushes.unwrap_or(1).max(1);
    let max_bytes_in_flight = raw
        .max_bytes_in_flight
        .unwrap_or(default_max_bytes_in_flight)
        .max(1);
    Ok(ModernSyncChannelConfig {
        batch_size: raw.batch_size,
        channel_size: raw.channel_size,
        flush_interval_ms: raw.flush_interval_ms,
        max_concurrent_flushes,
        max_bytes_in_flight,
    })
}

impl Convert for RawLoggingDestination {
    type Output = LoggingDestination;

    fn convert(self) -> Result<Self::Output> {
        let dest = match self {
            Self::logger(_) => LoggingDestination::Logger,
            Self::scribe(RawLoggingDestinationScribe { scribe_category }) => {
                LoggingDestination::Scribe { scribe_category }
            }
            Self::UnknownField(f) => {
                return Err(anyhow!("Unknown variant {f} of RawLoggingDestination"));
            }
        };
        Ok(dest)
    }
}

impl Convert for RawUpdateLoggingConfig {
    type Output = UpdateLoggingConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(UpdateLoggingConfig {
            bookmark_logging_destination: self.bookmark_logging_destination.convert()?,
            new_commit_logging_destination: self.new_commit_logging_destination.convert()?,
            git_content_refs_logging_destination: self
                .git_content_refs_logging_destination
                .convert()?,
        })
    }
}

impl Convert for RawCommitGraphConfig {
    type Output = CommitGraphConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(CommitGraphConfig {
            scuba_table: self.scuba_table,
            preloaded_commit_graph_blobstore_key: self.preloaded_commit_graph_blobstore_key,
            disable_commit_graph_v2_with_empty_common: self
                .disable_commit_graph_v2_with_empty_common,
        })
    }
}

impl Convert for RawMetadataLoggerConfig {
    type Output = MetadataLoggerConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(MetadataLoggerConfig {
            bookmarks: self
                .bookmarks
                .into_iter()
                .map(BookmarkKey::new)
                .collect::<Result<_>>()?,
            sleep_interval_secs: self.sleep_interval_secs.map(u64::try_from).transpose()?,
        })
    }
}

impl Convert for RawZelosConfig {
    type Output = ZelosConfig;

    fn convert(self) -> Result<Self::Output> {
        match self {
            Self::local_zelos_port(port) => Ok(ZelosConfig::Local {
                port: port.try_into()?,
            }),
            Self::zelos_tier(tier) => Ok(ZelosConfig::Remote { tier }),
            Self::UnknownField(f) => Err(anyhow!("Unknown variant {f} of RawZelosConfig")),
        }
    }
}

impl Convert for RawGitBundleURIConfig {
    type Output = GitBundleUriConfig;

    fn convert(self) -> Result<Self::Output> {
        match self.uri_generator_type {
            RawUriGeneratorType::cdn(cdn) => Ok(GitBundleUriConfig {
                uri_generator_type: UriGeneratorType::Cdn {
                    bucket: cdn.bucket,
                    api_key: cdn.api_key,
                },
                trusted_only: self.trusted_only,
            }),
            RawUriGeneratorType::manifold(manifold) => Ok(GitBundleUriConfig {
                uri_generator_type: UriGeneratorType::Manifold {
                    bucket: manifold.bucket,
                    api_key: manifold.api_key,
                },
                trusted_only: self.trusted_only,
            }),
            RawUriGeneratorType::local_fs(_) => Ok(GitBundleUriConfig {
                uri_generator_type: UriGeneratorType::LocalFs,
                trusted_only: self.trusted_only,
            }),
            RawUriGeneratorType::UnknownField(f) => {
                Err(anyhow!("Unknown variant {f} of RawGitBundleURIConfig"))
            }
        }
    }
}

impl Convert for RawShardedService {
    type Output = ShardedService;

    fn convert(self) -> Result<Self::Output> {
        let service = match self {
            RawShardedService::EDEN_API => ShardedService::SaplingRemoteApi,
            RawShardedService::SOURCE_CONTROL_SERVICE => ShardedService::SourceControlService,
            RawShardedService::DERIVED_DATA_SERVICE => ShardedService::DerivedDataService,
            RawShardedService::LAND_SERVICE => ShardedService::LandService,
            RawShardedService::LARGE_FILES_SERVICE => ShardedService::LargeFilesService,
            RawShardedService::DERIVATION_WORKER => ShardedService::DerivationWorker,
            RawShardedService::ASYNC_REQUESTS_WORKER => ShardedService::AsyncRequestsWorker,
            RawShardedService::WALKER_SCRUB_ALL => ShardedService::WalkerScrubAll,
            RawShardedService::WALKER_VALIDATE_ALL => ShardedService::WalkerValidateAll,
            RawShardedService::DERIVED_DATA_TAILER => ShardedService::DerivedDataTailer,
            RawShardedService::ALIAS_VERIFY => ShardedService::AliasVerify,
            RawShardedService::DRAFT_COMMIT_DELETION => ShardedService::DraftCommitDeletion,
            RawShardedService::MONONOKE_GIT_SERVER => ShardedService::MononokeGitServer,
            RawShardedService::REPO_METADATA_LOGGER => ShardedService::RepoMetadataLogger,
            RawShardedService::BOOKMARK_SERVICE => ShardedService::BookmarkService,
            RawShardedService::DIFF_SERVICE => ShardedService::DiffService,
            RawShardedService::DERIVATION_PIPELINE_TAILER => {
                ShardedService::DerivationPipelineTailer
            }
            RawShardedService::DERIVATION_PIPELINE_WORKER => {
                ShardedService::DerivationPipelineWorker
            }
            RawShardedService::P4_SYNC => ShardedService::P4Sync,
            v => return Err(anyhow!("Invalid value {v} for enum ShardedService")),
        };
        Ok(service)
    }
}

impl Convert for RawShardingModeConfig {
    type Output = ShardingModeConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(ShardingModeConfig {
            status: self
                .status
                .into_iter()
                // Since this is a simple type conversion, the only error that can be encountered would be due to an
                // unknown enum value. If that happens, it means we have a config that has more values than the code understands. In
                // such a case, it should be safe to ignore this unknown value cause the existing code can work without it.
                .filter_map(|(k, v)| k.convert().map(|k| (k, v)).ok())
                .collect(),
        })
    }
}

impl Convert for RawLazyLoadingConfig {
    type Output = LazyLoadingConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(LazyLoadingConfig {
            sharded: self
                .sharded
                .unwrap_or_default()
                .into_iter()
                // Unknown services are dropped, not rejected, so adding one is
                // not a breaking change for older binaries.
                .filter_map(|(k, v)| k.convert().map(|k| (k, v)).ok())
                .collect(),
            unsharded: self.unsharded.unwrap_or(false),
        })
    }
}

impl Convert for RawGitConcurrencyParams {
    type Output = GitConcurrencyParams;

    fn convert(self) -> Result<Self::Output> {
        Ok(GitConcurrencyParams {
            trees_and_blobs: self.trees_and_blobs.try_into()?,
            commits: self.commits.try_into()?,
            tags: self.tags.try_into()?,
            shallow: self.shallow.try_into()?,
        })
    }
}

impl Convert for RawGitConfigs {
    type Output = GitConfigs;

    fn convert(self) -> Result<Self::Output> {
        let git_concurrency = self.git_concurrency.convert()?;
        let git_lfs_interpret_pointers = self.git_lfs_interpret_pointers.unwrap_or(false);

        let fetch_message = self.fetch_message;

        let git_bundle_uri = self.git_bundle_uri_config.convert()?;

        let preloaded_cgdm_blobstore_key = self.preloaded_cgdm_blobstore_key;

        Ok(GitConfigs {
            git_concurrency,
            git_lfs_interpret_pointers,
            fetch_message,
            git_bundle_uri,
            preloaded_cgdm_blobstore_key,
        })
    }
}

impl Convert for RawP4SourceConfig {
    type Output = P4SourceConfig;

    fn convert(self) -> Result<Self::Output> {
        // A depot root, `//name/`: the importer appends `...` to it.
        let name = self
            .depot
            .strip_prefix("//")
            .and_then(|rest| rest.strip_suffix('/'))
            .filter(|name| !name.is_empty() && !name.contains('/'));
        ensure!(
            name.is_some(),
            "p4_source.depot must be a depot root of the form //name/, got {:?}",
            self.depot
        );
        ensure!(
            !self.server_alias.is_empty(),
            "p4_source.server_alias must not be empty"
        );
        Ok(P4SourceConfig {
            server_alias: self.server_alias,
            depot: self.depot,
        })
    }
}

impl Convert for RawXRepoSyncSourceConfig {
    type Output = XRepoSyncSourceConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(XRepoSyncSourceConfig {
            bookmark_regex: self.bookmark_regex,
            backsync_enabled: self.backsync_enabled,
        })
    }
}

impl Convert for RawXRepoSyncSourceConfigMapping {
    type Output = XRepoSyncSourceConfigMapping;

    fn convert(self) -> Result<Self::Output> {
        let mapping = self
            .mapping
            .into_iter()
            .map(|(repo_name, x_repo_sync_source_config)| {
                Ok((repo_name, x_repo_sync_source_config.convert()?))
            })
            .collect::<Result<_>>()?;
        Ok(XRepoSyncSourceConfigMapping { mapping })
    }
}

impl Convert for RawCommitCloudConfig {
    type Output = CommitCloudConfig;
    fn convert(self) -> Result<Self::Output> {
        Ok(CommitCloudConfig {
            mocked_employees: self.mocked_employees,
            disable_interngraph_notification: self.disable_interngraph_notification,
        })
    }
}

impl Convert for RawMetadataCacheUpdateMode {
    type Output = MetadataCacheUpdateMode;
    fn convert(self) -> Result<Self::Output> {
        let cache_update_mode = match self {
            RawMetadataCacheUpdateMode::tailing(tailing) => MetadataCacheUpdateMode::Tailing {
                category: tailing.category,
            },
            RawMetadataCacheUpdateMode::polling(_) => MetadataCacheUpdateMode::Polling,
            RawMetadataCacheUpdateMode::UnknownField(f) => {
                bail!("Unsupported MetadataCacheUpdateMode {f}")
            }
        };
        Ok(cache_update_mode)
    }
}

impl Convert for RawMetadataCacheConfig {
    type Output = MetadataCacheConfig;
    fn convert(self) -> Result<Self::Output> {
        ensure!(
            self.wbc_event_router_batching_enabled != Some(true)
                || self.wbc_process_bookmark_notifications == Some(true),
            "wbc_event_router_batching_enabled requires wbc_process_bookmark_notifications=true"
        );
        Ok(MetadataCacheConfig {
            wbc_update_mode: self
                .wbc_update_mode
                .map(|mode| mode.convert())
                .transpose()?,
            wbc_event_router_update_mode: self
                .wbc_event_router_update_mode
                .map(|mode| mode.convert())
                .transpose()?,
            wbc_process_bookmark_notifications: self
                .wbc_process_bookmark_notifications
                .unwrap_or(false),
            wbc_event_router_batching_enabled: self
                .wbc_event_router_batching_enabled
                .unwrap_or(false),
            tags_update_mode: self
                .tags_update_mode
                .map(|mode| mode.convert())
                .transpose()?,
            content_refs_update_mode: self
                .content_refs_update_mode
                .map(|mode| mode.convert())
                .transpose()?,
        })
    }
}

impl Convert for RawDirectoryBranchClusterFixedCluster {
    type Output = DirectoryBranchClusterFixedCluster;

    fn convert(self) -> Result<Self::Output> {
        let cluster_primary =
            NonRootMPath::new(self.cluster_primary).context("Invalid cluster primary path")?;
        let secondaries = self
            .secondaries
            .into_iter()
            .map(|path| NonRootMPath::new(path).context("Invalid secondary path"))
            .collect::<Result<Vec<_>>>()?;

        Ok(DirectoryBranchClusterFixedCluster {
            cluster_primary,
            secondaries,
        })
    }
}

impl Convert for RawDirectoryBranchClusterFixedConfig {
    type Output = DirectoryBranchClusterFixedConfig;

    fn convert(self) -> Result<Self::Output> {
        let clusters = self
            .clusters
            .into_iter()
            .map(|cluster| cluster.convert())
            .collect::<Result<Vec<_>>>()?;

        Ok(DirectoryBranchClusterFixedConfig { clusters })
    }
}

impl Convert for RawDirectoryBranchClusterConfig {
    type Output = DirectoryBranchClusterConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(DirectoryBranchClusterConfig {
            fixed_config: self.fixed_config.convert()?,
        })
    }
}

impl Convert for RawSoftRestrictedPathConfig {
    type Output = SoftRestrictedPathConfig;

    fn convert(self) -> Result<Self::Output> {
        let path = NonRootMPath::new(self.path.as_bytes()).with_context(|| {
            format!(
                "Invalid path for soft restricted path config: {}",
                self.path
            )
        })?;

        Ok(SoftRestrictedPathConfig {
            path,
            acl: self.acl,
            max_copied_files_limit: self.max_copied_files_limit.try_into()?,
        })
    }
}

fn parse_acl_manifest_mode(s: Option<&str>) -> Result<AclManifestMode> {
    match s {
        None | Some("Disabled") => Ok(AclManifestMode::Disabled),
        Some("Shadow") => Ok(AclManifestMode::Shadow),
        Some("Both") => Ok(AclManifestMode::Both),
        Some("Authoritative") => Ok(AclManifestMode::Authoritative),
        Some(other) => Err(anyhow!(
            "invalid acl_manifest_mode value '{other}': expected one of Disabled, Shadow, Both, Authoritative"
        )),
    }
}

/// Convert the raw per-path restriction metadata into its typed form. Every
/// failure is fail-closed: a malformed path or identity aborts config load
/// rather than silently dropping a restriction.
fn convert_path_restriction_metadata(
    path_restriction_metadata: Option<BTreeMap<String, RawPathRestrictionMetadata>>,
) -> Result<HashMap<NonRootMPath, PathRestrictionMetadata>> {
    path_restriction_metadata
        .unwrap_or_default()
        .into_iter()
        .map(|(path, raw)| {
            let non_root_path = NonRootMPath::new(path.as_bytes())
                .with_context(|| format!("Invalid path for restricted path config: {path}"))?;
            let repo_region_acl = MononokeIdentity::from_str(&raw.repo_region_acl)
                .with_context(|| format!("Failed to parse repo_region_acl for {path}"))?;
            let permission_request_group = raw
                .permission_request_group
                .map(|group| {
                    MononokeIdentity::from_str(&group).with_context(|| {
                        format!("Failed to parse permission_request_group for {path}")
                    })
                })
                .transpose()?;
            let rollout_allowlist_group = raw
                .rollout_allowlist_group
                .as_deref()
                .map(parse_bare_group_name)
                .transpose()
                .with_context(|| format!("Invalid rollout_allowlist_group for {path}"))?;
            Ok((
                non_root_path,
                PathRestrictionMetadata {
                    repo_region_acl,
                    permission_request_group,
                    rollout_allowlist_group,
                    read_only: raw.read_only.unwrap_or(false),
                },
            ))
        })
        .collect()
}

impl Convert for RawRestrictedPathsConfig {
    type Output = RestrictedPathsConfig;

    fn convert(self) -> Result<Self::Output> {
        let path_restriction_metadata =
            convert_path_restriction_metadata(self.path_restriction_metadata)?;

        let manifest_id_store_config =
            convert_manifest_id_store_config(self.manifest_id_store_config.unwrap_or_default())?;

        let soft_path_acls = self
            .soft_path_acls
            .unwrap_or_default()
            .into_iter()
            .map(|raw| raw.convert())
            .collect::<Result<Vec<_>>>()?;

        // tooling_allowlist_group is used directly as a group name for membership checking
        let tooling_allowlist_group = self.tooling_allowlist_acl;

        // admin_bypass_group is an optional group identity used for membership
        // checking when bypassing Path ACL enforcement.
        let admin_bypass_group = self
            .admin_bypass_group
            .map(|group| {
                MononokeIdentity::from_str(format!("GROUP:{group}").as_str())
                    .with_context(|| format!("Failed to parse admin_bypass_group `{group}`"))
            })
            .transpose()?;

        let enforcement_condition_sets = self
            .enforcement_condition_sets
            .unwrap_or_default()
            .into_iter()
            .map(|raw| {
                Ok::<_, anyhow::Error>(EnforcementConditionSet {
                    always_enabled: raw.always_enabled.unwrap_or(false),
                    require_client_request_flag: raw.require_client_request_flag.unwrap_or(false),
                    restriction_acls: raw
                        .restriction_acls
                        .iter()
                        .flatten()
                        .map(|s| {
                            MononokeIdentity::from_str(s)
                                .with_context(|| format!("parsing restriction_acl `{s}`"))
                        })
                        .collect::<Result<Vec<_>>>()?,
                    matchers: convert_request_matchers(raw)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let enforcement_exemption_sets = self
            .enforcement_exemption_sets
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, raw)| {
                convert_enforcement_exemption_set(raw)
                    .with_context(|| format!("invalid enforcement_exemption_sets[{index}]"))
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(RestrictedPathsConfig {
            path_restriction_metadata,
            manifest_id_store_config,
            soft_path_acls,
            tooling_allowlist_group,
            admin_bypass_group,
            acl_file_name: self
                .acl_file_name
                .unwrap_or(RestrictedPathsConfig::default().acl_file_name.to_string()),
            enforcement_condition_sets,
            enforcement_exemption_sets,
            enforcement_enabled: self.enforcement_enabled.unwrap_or(false),
            acl_manifest_mode: parse_acl_manifest_mode(self.acl_manifest_mode.as_deref())?,
            denial_message: self.denial_message,
        })
    }
}

/// Convert the request-metadata matchers of a raw enforcement condition set.
fn convert_request_matchers(raw: RawEnforcementConditionSet) -> Result<RequestMatchers> {
    Ok(RequestMatchers {
        entry_points: raw.entry_points.unwrap_or_default(),
        machine_tiers: raw.machine_tiers.unwrap_or_default(),
        build_rules: raw.build_rules.unwrap_or_default(),
        client_identity_regexes: raw
            .client_identity_regexes
            .unwrap_or_default()
            .into_iter()
            .map(|pattern| {
                ComparableRegex::new(&pattern)
                    .with_context(|| format!("parsing client_identity_regex `{pattern}`"))
            })
            .collect::<Result<Vec<_>>>()?,
        is_agent: raw.is_agent,
    })
}

/// Convert a raw enforcement exemption set.
///
/// Exemptions share the raw condition-set shape in thrift, but a mistake fails
/// open, so the fields an exemption cannot have are rejected rather than
/// ignored, as is a set with no matcher. That includes the deprecated
/// `restriction_roots`, which condition sets ignore.
fn convert_enforcement_exemption_set(
    raw: RawEnforcementConditionSet,
) -> Result<EnforcementExemptionSet> {
    ensure!(
        raw.always_enabled != Some(true),
        "exemptions cannot set `always_enabled`"
    );
    ensure!(
        raw.require_client_request_flag != Some(true),
        "exemptions cannot set `require_client_request_flag`"
    );
    ensure!(
        raw.restriction_acls.as_ref().is_none_or(Vec::is_empty),
        "exemptions cannot set `restriction_acls`"
    );
    ensure!(
        raw.restriction_roots.as_ref().is_none_or(Vec::is_empty),
        "exemptions cannot set `restriction_roots`"
    );
    EnforcementExemptionSet::new(convert_request_matchers(raw)?)
}

fn convert_manifest_id_store_config(
    raw: RawRestrictedPathsManifestIdStoreConfig,
) -> Result<RestrictedPathsManifestIdStoreConfig> {
    let defaults = RestrictedPathsManifestIdStoreConfig::default();
    let cache_update_interval_ms = positive_config_value(
        "cache_update_interval_ms",
        raw.cache_update_interval_ms,
        defaults.cache_update_interval_ms,
    )?;
    let incremental_cache_update_lookback_ids = positive_config_value(
        "incremental_cache_update_lookback_ids",
        raw.incremental_cache_update_lookback_ids,
        defaults.incremental_cache_update_lookback_ids,
    )?;
    let cache_full_refresh_interval_ms = positive_config_value(
        "cache_full_refresh_interval_ms",
        raw.cache_full_refresh_interval_ms,
        defaults.cache_full_refresh_interval_ms,
    )?;
    let use_incremental_cache_updates = raw.use_incremental_cache_updates.unwrap_or(false);

    Ok(RestrictedPathsManifestIdStoreConfig {
        use_manifest_id_cache: raw
            .use_manifest_id_cache
            .unwrap_or(defaults.use_manifest_id_cache),
        cache_update_interval_ms,
        use_incremental_cache_updates,
        incremental_cache_update_lookback_ids,
        cache_full_refresh_interval_ms,
    })
}

fn positive_config_value(field_name: &str, value: Option<i64>, default: u64) -> Result<u64> {
    let value = value
        .map(u64::try_from)
        .transpose()
        .with_context(|| format!("{field_name} must be positive"))?
        .unwrap_or(default);
    if value == 0 {
        bail!("{field_name} must be positive");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;
    use repos::RawDerivationPipelineConfig;

    use super::*;

    fn raw_pipeline_config_with_batch_size(batch_size: i64) -> RawDerivationPipelineConfig {
        RawDerivationPipelineConfig {
            types: Default::default(),
            bookmarks: Default::default(),
            stages: Default::default(),
            batch_size,
        }
    }

    #[mononoke::test]
    fn test_parse_xdb_mapping_shard_ids_drops_unknown_types() {
        // Given a shard-id config that names one type this binary knows and one
        // it does not (e.g. a type still rolling out to all binaries).
        let raw = RawDerivedDataTypesConfig {
            xdb_mapping_shard_ids: Some(std::collections::BTreeMap::from([
                ("blame_v3".to_string(), 0),
                ("totally_unknown_derived_data_type".to_string(), 7),
            ])),
            ..Default::default()
        };

        // When converting the raw config into Mononoke's typed config.
        let config: DerivedDataTypesConfig = raw.convert().unwrap();

        // Then the unknown type is dropped rather than failing the whole config
        // load, while the known type is preserved.
        assert_eq!(config.xdb_mapping_shard_ids.len(), 1);
        assert_eq!(
            config.xdb_mapping_shard_ids.get(&DerivableType::BlameV3),
            Some(&0usize),
        );
    }

    #[mononoke::test]
    fn test_parse_enabled_types_drops_unknown_types() {
        // Given an enabled-types list naming one type this binary knows and one
        // it does not (e.g. a type still rolling out to all binaries).
        let raw = RawDerivedDataTypesConfig {
            types: std::collections::BTreeSet::from([
                "fastlog".to_string(),
                "totally_unknown_derived_data_type".to_string(),
            ]),
            ..Default::default()
        };

        // When converting the raw config into Mononoke's typed config.
        let config: DerivedDataTypesConfig = raw.convert().unwrap();

        // Then the unknown type is dropped rather than failing the whole config
        // load, while the known type stays enabled.
        assert_eq!(config.types.len(), 1);
        assert!(config.types.contains(&DerivableType::Fastlog));
    }

    #[mononoke::test]
    fn test_parse_pipeline_config_rejects_zero_batch_size() {
        let raw = raw_pipeline_config_with_batch_size(0);
        let err = raw.convert().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("batch_size must be a positive integer") && msg.contains("got 0"),
            "expected positive-batch-size error, got: {msg}",
        );
    }

    #[mononoke::test]
    fn test_parse_pipeline_config_rejects_negative_batch_size() {
        let raw = raw_pipeline_config_with_batch_size(-5);
        let err = raw.convert().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("batch_size must be a positive integer") && msg.contains("got -5"),
            "expected positive-batch-size error, got: {msg}",
        );
    }

    fn empty_raw_restricted_paths_config() -> RawRestrictedPathsConfig {
        RawRestrictedPathsConfig::default()
    }

    /// What it tests: refresh intervals and lookback cannot disable safety throttles.
    /// Expected: zero and negative numeric values are rejected with their field name.
    #[mononoke::test]
    fn test_restricted_paths_manifest_id_store_config_rejects_non_positive_values() -> Result<()> {
        [
            ("cache_update_interval_ms", 0),
            ("cache_update_interval_ms", -1),
            ("incremental_cache_update_lookback_ids", 0),
            ("incremental_cache_update_lookback_ids", -1),
            ("cache_full_refresh_interval_ms", 0),
            ("cache_full_refresh_interval_ms", -1),
        ]
        .into_iter()
        .try_for_each(|(field_name, value)| {
            let mut nested = RawRestrictedPathsManifestIdStoreConfig::default();
            match field_name {
                "cache_update_interval_ms" => nested.cache_update_interval_ms = Some(value),
                "incremental_cache_update_lookback_ids" => {
                    nested.incremental_cache_update_lookback_ids = Some(value)
                }
                "cache_full_refresh_interval_ms" => {
                    nested.cache_full_refresh_interval_ms = Some(value)
                }
                _ => bail!("unexpected test field {field_name}"),
            }
            let mut raw = empty_raw_restricted_paths_config();
            raw.manifest_id_store_config = Some(nested);

            let error = <RawRestrictedPathsConfig as Convert>::convert(raw)
                .err()
                .ok_or_else(|| anyhow!("expected {field_name}={value} to fail"))?;
            anyhow::ensure!(
                format!("{error:#}").contains(field_name),
                "expected error to name {field_name}, got: {error:#}"
            );
            Ok(())
        })
    }

    #[mononoke::test]
    fn test_parse_acl_manifest_mode_absent_defaults_to_disabled() {
        assert_eq!(
            parse_acl_manifest_mode(None).unwrap(),
            AclManifestMode::Disabled
        );
    }

    #[mononoke::test]
    fn test_parse_enforcement_enabled_absent_defaults_to_false() {
        let cfg: RestrictedPathsConfig = empty_raw_restricted_paths_config().convert().unwrap();
        assert!(!cfg.enforcement_enabled);
    }

    #[mononoke::test]
    fn test_parse_restriction_acls_passthrough() {
        let raw_set = RawEnforcementConditionSet {
            restriction_acls: Some(vec![
                "USER:test_user".to_string(),
                "GROUP:test_group".to_string(),
            ]),
            ..Default::default()
        };
        let mut raw = empty_raw_restricted_paths_config();
        raw.enforcement_condition_sets = Some(vec![raw_set]);
        let cfg: RestrictedPathsConfig = raw.convert().unwrap();
        assert_eq!(cfg.enforcement_condition_sets.len(), 1);
        assert_eq!(cfg.enforcement_condition_sets[0].restriction_acls.len(), 2);
    }

    #[mononoke::test]
    fn test_parse_restriction_acls_invalid_value_errors_with_value_in_message() {
        let raw_set = RawEnforcementConditionSet {
            restriction_acls: Some(vec!["bogus".to_string()]),
            ..Default::default()
        };
        let mut raw = empty_raw_restricted_paths_config();
        raw.enforcement_condition_sets = Some(vec![raw_set]);

        let result: Result<RestrictedPathsConfig> = raw.convert();
        let err = result.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("restriction_acl `bogus`"),
            "error should contain offending value: {msg}"
        );
    }

    #[mononoke::test]
    fn test_parse_client_identity_regexes_passthrough() {
        let raw_set = RawEnforcementConditionSet {
            client_identity_regexes: Some(vec!["^USER:foo$".to_string()]),
            ..Default::default()
        };
        let mut raw = empty_raw_restricted_paths_config();
        raw.enforcement_condition_sets = Some(vec![raw_set]);
        let cfg: RestrictedPathsConfig = raw.convert().unwrap();
        assert_eq!(cfg.enforcement_condition_sets.len(), 1);
        let regexes = &cfg.enforcement_condition_sets[0]
            .matchers
            .client_identity_regexes;
        assert_eq!(regexes.len(), 1, "expected exactly one compiled regex");
        assert_eq!(regexes[0].as_str(), "^USER:foo$");
        assert!(
            regexes[0].is_match("USER:foo"),
            "compiled regex should match the canonical identity Display form",
        );
    }

    #[mononoke::test]
    fn test_parse_is_agent_passthrough() {
        let raw_sets = vec![
            RawEnforcementConditionSet {
                is_agent: Some(true),
                ..Default::default()
            },
            RawEnforcementConditionSet {
                is_agent: Some(false),
                ..Default::default()
            },
            RawEnforcementConditionSet {
                is_agent: None,
                ..Default::default()
            },
        ];
        let mut raw = empty_raw_restricted_paths_config();
        raw.enforcement_condition_sets = Some(raw_sets);
        let cfg: RestrictedPathsConfig = raw.convert().unwrap();
        assert_eq!(cfg.enforcement_condition_sets.len(), 3);
        assert_eq!(
            cfg.enforcement_condition_sets[0].matchers.is_agent,
            Some(true)
        );
        assert_eq!(
            cfg.enforcement_condition_sets[1].matchers.is_agent,
            Some(false)
        );
        assert_eq!(cfg.enforcement_condition_sets[2].matchers.is_agent, None);
    }

    /// What it tests: exemption sets are parsed with the same matchers as
    /// condition sets.
    /// Expected: request-local matchers pass through unchanged, and an absent
    /// list parses as no exemptions.
    #[mononoke::test]
    fn test_parse_enforcement_exemption_sets_passthrough() -> Result<()> {
        let cfg: RestrictedPathsConfig = empty_raw_restricted_paths_config().convert()?;
        assert!(
            cfg.enforcement_exemption_sets.is_empty(),
            "an absent list should parse as no exemptions"
        );

        let mut raw = empty_raw_restricted_paths_config();
        raw.enforcement_exemption_sets = Some(vec![RawEnforcementConditionSet {
            entry_points: Some(vec!["scs".to_string()]),
            machine_tiers: Some(vec!["od".to_string()]),
            build_rules: Some(vec!["fbcode:foo:bar".to_string()]),
            client_identity_regexes: Some(vec!["^SERVICE_IDENTITY:svc$".to_string()]),
            is_agent: Some(false),
            ..Default::default()
        }]);
        let cfg: RestrictedPathsConfig = raw.convert()?;

        let [exemption] = cfg.enforcement_exemption_sets.as_slice() else {
            bail!(
                "expected one exemption, got {:?}",
                cfg.enforcement_exemption_sets
            );
        };
        assert_eq!(
            exemption,
            &EnforcementExemptionSet::new(RequestMatchers {
                entry_points: vec!["scs".to_string()],
                machine_tiers: vec!["od".to_string()],
                build_rules: vec!["fbcode:foo:bar".to_string()],
                client_identity_regexes: vec![ComparableRegex::new("^SERVICE_IDENTITY:svc$")?],
                is_agent: Some(false),
            })?,
            "every request-local matcher should pass through unchanged"
        );
        Ok(())
    }

    /// What it tests: exemption sets that could match more than intended are
    /// rejected at parse time.
    /// Expected: `always_enabled`, `require_client_request_flag`,
    /// `restriction_acls`, the deprecated `restriction_roots` and a set with no
    /// matcher each fail with an error naming the offending exemption and field.
    #[mononoke::test]
    fn test_parse_enforcement_exemption_sets_rejects_unsafe_sets() -> Result<()> {
        let scs_only = || RawEnforcementConditionSet {
            entry_points: Some(vec!["scs".to_string()]),
            ..Default::default()
        };
        let cases = [
            (
                RawEnforcementConditionSet {
                    always_enabled: Some(true),
                    ..scs_only()
                },
                "`always_enabled`",
            ),
            (
                RawEnforcementConditionSet {
                    require_client_request_flag: Some(true),
                    ..scs_only()
                },
                "`require_client_request_flag`",
            ),
            (
                RawEnforcementConditionSet {
                    restriction_acls: Some(vec!["REPO_REGION:tent".to_string()]),
                    ..scs_only()
                },
                "`restriction_acls`",
            ),
            (
                RawEnforcementConditionSet {
                    restriction_roots: Some(vec!["secret/tent".to_string()]),
                    ..scs_only()
                },
                "`restriction_roots`",
            ),
            (
                RawEnforcementConditionSet {
                    always_enabled: Some(false),
                    require_client_request_flag: Some(false),
                    ..Default::default()
                },
                "must set at least one of",
            ),
        ];

        cases.into_iter().try_for_each(|(exemption, expected)| {
            let mut raw = empty_raw_restricted_paths_config();
            raw.enforcement_exemption_sets = Some(vec![scs_only(), exemption]);
            let error = <RawRestrictedPathsConfig as Convert>::convert(raw)
                .err()
                .ok_or_else(|| anyhow!("expected exemption rejected for {expected}"))?;
            let message = format!("{error:#}");
            ensure!(
                message.contains("enforcement_exemption_sets[1]") && message.contains(expected),
                "expected error naming enforcement_exemption_sets[1] and {expected}, got: {message}"
            );
            Ok(())
        })
    }

    /// What it tests: `false` is the same as unset for the boolean fields an
    /// exemption may not enable.
    /// Expected: an exemption with explicit `false` values and a real matcher
    /// parses.
    #[mononoke::test]
    fn test_parse_enforcement_exemption_sets_accepts_explicit_false() -> Result<()> {
        let mut raw = empty_raw_restricted_paths_config();
        raw.enforcement_exemption_sets = Some(vec![RawEnforcementConditionSet {
            always_enabled: Some(false),
            require_client_request_flag: Some(false),
            is_agent: Some(true),
            ..Default::default()
        }]);
        let cfg: RestrictedPathsConfig = raw.convert()?;
        assert_eq!(
            cfg.enforcement_exemption_sets.len(),
            1,
            "explicit `false` should be accepted like an unset field"
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_parse_denial_message_passthrough() {
        let cfg: RestrictedPathsConfig = empty_raw_restricted_paths_config().convert().unwrap();
        assert_eq!(cfg.denial_message, None);

        let mut raw = empty_raw_restricted_paths_config();
        raw.denial_message = Some("see https://fburl.com/example".to_string());
        let cfg: RestrictedPathsConfig = raw.convert().unwrap();
        assert_eq!(
            cfg.denial_message.as_deref(),
            Some("see https://fburl.com/example")
        );
    }

    #[mononoke::test]
    fn test_parse_client_identity_regexes_invalid_value_errors_with_value_in_message() {
        let raw_set = RawEnforcementConditionSet {
            client_identity_regexes: Some(vec!["[".to_string()]),
            ..Default::default()
        };
        let mut raw = empty_raw_restricted_paths_config();
        raw.enforcement_condition_sets = Some(vec![raw_set]);

        let result: Result<RestrictedPathsConfig> = raw.convert();
        let err = result.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("client_identity_regex `[`"),
            "error should contain offending value: {msg}"
        );
    }

    fn raw_metadata(
        repo_region_acl: &str,
        permission_request_group: Option<&str>,
        read_only: Option<bool>,
    ) -> RawPathRestrictionMetadata {
        RawPathRestrictionMetadata {
            repo_region_acl: repo_region_acl.to_string(),
            permission_request_group: permission_request_group.map(|s| s.to_string()),
            read_only,
            ..Default::default()
        }
    }

    /// What it tests: a `path_restriction_metadata` entry with explicit request
    /// group and `read_only = true`.
    /// Expected: all explicit fields are preserved.
    #[mononoke::test]
    fn test_path_restriction_metadata_explicit_fields() {
        let mut raw = empty_raw_restricted_paths_config();
        raw.path_restriction_metadata = Some(
            [(
                "foo".to_string(),
                raw_metadata("REPO_REGION:acl1", Some("GROUP:reviewers"), Some(true)),
            )]
            .into_iter()
            .collect(),
        );

        let cfg = raw.convert().unwrap();

        let path = NonRootMPath::new("foo").unwrap();
        let md = cfg.path_restriction_metadata.get(&path).expect("entry");
        assert_eq!(md.repo_region_acl.to_string(), "REPO_REGION:acl1");
        assert_eq!(
            md.permission_request_group.as_ref().map(|i| i.to_string()),
            Some("GROUP:reviewers".to_string())
        );
        assert!(md.read_only, "explicit read_only=true is honored");
    }

    /// What it tests: a `path_restriction_metadata` entry with only the required
    /// `repo_region_acl` set.
    /// Expected: the optional fields take their documented defaults — no request
    /// group, and not read-only.
    #[mononoke::test]
    fn test_path_restriction_metadata_optional_field_defaults() {
        let mut raw = empty_raw_restricted_paths_config();
        raw.path_restriction_metadata = Some(
            [(
                "foo/bar".to_string(),
                raw_metadata("REPO_REGION:acl1", None, None),
            )]
            .into_iter()
            .collect(),
        );

        let cfg = raw.convert().unwrap();

        let path = NonRootMPath::new("foo/bar").unwrap();
        let md = cfg.path_restriction_metadata.get(&path).expect("entry");
        assert_eq!(md.repo_region_acl.to_string(), "REPO_REGION:acl1");
        assert_eq!(md.permission_request_group, None);
        assert_eq!(
            md.rollout_allowlist_group, None,
            "no rollout allowlist when the field is unset"
        );
        assert!(!md.read_only, "read_only defaults to false when unset");
    }

    fn raw_metadata_with_rollout_group(
        rollout_allowlist_group: &str,
    ) -> RawPathRestrictionMetadata {
        RawPathRestrictionMetadata {
            repo_region_acl: "REPO_REGION:acl1".to_string(),
            rollout_allowlist_group: Some(rollout_allowlist_group.to_string()),
            ..Default::default()
        }
    }

    fn convert_with_rollout_group(rollout_allowlist_group: &str) -> Result<RestrictedPathsConfig> {
        let mut raw = empty_raw_restricted_paths_config();
        raw.path_restriction_metadata = Some(
            [(
                "foo".to_string(),
                raw_metadata_with_rollout_group(rollout_allowlist_group),
            )]
            .into_iter()
            .collect(),
        );
        raw.convert()
    }

    /// What it tests: a bare `rollout_allowlist_group` name is prefixed to a
    /// `GROUP:` identity, matching how `admin_bypass_group` is spelled in config.
    /// Expected: `titan_rollout` becomes `GROUP:titan_rollout`.
    #[mononoke::test]
    fn test_rollout_allowlist_group_is_prefixed() {
        let cfg = convert_with_rollout_group("titan_rollout").unwrap();

        let path = NonRootMPath::new("foo").unwrap();
        let md = cfg.path_restriction_metadata.get(&path).expect("entry");
        assert_eq!(
            md.rollout_allowlist_group.as_ref().map(|i| i.to_string()),
            Some("GROUP:titan_rollout".to_string()),
        );
    }

    /// What it tests: malformed `rollout_allowlist_group` values.
    /// Expected: every one fails config load rather than producing an identity
    /// that silently never matches, which would leave a tent owner believing an
    /// allowlist is active when it is not.
    #[mononoke::test]
    fn test_malformed_rollout_allowlist_group_fails_closed() {
        // (value, fragment the error must mention)
        let rejected = [
            ("", "must not be empty"),
            ("   ", "whitespace"),
            (" titan_rollout", "whitespace"),
            ("titan_rollout ", "whitespace"),
            ("GROUP:titan_rollout", "omit the `GROUP:` prefix"),
            ("USER:alice", "omit the `GROUP:` prefix"),
        ];

        for (value, expected_fragment) in rejected {
            let err = convert_with_rollout_group(value)
                .expect_err(&format!("`{value}` should be rejected"));
            let msg = format!("{err:#}");
            assert!(
                msg.contains(expected_fragment),
                "error for `{value}` should mention {expected_fragment:?}, got: {msg}"
            );
            assert!(
                msg.contains("foo"),
                "error for `{value}` should name the offending path, got: {msg}"
            );
        }
    }

    /// What it tests: a malformed `repo_region_acl` string in
    /// `path_restriction_metadata`.
    /// Expected: config load fails (fail-closed), not a silent drop that would
    /// leave the path unrestricted.
    #[mononoke::test]
    fn test_malformed_repo_region_acl_fails_closed() {
        let mut raw = empty_raw_restricted_paths_config();
        raw.path_restriction_metadata = Some(
            [(
                "foo".to_string(),
                raw_metadata("not-an-identity", None, None),
            )]
            .into_iter()
            .collect(),
        );

        let err = raw.convert().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("repo_region_acl") && msg.contains("foo"),
            "expected parse error naming the field and path, got: {msg}"
        );
    }

    /// What it tests: a malformed path key in `path_restriction_metadata`.
    /// Expected: config load fails (fail-closed).
    #[mononoke::test]
    fn test_malformed_path_key_fails_closed() {
        let mut raw = empty_raw_restricted_paths_config();
        raw.path_restriction_metadata = Some(
            [("".to_string(), raw_metadata("REPO_REGION:acl1", None, None))]
                .into_iter()
                .collect(),
        );

        let err = raw.convert().unwrap_err();
        assert!(format!("{err:#}").contains("Invalid path"));
    }

    /// What it tests: a malformed `repo_region_acl` string in new-field metadata.
    /// Expected: config load fails (fail-closed), not a silent drop.
    #[mononoke::test]
    fn test_merge_malformed_repo_region_acl_fails_closed() {
        let mut raw = empty_raw_restricted_paths_config();
        raw.path_restriction_metadata = Some(
            [(
                "foo".to_string(),
                raw_metadata("not-an-identity", None, None),
            )]
            .into_iter()
            .collect(),
        );

        let err = raw.convert().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("repo_region_acl") && msg.contains("foo"),
            "expected repo_region_acl parse error, got: {msg}"
        );
    }

    /// What it tests: `read_only` absent on a metadata entry.
    /// Expected: it defaults to false.
    #[mononoke::test]
    fn test_merge_read_only_absent_defaults_false() {
        let mut raw = empty_raw_restricted_paths_config();
        raw.path_restriction_metadata = Some(
            [(
                "foo".to_string(),
                raw_metadata("REPO_REGION:acl1", None, None),
            )]
            .into_iter()
            .collect(),
        );

        let cfg = raw.convert().unwrap();

        let path = NonRootMPath::new("foo").unwrap();
        assert!(!cfg.path_restriction_metadata.get(&path).unwrap().read_only);
    }
}
