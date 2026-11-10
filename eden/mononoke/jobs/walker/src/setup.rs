/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroU64;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Error;
use anyhow::bail;
use anyhow::format_err;
use blobstore::Blobstore;
use blobstore_factory::ScrubHandler;
use context::CoreContext;
use fbinit::FacebookInit;
use metaconfig_types::CommonConfig;
use metaconfig_types::MetadataDatabaseConfig;
use metaconfig_types::Redaction;
use metaconfig_types::RepoConfig;
use metaconfig_types::RepoConfigSource;
use metaconfig_types::WalkerJobParams;
use metaconfig_types::WalkerJobType;
use mononoke_app::MononokeApp;
use mononoke_app::args::MultiRepoArgs;
use mononoke_types::repo::RepositoryId;
use newfilenodes::NewFilenodesBuilder;
use repo_factory::RepoFactory;
use samplingblob::ComponentSamplingHandler;
use samplingblob::SamplingBlobstore;
use samplingblob::SamplingHandler;
use scuba_ext::MononokeScubaSampleBuilder;
use sql_ext::facebook::MysqlOptions;
use tracing::info;
use tracing::warn;

use crate::WalkerArgs;
use crate::args::NodeTypeArg;
use crate::args::TailArgs;
use crate::args::WalkerCommonArgs;
use crate::args::WalkerGraphParams;
use crate::commands::JobParams;
use crate::commands::JobWalkParams;
use crate::commands::RepoSubcommandParams;
use crate::detail::blobstore::StatsScrubHandler;
use crate::detail::blobstore::replace_blobconfig;
use crate::detail::graph::EdgeType;
use crate::detail::graph::Node;
use crate::detail::graph::NodeType;
use crate::detail::graph::SqlShardInfo;
use crate::detail::progress::ProgressOptions;
use crate::detail::progress::ProgressStateCountByType;
use crate::detail::progress::ProgressStateMutex;
use crate::detail::progress::sort_by_string;
use crate::detail::repo::Repo;
use crate::detail::tail::TailParams;
use crate::detail::validate::REPO;
use crate::detail::validate::WALK_TYPE;
use crate::detail::walk::OutgoingEdge;
use crate::detail::walk::RepoWalkParams;
const CHECKPOINT_PREFIX: &str = "mononoke_sharded_walker";

