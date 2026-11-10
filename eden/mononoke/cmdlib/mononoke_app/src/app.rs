/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::any::TypeId;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

#[cfg(fbcode_build)]
use MononokeAppStats_ods3::Instrument_MononokeAppStats;
#[cfg(fbcode_build)]
use MononokeAppStats_ods3_types::MononokeAppStats;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use base_app::BaseApp;
use blobstore::Blobstore;
use blobstore::KeyedBlobstore;
use blobstore_factory::BlobstoreOptions;
use blobstore_factory::ReadOnlyStorage;
use cached_config::ConfigStore;
use clap::ArgMatches;
use clap::Error as ClapError;
use clap::FromArgMatches;
use context::CoreContext;
use environment::MononokeEnvironment;
use facet::AsyncBuildable;
use fbinit::FacebookInit;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures_util::try_join;
use metaconfig_parser::RepoConfigs;
use metaconfig_parser::StorageConfigs;
use metaconfig_types::BlobConfig;
use metaconfig_types::BlobstoreId;
use metaconfig_types::Redaction;
use metaconfig_types::RepoConfig;
use metaconfig_types::ShardedService;
use mononoke_configs::MononokeConfigs;
#[allow(unused)]
use mononoke_macros::mononoke;
use mononoke_types::RepositoryId;
use redactedblobstore::RedactedBlobstoreConfig;
use redactedblobstore::RedactionConfigBlobstore;
use repo_blobstore::RepoBlobstore;
use repo_factory::RepoFactory;
use repo_factory::RepoFactoryBuilder;
use running::ShutdownGracePeriod;
use running::run_until_terminated;
use scuba_ext::MononokeScubaSampleBuilder;
use services::Fb303Service;
use sql_ext::facebook::MysqlOptions;
use stats::prelude::*;
#[cfg(not(test))]
use stats::schedule_stats_aggregation_preview;
use tokio::runtime::Handle;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::args::AsRepoArg;
use crate::args::ConfigArgs;
use crate::args::ConfigMode;
use crate::args::MultiRepoArgs;
use crate::args::RepoArg;
use crate::args::RepoBlobstoreArgs;
use crate::args::SourceAndTargetRepoArgs;
use crate::extension::AppExtension;
use crate::extension::AppExtensionArgsBox;
use crate::extension::BoxedAppExtensionArgs;
use crate::monitoring::MonitoringAppExtension;
use crate::repos_manager::MononokeReposManager;
#[cfg(fbcode_build)]
use crate::server_status::MononokeAppServerStatus;

define_stats! {
    prefix = "mononoke.app";
    completion_duration_secs: timeseries(Average, Sum, Count),
}

pub struct MononokeApp {
    pub fb: FacebookInit,
    config_mode: ConfigMode,
    args: ArgMatches,
    runtime: Option<Runtime>,
    env: Arc<MononokeEnvironment>,
    extension_args: HashMap<TypeId, Box<dyn BoxedAppExtensionArgs>>,
    configs: Arc<MononokeConfigs>,
    repo_factory: Arc<RepoFactory>,
    quiescing: Arc<AtomicBool>,
}

impl BaseApp for MononokeApp {
    fn subcommand(&self) -> Option<(&str, &ArgMatches)> {
        self.args.subcommand()
    }
}