pub async fn setup_common(
    walk_stats_key: &'static str,
    app: &MononokeApp,
    repo_args: &MultiRepoArgs,
    common_args: &WalkerCommonArgs,
    blobstore_sampler: Option<Arc<dyn SamplingHandler>>,
    blobstore_component_sampler: Option<Arc<dyn ComponentSamplingHandler>>,
) -> Result<JobParams, Error> {
    let ctx = app.new_basic_context();
    let mut scuba_builder = app.environment().scuba_sample_builder.clone();
    let walker_type = app.args::<WalkerArgs>()?.walker_type;
    scuba_builder.add(WALK_TYPE, walk_stats_key);

    let WalkerGraphParams {
        include_node_types,
        include_edge_types,
        error_as_data_node_types,
        error_as_data_edge_types,
    } = common_args.graph_params.parse_args()?;

    if !error_as_data_node_types.is_empty() || !error_as_data_edge_types.is_empty() {
        if !app.readonly_storage().0 {
            return Err(format_err!(
                "Error as data could mean internal state is invalid, run with --with-readonly-storage=true to ensure no risk of persisting it"
            ));
        }
        warn!(
            "Error as data enabled, walk results may not be complete. Errors as data enabled for node types {:?} edge types {:?}",
            sort_by_string(&error_as_data_node_types),
            sort_by_string(&error_as_data_edge_types)
        );
    }

    // There is no need to check if repos is empty: at least one repo arg
    // is required when running the command.
    let repos = app.multi_repo_configs(repo_args.ids_or_names()?)?;
    let repo_count = repos.len();
    if repo_count > 1 {
        info!(
            "Walking repos {:?}",
            repos.iter().map(|repo| &repo.0).collect::<Vec<_>>()
        );
    }

    let repos = override_repo_configs(walk_stats_key, app, common_args, repos)?;

    // configure repo factory
    let repo_id_to_name: HashMap<_, _> = repos
        .iter()
        .map(|(name, conf)| (conf.repoid, name.clone()))
        .collect();
    let repo_factory = setup_repo_factory(
        walk_stats_key,
        app,
        repo_id_to_name,
        blobstore_sampler,
        blobstore_component_sampler,
        scuba_builder.clone(),
        common_args.quiet,
    );

    let progress_options = common_args.progress.parse_args();
    let hash_validation_node_types = common_args.hash_validation.parse_args();

    let mysql_options = app.mysql_options();

    let walk_roots = common_args.walk_roots.parse_args()?;
    let exclude_nodes = common_args.exclude_nodes.parse_args()?;
    let mut parsed_tail_params = parse_tail_params(
        &ctx,
        &common_args.tailing,
        mysql_options,
        &repos,
        &walk_roots,
    )
    .await?;

    let mut per_repo = Vec::new();
    let mut error_as_data_node_types_for_all_repos = error_as_data_node_types;
    for (repo, repo_conf) in repos {
        let metadatadb_config = &repo_conf.storage_config.metadata;
        let tail_params = parsed_tail_params
            .get_mut(metadatadb_config)
            .ok_or_else(|| format_err!("No tail params for {repo}"))?;

        // repo factory reuses sql factory if one was already initiated for the config
        let sql_factory = repo_factory.sql_factory(metadatadb_config).await?;
        let sql_shard_info = SqlShardInfo {
            filenodes: sql_factory.tier_info_shardable::<NewFilenodesBuilder>()?,
            active_keys_per_shard: mysql_options.per_key_limit(),
        };

        let walker_config_params = walker_type
            .as_ref()
            .and_then(|job_type| walker_config_params(&repo_conf, job_type));
        // Concurrency is primarily provided by config and then by
        // CLI in case config value is absent.
        let scheduled_max_concurrency = walker_config_params
            .and_then(|p| p.scheduled_max_concurrency.map(|i| i as usize))
            .unwrap_or(common_args.scheduled_max);
        // Exclude nodes that might be provided as part of walker config.
        let included_nodes = walker_config_params
            .and_then(|p| p.exclude_node_type.as_ref())
            .map(|s| s.parse::<NodeTypeArg>())
            .transpose()?
            .map(|n| HashSet::<NodeType>::from_iter(n.0.iter().cloned()))
            .map_or_else(
                || include_node_types.clone(),
                |excluded_nodes| {
                    include_node_types
                        .difference(&excluded_nodes)
                        .copied()
                        .collect()
                },
            );

        if let Some(ref mut chunking) = tail_params.chunking {
            // Allow Remaining Deferred = True if either the CLI or the walker
            // config say so.
            chunking.allow_remaining_deferred |=
                walker_config_params.is_some_and(|p| p.allow_remaining_deferred);
            // If the type of walker is specified, the checkpoint name should be
            // a combination of checkpoint_prefix + walker_type + repo_name.
            if let Some(walker_type) = walker_type {
                if let Some(ref mut checkpoints) = chunking.checkpoints {
                    checkpoints.checkpoint_name =
                        format!("{CHECKPOINT_PREFIX}_{walker_type}_{repo}");
                }
            }
        }
        // NOTE: error_as_data_node_types is an argument that can be specified for
        // individual repos but the walker just assumes one universal value for it even
        // when executing for multiple repos. For sharded execution, having per-repo and
        // all-repo value for error_as_data_node_types will behave the same since the entire
        // setup is done once-per-repo in sharded setting. In CLI, this behavior is enforced
        // by requiring all repos executing together to have the same value for this field.
        error_as_data_node_types_for_all_repos = walker_config_params
            .and_then(|p| p.error_as_node_data_type.as_ref())
            .map(|s| s.parse::<NodeTypeArg>())
            .transpose()?
            .map_or(error_as_data_node_types_for_all_repos, |n| {
                HashSet::<NodeType>::from_iter(n.0.iter().cloned())
            });

        let common_config = app.repo_configs().common.clone();
        let one_repo = setup_repo(
            walk_stats_key,
            app.fb,
            &repo_factory,
            scuba_builder.clone(),
            sql_shard_info,
            scheduled_max_concurrency,
            repo_count,
            repo.clone(),
            &repo_conf,
            walk_roots.clone(),
            exclude_nodes.clone(),
            tail_params.clone(),
            include_edge_types.clone(),
            included_nodes,
            hash_validation_node_types.clone(),
            progress_options,
            common_config,
        )
        .await?;
        per_repo.push(one_repo);
    }

    Ok(JobParams {
        walk_params: JobWalkParams {
            enable_derive: common_args.enable_derive,
            quiet: common_args.quiet,
            error_as_data_node_types: error_as_data_node_types_for_all_repos,
            error_as_data_edge_types,
        },
        per_repo,
    })
}

/// Fetch the configuration parameters specific to a particular variant
/// of the walker job.
pub fn walker_config_params<'a>(
    repo_config: &'a RepoConfig,
    job_type: &'a WalkerJobType,
) -> Option<&'a WalkerJobParams> {
    if let Some(config) = repo_config.walker_config.as_ref() {
        if let Some(params) = config.params.as_ref() {
            return params
                .iter()
                .find_map(|(k, v)| if *k == *job_type { Some(v) } else { None });
        }
    }
    None
}

// Override the blobstore config so we can do things like run on one side of a multiplex
fn override_repo_configs(
    walk_stats_key: &'static str,
    app: &MononokeApp,
    common_args: &WalkerCommonArgs,
    mut repos: Vec<(String, RepoConfig)>,
) -> Result<Vec<(String, RepoConfig)>, Error> {
    let storage_override = if let Some(storage_id) = &common_args.storage_id {
        let storage_configs = &app.storage_configs().storage;
        let config_opt = storage_configs.get(storage_id).cloned();
        let storage_config = config_opt.ok_or_else(|| {
            format_err!(
                "Storage id `{}` not found in {:?}",
                storage_id,
                storage_configs.keys()
            )
        })?;
        Some(storage_config)
    } else {
        None
    };

    for (name, config) in &mut repos {
        if let Some(storage_config) = storage_override.clone() {
            config.storage_config = storage_config;
        }
        replace_blobconfig(
            &mut config.storage_config.blobstore,
            common_args.inner_blobstore_id,
            name,
            walk_stats_key,
            app.blobstore_options().scrub_options.is_some(),
        )?;

        let sampling_multiplier =
            NonZeroU64::new(common_args.blobstore_sampling_multiplier).context("Cannot be zero")?;
        config
            .storage_config
            .blobstore
            .apply_sampling_multiplier(sampling_multiplier);
    }

    // Disable redaction unless we are running with it enabled.
    if !common_args.enable_redaction {
        for (_repo, config) in &mut repos {
            config.redaction = Redaction::Disabled;
        }
    };

    Ok(repos)
}

fn setup_repo_factory(
    walk_stats_key: &'static str,
    app: &MononokeApp,
    repo_id_to_name: HashMap<RepositoryId, String>,
    blobstore_sampler: Option<Arc<dyn SamplingHandler>>,
    blobstore_component_sampler: Option<Arc<dyn ComponentSamplingHandler>>,
    scuba_builder: MononokeScubaSampleBuilder,
    quiet: bool,
) -> RepoFactory {
    // We want to customize the repo factory, so take a deep clone
    // of the factory on the App.
    let mut repo_factory = RepoFactory::clone(app.repo_factory());
    if let Some(blobstore_sampler) = blobstore_sampler.clone() {
        repo_factory.with_blobstore_override({
            move |blobstore| -> Arc<dyn Blobstore> {
                if !quiet {
                    info!("Sampling from blobstore: {}", blobstore);
                }
                Arc::new(SamplingBlobstore::new(blobstore, blobstore_sampler.clone()))
            }
        });
    }

    if let Some(sampler) = blobstore_component_sampler {
        repo_factory.with_blobstore_component_sampler(sampler);
    }

    repo_factory.with_scrub_handler(Arc::new(StatsScrubHandler::new(
        false,
        scuba_builder,
        walk_stats_key,
        repo_id_to_name,
    )) as Arc<dyn ScrubHandler>);

    repo_factory
}

async fn parse_tail_params(
    ctx: &CoreContext,
    tail_args: &TailArgs,
    mysql_options: &MysqlOptions,
    repos: &[(String, RepoConfig)],
    walk_roots: &[OutgoingEdge],
) -> Result<HashMap<MetadataDatabaseConfig, TailParams>, Error> {
    let mut parsed_tail_params: HashMap<MetadataDatabaseConfig, TailParams> = HashMap::new();
    for (_repo, repo_conf) in repos {
        let metadatadb_config = &repo_conf.storage_config.metadata;
        let tail_params = match parsed_tail_params.get(metadatadb_config) {
            Some(tail_params) => tail_params.clone(),
            None => {
                let tail_params = tail_args
                    .parse_args(ctx, metadatadb_config, mysql_options)
                    .await?;
                parsed_tail_params.insert(metadatadb_config.clone(), tail_params.clone());
                tail_params
            }
        };

        if tail_params.chunking.is_none() && walk_roots.is_empty() {
            bail!(
                "No walk roots provided, pass with  --bookmark, --walk-root or --chunk-by-public",
            );
        }
    }

    Ok(parsed_tail_params)
}