impl MononokeApp {
    pub(crate) fn new(
        fb: FacebookInit,
        config_mode: ConfigMode,
        args: ArgMatches,
        runtime: Runtime,
        env: MononokeEnvironment,
        extension_args: HashMap<TypeId, Box<dyn BoxedAppExtensionArgs>>,
    ) -> Result<Self> {
        let env = Arc::new(env);
        let configs = ConfigArgs::from_arg_matches(&args)?
            .create_mononoke_configs(runtime.handle().clone(), &env.as_ref().config_store)?;

        let repo_factory = Arc::new(RepoFactory::new(env.clone(), configs.clone()));

        Ok(MononokeApp {
            fb,
            config_mode,
            args,
            runtime: Some(runtime),
            env,
            extension_args,
            configs,
            repo_factory,
            quiescing: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn extension_args<Ext>(&self) -> Result<&Ext::Args>
    where
        Ext: AppExtension + 'static,
    {
        if let Some(ext) = self.extension_args.get(&TypeId::of::<Ext>()) {
            if let Some(ext) = ext.as_any().downcast_ref::<AppExtensionArgsBox<Ext>>() {
                return Ok(ext.args());
            }
        }
        Err(anyhow!(
            "Extension {} arguments not found (was it registered with MononokeApp?)",
            std::any::type_name::<Ext>(),
        ))
    }

    /// Start the monitoring server for the provided service.
    pub fn start_monitoring<Service>(
        &self,
        handle: &Handle,
        app_name: &str,
        service: Service,
    ) -> Result<()>
    where
        Service: Fb303Service + Sync + Send + 'static,
    {
        let monitoring_args = self.extension_args::<MonitoringAppExtension>()?;
        monitoring_args.start_monitoring_server(self.fb, handle, app_name, service)?;
        Ok(())
    }

    /// Start the background stats aggregation thread.
    pub fn start_stats_aggregation(&self) -> Result<()> {
        #[cfg(not(test))]
        {
            self.env.runtime.block_on(async move {
                let stats_aggregation = schedule_stats_aggregation_preview()
                    .map_err(|_| anyhow!("Failed to create stats aggregation worker"))?;
                mononoke::spawn_task(stats_aggregation);
                anyhow::Ok(())
            })?;
        }
        Ok(())
    }

    /// Execute a future on this app's runtime.
    ///
    /// If you are looking for a replacement for `cmdlib::helpers::block_execute`, prefer
    /// `run_with_monitoring_and_logging`.
    pub fn run_basic<F, Fut>(mut self, main: F) -> Result<()>
    where
        F: Fn(MononokeApp) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let runtime = self
            .runtime
            .take()
            .ok_or_else(|| anyhow!("MononokeApp already started"))?;
        runtime.block_on(main(self))
    }

    /// Execute a future on this app's runtime.
    ///
    /// This future will run with monitoring enabled, and errors will be logged to glog.
    pub fn run_with_monitoring_and_logging<F, Fut, Service>(
        mut self,
        main: F,
        app_name: &str,
        service: Service,
    ) -> Result<()>
    where
        F: Fn(MononokeApp) -> Fut,
        Fut: Future<Output = Result<()>>,
        Service: Fb303Service + Sync + Send + 'static,
    {
        let runtime = self
            .runtime
            .take()
            .ok_or_else(|| anyhow!("MononokeApp already started"))?;
        self.start_monitoring(runtime.handle(), app_name, service)?;
        self.start_stats_aggregation()?;

        let result = runtime.block_on(main(self));

        if let Err(e) = result {
            // Log error in glog format
            error!("Execution error: {:?}", e);

            // Replace the error with a simple error so it isn't logged twice.
            return Err(anyhow!("Execution failed"));
        }

        Ok(())
    }

    /// Status to serve from a Thrift server's `getStatus` interface, via
    /// `ThriftServerBuilder::with_custom_status_impl`.
    ///
    /// The status flips to `STOPPING` when this app starts to quiesce, so
    /// callers see the instance leave rotation for the whole of the shutdown
    /// grace period instead of only once the server stops.
    #[cfg(fbcode_build)]
    pub fn server_status(&self) -> MononokeAppServerStatus {
        MononokeAppServerStatus::new(self.quiescing.clone())
    }

    /// Run a server future, and wait until a termination signal is received.
    ///
    /// When the termination signal is received, the `quiesce` callback is
    /// called.  This should perform any steps required to quiesce the server,
    /// for example by removing this instance from routing configuration, or
    /// asking the load balancer to stop sending requests to this instance.
    /// Requests that do arrive should still be accepted.
    ///
    /// After the `shutdown_grace_period`, the `shutdown` future is awaited.
    /// This should do any additional work to stop accepting connections and wait
    /// until all outstanding requests have been handled. The `server` future
    /// continues to run while `shutdown` is being awaited.
    ///
    /// Once both `shutdown` and `server` have completed, the process
    /// exits. If `shutdown_timeout` is exceeded, the server future is canceled
    /// and an error is returned.
    pub fn run_until_terminated<ServerFn, ServerFut, QuiesceFn, ShutdownFut>(
        mut self,
        server: ServerFn,
        quiesce: QuiesceFn,
        shutdown_grace_period: impl ShutdownGracePeriod,
        shutdown: ShutdownFut,
        shutdown_timeout: Duration,
        requests_counter: Option<Arc<AtomicI64>>,
    ) -> Result<()>
    where
        ServerFn: FnOnce(MononokeApp) -> ServerFut + Send + 'static,
        ServerFut: Future<Output = Result<()>> + Send + 'static,
        QuiesceFn: FnOnce(),
        ShutdownFut: Future<Output = ()>,
    {
        let runtime = self
            .runtime
            .take()
            .ok_or_else(|| anyhow!("MononokeApp already started"))?;
        let quiescing = self.quiescing.clone();
        let server = async move { server(self).await };
        runtime.block_on(run_until_terminated(
            server,
            move || {
                quiescing.store(true, Ordering::Relaxed);
                quiesce();
            },
            shutdown_grace_period,
            shutdown,
            shutdown_timeout,
            requests_counter,
        ))
    }

    /// Wait until a termination signal is received.
    ///
    /// This method does not have a server future, and so is useful when all
    /// serving listeners are running on another executor (e.g. a C++
    /// executor for a thrift service).
    ///
    /// When the termination signal is received, the same quiesce-shutdown
    /// procedure as for `run_until_terminated` is followed.
    pub fn wait_until_terminated<QuiesceFn, ShutdownFut>(
        self,
        quiesce: QuiesceFn,
        shutdown_grace_period: impl ShutdownGracePeriod,
        shutdown: ShutdownFut,
        shutdown_timeout: Duration,
        requests_counter: Option<Arc<AtomicI64>>,
    ) -> Result<()>
    where
        QuiesceFn: FnOnce(),
        ShutdownFut: Future<Output = ()>,
    {
        let (exit_tx, exit_rx) = oneshot::channel();
        let server = move |app| async move {
            exit_rx.await?;
            drop(app);
            Ok(())
        };

        self.run_until_terminated(
            server,
            || {
                let _ = exit_tx.send(());
                quiesce();
            },
            shutdown_grace_period,
            shutdown,
            shutdown_timeout,
            requests_counter,
        )
    }

    /// Returns the selected subcommand of the app (if this app
    /// has subcommands).
    pub fn matches(&self) -> &ArgMatches {
        &self.args
    }

    /// Returns a parsed args struct based on the arguments provided
    /// on the command line.
    pub fn args<Args>(&self) -> Result<Args, ClapError>
    where
        Args: FromArgMatches,
    {
        Args::from_arg_matches(&self.args)
    }

    /// Returns a handle to this app's runtime.
    pub fn runtime(&self) -> &Handle {
        &self.env.runtime
    }

    /// The config store for this app.
    pub fn config_store(&self) -> &ConfigStore {
        &self.env.config_store
    }

    pub fn configs(&self) -> Arc<MononokeConfigs> {
        self.configs.clone()
    }

    /// The repo configs for this app.
    pub fn repo_configs(&self) -> Arc<RepoConfigs> {
        self.configs.repo_configs()
    }

    /// The storage configs for this app.
    pub fn storage_configs(&self) -> Arc<StorageConfigs> {
        self.configs.storage_configs()
    }

    /// The mysql options for this app.
    pub fn mysql_options(&self) -> &MysqlOptions {
        &self.env.mysql_options
    }

    /// The blobstore options for this app.
    pub fn blobstore_options(&self) -> &BlobstoreOptions {
        &self.env.blobstore_options
    }

    /// The readonly storage options for this app.
    pub fn readonly_storage(&self) -> &ReadOnlyStorage {
        &self.env.readonly_storage
    }

    /// Create a basic CoreContext.
    ///
    /// This is a good choice for simple CLI tools like admin.  It will
    /// contain basic logging, and also scuba logging if configured by the
    /// command line arguments.
    pub fn new_basic_context(&self) -> CoreContext {
        CoreContext::new_with_scuba(self.env.fb, self.env.scuba_sample_builder.clone())
    }

    /// Return repo factory used by app.
    pub fn repo_factory(&self) -> &Arc<RepoFactory> {
        &self.repo_factory
    }

    /// Mononoke environment for this app.
    pub fn environment(&self) -> &Arc<MononokeEnvironment> {
        &self.env
    }

    /// Returns true if this is a production configuration of Mononoke
    pub fn is_production(&self) -> bool {
        self.config_mode == ConfigMode::Production
    }

    pub fn repo_config_by_name(&self, repo_name: &str) -> Result<RepoConfig> {
        self.configs.get_or_load_repo_config(repo_name)
    }

    /// Get repo config based on user-provided arguments.
    pub fn repo_config(&self, repo_arg: &RepoArg) -> Result<(String, RepoConfig)> {
        match repo_arg {
            RepoArg::Id(repo_id) => self.configs.get_or_load_repo_config_by_id(repo_id.id()),
            RepoArg::Name(repo_name) => {
                let repo_config = self.repo_config_by_name(repo_name)?;
                Ok((repo_name.to_string(), repo_config))
            }
        }
    }

    /// Get repo id based on user-provided arguments.
    pub fn repo_id(&self, repo_arg: &RepoArg) -> Result<RepositoryId> {
        match repo_arg {
            RepoArg::Id(repo_id) => Ok(*repo_id),
            RepoArg::Name(repo_name) => {
                let repo_config = self.repo_config_by_name(repo_name)?;
                Ok(repo_config.repoid)
            }
        }
    }

    /// Get repo configs based on user-provided arguments.
    pub fn multi_repo_configs(&self, repo_args: Vec<RepoArg>) -> Result<Vec<(String, RepoConfig)>> {
        let mut repos = vec![];
        let mut unique_repos = HashSet::new();
        for repo in repo_args {
            let (name, repo_conf) = self.repo_config(&repo)?;
            if unique_repos.insert(name.clone()) {
                repos.push((name, repo_conf));
            }
        }

        Ok(repos)
    }

    /// Open repositories based on user-provided arguments.
    pub async fn open_repos<Repo>(&self, repos_args: &MultiRepoArgs) -> Result<Vec<Repo>>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>,
    {
        let args = repos_args.ids_or_names()?;
        let mut repos = vec![];
        for arg in args {
            repos.push(self.repo_config(&arg)?);
        }

        let repos: HashMap<_, _> = repos.into_iter().collect();
        let common_config = self.repo_configs().common.clone();
        let repos: Vec<_> = stream::iter(repos)
            .map(|(repo_name, repo_config)| {
                let repo_factory = self.repo_factory.clone();
                let common_config = common_config.clone();
                let source = self.configs.repo_config_source(&repo_name);
                async move {
                    repo_factory
                        .build(repo_name, repo_config, common_config, source)
                        .await
                }
            })
            .buffered(100)
            .try_collect()
            .await?;

        Ok(repos)
    }

    pub async fn maybe_open_repo<Repo>(
        &self,
        repo_args: &Option<impl AsRepoArg>,
    ) -> Result<Option<Repo>>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>,
    {
        match repo_args {
            Some(repo_args) => Ok(Some(self.open_repo(repo_args).await?)),
            None => Ok(None),
        }
    }

    /// Open a repository based on user-provided arguments.
    pub async fn open_repo<Repo>(&self, repo_args: &impl AsRepoArg) -> Result<Repo>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>,
    {
        let repo_arg = repo_args.as_repo_arg();
        let (repo_name, repo_config) = self.repo_config(repo_arg)?;
        let common_config = self.repo_configs().common.clone();
        let source = self.configs.repo_config_source(&repo_name);
        let repo = self
            .repo_factory
            .build(repo_name, repo_config, common_config, source)
            .await?;
        Ok(repo)
    }

    /// Open a named repository.
    /// FIXME should pass a String but I need to figure out how get the repo config
    pub async fn open_named_repo<Repo>(&self, repo_id: RepositoryId) -> Result<Repo>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>,
    {
        // Route through `get_or_load_repo_config_by_id` so split-loaded repos
        // (only present in the per-tier RepoSpec manifest) resolve correctly.
        let (repo_name, repo_config) = self
            .configs
            .get_or_load_repo_config_by_id(repo_id.id())
            .with_context(|| format!("unknown repoid: {repo_id:?}"))?;
        let common_config = self.repo_configs().common.clone();
        let source = self.configs.repo_config_source(&repo_name);
        let repo = self
            .repo_factory
            .build(repo_name, repo_config, common_config, source)
            .await?;
        Ok(repo)
    }

    /// Open a repository based on user-provided arguments while modifying the repo_factory as
    /// needed
    pub async fn open_repo_with_factory_customization<Repo>(
        &self,
        repo_args: &impl AsRepoArg,
        customize_repo_factory: impl Fn(&mut RepoFactory) -> &mut RepoFactory,
    ) -> Result<Repo>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>,
    {
        let repo_arg = repo_args.as_repo_arg();
        let (repo_name, repo_config) = self.repo_config(repo_arg)?;
        let common_config = self.repo_configs().common.clone();
        let source = self.configs.repo_config_source(&repo_name);
        let mut repo_factory = self.repo_factory.clone();
        let repo_factory = Arc::make_mut(&mut repo_factory);
        customize_repo_factory(repo_factory);
        let repo = repo_factory
            .build(repo_name, repo_config, common_config, source)
            .await?;
        Ok(repo)
    }

    /// Open a repository based on user-provided arguments while modifying the repo_factory as
    /// needed. Make sure that the opened repo has redaction DISABLED
    pub async fn open_repo_unredacted_with_factory_customization<Repo>(
        &self,
        repo_args: &impl AsRepoArg,
        customize_repo_factory: impl Fn(&mut RepoFactory) -> &mut RepoFactory,
    ) -> Result<Repo>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>,
    {
        let repo_arg = repo_args.as_repo_arg();
        let (repo_name, mut repo_config) = self.repo_config(repo_arg)?;
        let common_config = self.repo_configs().common.clone();
        let source = self.configs.repo_config_source(&repo_name);
        repo_config.redaction = Redaction::Disabled;
        let mut repo_factory = self.repo_factory.clone();
        let repo_factory = Arc::make_mut(&mut repo_factory);
        customize_repo_factory(repo_factory);
        let repo = repo_factory
            .build(repo_name, repo_config, common_config, source)
            .await?;
        Ok(repo)
    }

    /// Open an existing repo object
    /// Make sure that the opened repo has redaction DISABLED
    pub async fn open_repo_unredacted<Repo>(&self, repo_args: &impl AsRepoArg) -> Result<Repo>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>,
    {
        let repo_arg = repo_args.as_repo_arg();
        let (repo_name, mut repo_config) = self.repo_config(repo_arg)?;
        let common_config = self.repo_configs().common.clone();
        let source = self.configs.repo_config_source(&repo_name);
        repo_config.redaction = Redaction::Disabled;
        let repo = self
            .repo_factory
            .build(repo_name, repo_config, common_config, source)
            .await?;
        Ok(repo)
    }

    /// Create a new repo object -- for local instances, expect its contents to be empty.
    pub async fn create_repo<Repo>(
        &self,
        repo_arg: &impl AsRepoArg,
        maybe_inner_blobstore_id: Option<u64>,
    ) -> Result<Repo>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>,
    {
        let (repo_name, mut repo_config) = self.repo_config(repo_arg.as_repo_arg())?;
        let common_config = self.repo_configs().common.clone();
        let source = self.configs.repo_config_source(&repo_name);
        if let Some(id) = maybe_inner_blobstore_id {
            self.override_blobconfig(&mut repo_config.storage_config.blobstore, id)?;
        }
        info!(
            "using repo \"{}\" repoid {:?}",
            repo_name, repo_config.repoid
        );

        match &repo_config.storage_config.blobstore {
            BlobConfig::Files { path } | BlobConfig::Sqlite { path } => {
                setup_repo_dir(path, CreateStorage::ExistingOrCreate)?;
            }
            _ => {}
        }
        let repo = self
            .repo_factory
            .build(repo_name, repo_config, common_config, source)
            .await?;
        Ok(repo)
    }

    /// Create a new repo object -- for local instances, expect its contents to be empty.
    /// Makes sure that the opened repo has redaction DISABLED
    pub async fn create_repo_unredacted<Repo>(
        &self,
        repo_arg: &impl AsRepoArg,
        maybe_inner_blobstore_id: Option<u64>,
    ) -> Result<Repo>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>,
    {
        let (repo_name, mut repo_config) = self.repo_config(repo_arg.as_repo_arg())?;
        let common_config = self.repo_configs().common.clone();
        let source = self.configs.repo_config_source(&repo_name);
        if let Some(id) = maybe_inner_blobstore_id {
            self.override_blobconfig(&mut repo_config.storage_config.blobstore, id)?;
        }
        info!(
            "using repo \"{}\" repoid {:?}",
            repo_name, repo_config.repoid
        );

        match &repo_config.storage_config.blobstore {
            BlobConfig::Files { path } | BlobConfig::Sqlite { path } => {
                setup_repo_dir(path, CreateStorage::ExistingOrCreate)?;
            }
            _ => {}
        }
        repo_config.redaction = Redaction::Disabled;
        let repo = self
            .repo_factory
            .build(repo_name, repo_config, common_config, source)
            .await?;
        Ok(repo)
    }

    /// Open a source and target repos based on user-provided arguments.
    pub async fn open_source_and_target_repos<Repo>(
        &self,
        repo_args: &SourceAndTargetRepoArgs,
    ) -> Result<(Repo, Repo)>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>,
    {
        let (source_repo_name, source_repo_config) =
            self.repo_config(repo_args.source_repo.as_repo_arg())?;
        let (target_repo_name, target_repo_config) =
            self.repo_config(repo_args.target_repo.as_repo_arg())?;
        let common_config = self.repo_configs().common.clone();
        let source_source = self.configs.repo_config_source(&source_repo_name);
        let target_source = self.configs.repo_config_source(&target_repo_name);
        let source_repo_fut = self.repo_factory.build(
            source_repo_name,
            source_repo_config,
            common_config.clone(),
            source_source,
        );
        let target_repo_fut = self.repo_factory.build(
            target_repo_name,
            target_repo_config,
            common_config,
            target_source,
        );

        let (source_repo, target_repo) = try_join!(source_repo_fut, target_repo_fut)?;
        Ok((source_repo, target_repo))
    }

    /// Create a manager for all configured repos based on deep-sharding status, excluding
    /// those filtered by `repo_filter_from` in `MononokeEnvironment`.
    pub async fn open_managed_repos<Repo>(
        &self,
        service: Option<ShardedService>,
    ) -> Result<MononokeReposManager<Repo>>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
            + Send
            + Sync
            + 'static,
    {
        let redaction_disabled = self.env.redaction_disabled;
        self.open_managed_repos_with_redaction_disabled(service, redaction_disabled)
            .await
    }

    /// Create a manager for all configured repos based on deep-sharding status, excluding
    /// those filtered by `repo_filter_from` in `MononokeEnvironment`.
    /// When reloading the config, make sure that the opened repo has redaction DISABLED
    pub async fn open_managed_repos_unredacted<Repo>(
        &self,
        service: Option<ShardedService>,
    ) -> Result<MononokeReposManager<Repo>>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
            + Send
            + Sync
            + 'static,
    {
        let redaction_disabled = true;
        self.open_managed_repos_with_redaction_disabled(service, redaction_disabled)
            .await
    }

    /// Create a manager for all configured repos based on deep-sharding status, excluding
    /// those filtered by `repo_filter_from` in `MononokeEnvironment`.
    async fn open_managed_repos_with_redaction_disabled<Repo>(
        &self,
        service: Option<ShardedService>,
        redaction_disabled: bool,
    ) -> Result<MononokeReposManager<Repo>>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
            + Send
            + Sync
            + 'static,
    {
        let repo_filter = self.environment().filter_repos.clone();
        let service_name = service.clone();

        // Load configs from both legacy blob and manifest, keeping the per-repo
        // parse failures so we can optionally fail closed instead of silently
        // serving fewer/0 repos.
        let outcome = self.configs.load_all_repo_configs_checked()?;
        self.enforce_startup_parse_failures(&outcome.failed, service.as_ref())?;
        let configs = outcome.loaded;

        let repo_names = configs.into_iter().filter_map(|(name, config)| {
            let is_matching_filter = repo_filter.as_ref().is_none_or(|filter| filter(&name));
            let is_deep_sharded = service
                .as_ref()
                .and_then(|service| {
                    config
                        .deep_sharding_config
                        .and_then(|c| c.status.get(service).copied())
                })
                .unwrap_or(false);
            // Initialize repos that are enabled and not deep-sharded (i.e. need to exist
            // at service startup)
            if config.enabled && !is_deep_sharded && is_matching_filter {
                Some(name)
            } else {
                None
            }
        });
        let mgr = self
            .open_named_managed_repos_with_redaction_disabled(
                repo_names,
                service_name,
                redaction_disabled,
            )
            .await?;
        Ok(mgr.with_repo_filter(repo_filter))
    }

    /// Enforce or log per-repo config parse failures seen at startup. When the
    /// per-tier `fail_closed_on_repo_config_parse_error` killswitch is on, bails
    /// listing every repo this task serves at boot (matching the repo filter) that
    /// failed to parse. Whole-tier (`service == None`) callers serve every enabled
    /// repo, so none are excluded; for a specific service, the manifest
    /// `is_deep_sharded` flag is the conservative parse-independent proxy for
    /// on-demand (fail-open) repos. Otherwise preserves the historical
    /// one-warn-per-dropped-repo behavior.
    fn enforce_startup_parse_failures(
        &self,
        failed: &[(String, anyhow::Error)],
        service: Option<&ShardedService>,
    ) -> Result<()> {
        let fail_closed = self.configs.tier_name().is_some_and(|tier| {
            justknobs::eval(
                "scm/mononoke:fail_closed_on_repo_config_parse_error",
                None,
                Some(tier),
            )
        });
        if !fail_closed {
            for (name, e) in failed {
                warn!("batch_load: failed to load config for {name}: {e:#}");
            }
            return Ok(());
        }
        // Whole-tier (service == None) callers serve all enabled repos, so exclude
        // nothing; for a specific service, manifest is_deep_sharded is the
        // conservative parse-independent proxy (deep repos load on demand, fail-open).
        let serves_all = service.is_none();
        let manifest = self.configs.manifest();
        let non_deep: HashSet<&str> = manifest
            .as_ref()
            .map(|m| {
                m.repos
                    .iter()
                    .filter(|e| !e.is_deep_sharded)
                    .map(|e| e.repo_name.as_str())
                    .collect()
            })
            .unwrap_or_default();
        let repo_filter = self.environment().filter_repos.clone();
        let blocking = startup_blocking_failures(
            failed,
            |n| serves_all || non_deep.contains(n),
            |n| repo_filter.as_ref().is_none_or(|f| f(n)),
        );
        startup_parse_failure_decision(&blocking)
    }

    /// Create a manager for a set of named managed repos.  These repos must
    /// be configured in the config.
    pub async fn open_named_managed_repos<Repo, Names>(
        &self,
        repo_names: Names,
        service: Option<ShardedService>,
    ) -> Result<MononokeReposManager<Repo>>
    where
        Names: IntoIterator<Item = String>,
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
            + Send
            + Sync
            + 'static,
    {
        let redaction_disabled = self.env.redaction_disabled;
        self.open_named_managed_repos_with_redaction_disabled(
            repo_names,
            service,
            redaction_disabled,
        )
        .await
    }

    /// Create a manager for a set of named managed repos.  These repos must
    /// be configured in the config.
    /// When reloading the config, make sure that the opened repo has redaction DISABLED
    pub async fn open_named_managed_repos_unredacted<Repo, Names>(
        &self,
        repo_names: Names,
        service: Option<ShardedService>,
    ) -> Result<MononokeReposManager<Repo>>
    where
        Names: IntoIterator<Item = String>,
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
            + Send
            + Sync
            + 'static,
    {
        let redaction_disabled = true;
        self.open_named_managed_repos_with_redaction_disabled(
            repo_names,
            service,
            redaction_disabled,
        )
        .await
    }

    async fn open_named_managed_repos_with_redaction_disabled<Repo, Names>(
        &self,
        repo_names: Names,
        service: Option<ShardedService>,
        redaction_disabled: bool,
    ) -> Result<MononokeReposManager<Repo>>
    where
        Names: IntoIterator<Item = String>,
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
            + Send
            + Sync
            + 'static,
    {
        let start = Instant::now();
        let repos_mgr = MononokeReposManager::new_with_redaction_disabled(
            self.configs.clone(),
            self.repo_factory().clone(),
            service,
            repo_names,
            redaction_disabled,
        )
        .await?;
        info!(
            "All repos initialized. It took: {} seconds",
            start.elapsed().as_secs()
        );
        STATS::completion_duration_secs
            .add_value(start.elapsed().as_secs().try_into().unwrap_or(i64::MAX));

        #[cfg(fbcode_build)]
        {
            let instrument = Instrument_MononokeAppStats::new();
            instrument.observe(MononokeAppStats {
                completion_duration_secs: Some(start.elapsed().as_secs() as f64),
                ..Default::default()
            });
        }
        Ok(repos_mgr)
    }

    /// Create a manager for a single repo, specified by repo arguments.
    pub async fn open_managed_repo_arg<Repo>(
        &self,
        repo_arg: &impl AsRepoArg,
    ) -> Result<MononokeReposManager<Repo>>
    where
        Repo: for<'builder> AsyncBuildable<'builder, RepoFactoryBuilder<'builder>>
            + Send
            + Sync
            + 'static,
    {
        let (repo_name, _) = self.repo_config(repo_arg.as_repo_arg())?;
        self.open_named_managed_repos(Some(repo_name), None).await
    }

    /// Open a raw blobstore based only on storage name without repo prefix.
    pub async fn open_raw_blobstore(
        &self,
        storage_name: &str,
        inner_blobstore_id: Option<u64>,
        use_mutable: bool,
    ) -> Result<Arc<dyn Blobstore>> {
        let storage_configs = self.storage_configs();
        let storage_config = storage_configs
            .storage
            .get(storage_name)
            .ok_or_else(|| anyhow!("unknown storage name: {storage_name:?}"))?
            .clone();

        let mut blob_config = if use_mutable {
            storage_config.mutable_blobstore
        } else {
            storage_config.blobstore
        };

        if let Some(id) = inner_blobstore_id {
            self.override_blobconfig(&mut blob_config, id)?;
        };

        let blobstore = blobstore_factory::make_blobstore(
            self.env.fb,
            blob_config,
            &self.env.mysql_options,
            self.env.readonly_storage,
            &self.env.blobstore_options,
            &self.env.config_store,
            &blobstore_factory::default_scrub_handler(),
            None,
            None,
        )
        .await?;

        Ok(blobstore)
    }

    /// Open just the blobstore based on user-provided arguments.
    pub async fn open_blobstore(
        &self,
        repo_blobstore_args: &RepoBlobstoreArgs,
    ) -> Result<Arc<dyn KeyedBlobstore>> {
        // Route through MononokeConfigs::get_or_load_repo_config* so split-loaded
        // repos (only present in the per-tier RepoSpec manifest) resolve correctly.
        let (repo_id, mut redaction, storage_config) =
            if let Some(repo_id) = repo_blobstore_args.repo_id {
                let repo_id = RepositoryId::new(repo_id);
                let (_repo_name, repo_config) = self
                    .configs
                    .get_or_load_repo_config_by_id(repo_id.id())
                    .with_context(|| format!("unknown repoid: {repo_id:?}"))?;
                (repo_id, repo_config.redaction, repo_config.storage_config)
            } else if let Some(repo_name) = &repo_blobstore_args.repo_name {
                let repo_config = self
                    .configs
                    .get_or_load_repo_config(repo_name)
                    .with_context(|| format!("unknown reponame: {repo_name:?}"))?;
                (
                    repo_config.repoid,
                    repo_config.redaction,
                    repo_config.storage_config,
                )
            } else {
                return Err(anyhow!("Expected either repo_id or repo_name"));
            };

        let mut blob_config = if repo_blobstore_args.use_mutable {
            storage_config.mutable_blobstore
        } else {
            storage_config.blobstore
        };

        if let Some(id) = repo_blobstore_args.inner_blobstore_id {
            self.override_blobconfig(&mut blob_config, id)?;
        };

        let blobstore = blobstore_factory::make_blobstore(
            self.env.fb,
            blob_config,
            &self.env.mysql_options,
            self.env.readonly_storage,
            &self.env.blobstore_options,
            &self.env.config_store,
            &blobstore_factory::default_scrub_handler(),
            None,
            None,
        )
        .await?;

        if repo_blobstore_args.bypass_redaction {
            redaction = Redaction::Disabled;
        }

        let redacted_blobs = if redaction == Redaction::Enabled {
            let redacted_blobs = self
                .repo_factory
                .redacted_blobs(
                    self.new_basic_context(),
                    &storage_config.metadata,
                    &Arc::new(self.repo_configs().common.clone()),
                )
                .await?;
            Some(redacted_blobs)
        } else {
            None
        };

        let redacted_blobstore_config =
            RedactedBlobstoreConfig::new(redacted_blobs, self.redaction_scuba_builder()?);

        Ok(Arc::new(RepoBlobstore::build(
            blobstore,
            repo_id.prefix(),
            redacted_blobstore_config,
        )))
    }

    pub async fn open_blobstore_with_overridden_blob_config(
        &self,
        config: &BlobConfig,
    ) -> Result<Arc<dyn Blobstore>> {
        self.repo_factory.blobstore(config).await
    }

    pub async fn redaction_config_blobstore(&self) -> Result<Arc<RedactionConfigBlobstore>> {
        self.repo_factory
            .redaction_config_blobstore_from_config(
                &self.repo_configs().common.redaction_config.blobstore,
            )
            .await
    }

    fn redaction_scuba_builder(&self) -> Result<MononokeScubaSampleBuilder> {
        let params = &self.repo_configs().common.censored_scuba_params;
        let mut builder =
            MononokeScubaSampleBuilder::with_opt_table(self.env.fb, params.table.clone())?;
        if let Some(file) = &params.local_path {
            builder = builder
                .with_log_file(file)
                .context("Failed to open scuba log file")?;
        }

        Ok(builder)
    }

    fn override_blobconfig(&self, blob_config: &mut BlobConfig, id: u64) -> Result<()> {
        match blob_config {
            &mut BlobConfig::MultiplexedWal { ref blobstores, .. } => {
                let sought_id = BlobstoreId::new(id);
                let inner_blob_config = blobstores
                    .iter()
                    .find_map(|(blobstore_id, _, blobstore)| {
                        if blobstore_id == &sought_id {
                            Some(blobstore.clone())
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| anyhow!("could not find a blobstore with id {id}"))?;
                *blob_config = inner_blob_config;
            }
            _ => {
                return Err(anyhow!(
                    "inner-blobstore-id supplied but blobstore is not multiplexed"
                ));
            }
        }
        Ok(())
    }
}

/// Parse failures that must block startup: repos this task serves at boot
/// (non-deep-sharded per the manifest + matching the repo filter). Deep /
/// filtered-out repos load on demand (fail-open), so exclude them.
fn startup_blocking_failures<'a>(
    failed: &'a [(String, anyhow::Error)],
    is_non_deep_manifest_repo: impl Fn(&str) -> bool,
    matches_filter: impl Fn(&str) -> bool,
) -> Vec<&'a (String, anyhow::Error)> {
    failed
        .iter()
        .filter(|(name, _)| is_non_deep_manifest_repo(name) && matches_filter(name))
        .collect()
}

/// Pure fail-closed decision for startup repo-config parse failures.
///
/// Given the already-scoped `blocking` failures (see `startup_blocking_failures`),
/// returns an error listing every blocking repo (name + error) so startup bails
/// instead of silently serving fewer/0 repos. Empty -> `Ok(())`.
fn startup_parse_failure_decision(blocking: &[&(String, anyhow::Error)]) -> Result<()> {
    if blocking.is_empty() {
        return Ok(());
    }

    let details = blocking
        .iter()
        .map(|(name, e)| format!("{name}: {e:#}"))
        .collect::<Vec<_>>()
        .join("; ");
    Err(anyhow!(
        "fail-closed startup: {} repo config(s) failed to parse: {details}",
        blocking.len(),
    ))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CreateStorage {
    ExistingOnly,
    ExistingOrCreate,
}

pub fn setup_repo_dir<P: AsRef<Path>>(data_dir: P, create: CreateStorage) -> Result<()> {
    let data_dir = data_dir.as_ref();

    if !data_dir.is_dir() {
        bail!("{data_dir:?} does not exist or is not a directory");
    }

    // Validate directory layout
    #[allow(clippy::single_element_loop)]
    for subdir in &["blobs"] {
        let subdir = data_dir.join(subdir);

        if subdir.exists() && !subdir.is_dir() {
            bail!("{subdir:?} already exists and is not a directory");
        }

        if !subdir.exists() {
            if CreateStorage::ExistingOnly == create {
                bail!("{subdir:?} not found in ExistingOnly mode",);
            }
            fs::create_dir(&subdir)
                .with_context(|| format!("failed to create subdirectory {subdir:?}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