// Setup for just one repo. Try and keep clap parsing out of here, should be done beforehand
async fn setup_repo<'a>(
    walk_stats_key: &'static str,
    fb: FacebookInit,
    repo_factory: &'a RepoFactory,
    mut scuba_builder: MononokeScubaSampleBuilder,
    sql_shard_info: SqlShardInfo,
    scheduled_max: usize,
    repo_count: usize,
    repo_name: String,
    repo_config: &'a RepoConfig,
    walk_roots: Vec<OutgoingEdge>,
    exclude_nodes: HashSet<Node>,
    mut tail_params: TailParams,
    include_edge_types: HashSet<EdgeType>,
    mut include_node_types: HashSet<NodeType>,
    hash_validation_node_types: HashSet<NodeType>,
    progress_options: ProgressOptions,
    common_config: CommonConfig,
) -> Result<(RepoSubcommandParams, RepoWalkParams), Error> {
    let scheduled_max = scheduled_max / repo_count;
    scuba_builder.add(REPO, repo_name.clone());

    // Only walk derived node types that the repo is configured to contain
    include_node_types.retain(|t| {
        if let Some(t) = t.derived_data_type() {
            repo_config.derived_data_config.is_enabled(t)
        } else {
            true
        }
    });

    let mut root_node_types: HashSet<_> =
        walk_roots.iter().map(|e| e.label.outgoing_type()).collect();

    if let Some(ref mut chunking) = tail_params.chunking {
        chunking.chunk_by.retain(|t| {
            if let Some(t) = t.derived_data_type() {
                repo_config.derived_data_config.is_enabled(t)
            } else {
                true
            }
        });

        root_node_types.extend(chunking.chunk_by.iter().cloned());
    }

    let (include_edge_types, include_node_types) =
        reachable_graph_elements(include_edge_types, include_node_types, &root_node_types);
    info!(
        "Walking edge types {:?}",
        sort_by_string(&include_edge_types)
    );
    info!(
        "Walking node types {:?}",
        sort_by_string(&include_node_types)
    );

    scuba_builder.add(REPO, repo_name.clone());

    let mut progress_node_types = include_node_types.clone();
    for e in &walk_roots {
        progress_node_types.insert(e.target.get_type());
    }

    let progress_state = ProgressStateMutex::new(ProgressStateCountByType::new(
        fb,
        walk_stats_key,
        repo_name.clone(),
        progress_node_types,
        progress_options,
    ));

    let repo: Repo = repo_factory
        .build(
            repo_name.clone(),
            repo_config.clone(),
            common_config,
            RepoConfigSource::default(),
        )
        .await?;

    Ok((
        RepoSubcommandParams {
            progress_state,
            tail_params,
            lfs_threshold: repo_config.lfs.threshold,
        },
        RepoWalkParams {
            repo,
            scheduled_max,
            sql_shard_info,
            walk_roots,
            exclude_nodes,
            include_node_types,
            include_edge_types,
            hash_validation_node_types,
            scuba_builder,
        },
    ))
}

fn reachable_graph_elements(
    mut include_edge_types: HashSet<EdgeType>,
    mut include_node_types: HashSet<NodeType>,
    root_node_types: &HashSet<NodeType>,
) -> (HashSet<EdgeType>, HashSet<NodeType>) {
    // This stops us logging that we're walking unreachable edge/node types
    let mut param_count = include_edge_types.len() + include_node_types.len();
    let mut last_param_count = 0;
    while param_count != last_param_count {
        let include_edge_types_stable = include_edge_types.clone();
        // Only retain edge types that are traversable
        include_edge_types.retain(|e| {
            e.incoming_type()
                .is_none_or(|t|
                    // its an incoming_type we want
                    (include_node_types.contains(&t) || root_node_types.contains(&t)) &&
                    // Another existing edge can get us to this node type
                    (root_node_types.contains(&t) || include_edge_types_stable.iter().any(|o| o.outgoing_type() == t)))
                // its an outgoing_type we want
                && include_node_types.contains(&e.outgoing_type())
        });
        // Only retain node types we expect to step to after graph entry
        include_node_types.retain(|t| {
            include_edge_types
                .iter()
                .any(|e| &e.outgoing_type() == t || e.incoming_type().is_some_and(|ot| &ot == t))
        });
        last_param_count = param_count;
        param_count = include_edge_types.len() + include_node_types.len();
    }
    (include_edge_types, include_node_types)
}
