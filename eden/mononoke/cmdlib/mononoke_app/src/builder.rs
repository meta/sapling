/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::any::TypeId;
use std::collections::HashMap;
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use arg_extensions::ArgDefaults;
use blobstore_factory::BlobstoreArgs;
use blobstore_factory::BlobstoreOptions;
use blobstore_factory::CachelibBlobstoreOptions;
use blobstore_factory::ChaosOptions;
use blobstore_factory::DelayOptions;
#[cfg(fbcode_build)]
use blobstore_factory::ManifoldArgs;
use blobstore_factory::PackOptions;
use blobstore_factory::ReadOnlyStorage;
use blobstore_factory::ReadOnlyStorageArgs;
use blobstore_factory::ThrottleOptions;
use cached_config::ConfigStore;
use clap::ArgMatches;
use clap::Args;
use clap::Command;
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::error::ErrorKind;
use clientinfo::ClientEntryPoint;
use cmdlib_caching::CachelibArgs;
use cmdlib_caching::CachelibSettings;
use cmdlib_caching::init_cachelib;
use cmdlib_logging::LoggingArgs;
use cmdlib_logging::ScubaLoggingArgs;
use commit_graph_types::environment::CommitGraphArgs;
use derived_data_remote::RemoteDerivationArgs;
use environment::BookmarkCacheOptions;
use environment::MononokeEnvironment;
use fbinit::FacebookInit;
use megarepo_config::MegarepoConfigsArgs;
use megarepo_config::MononokeMegarepoConfigsOptions;
use permission_checker::AclProvider;
#[cfg(not(fbcode_build))]
use permission_checker::DefaultAclProvider;
use permission_checker::InternalAclProvider;
use rendezvous::RendezVousArgs;
use repo_derivation_queues::DerivationQueueArgs;
use repos::MononokeService;
use sql_ext::facebook::MysqlOptions;
use sql_ext::facebook::PoolConfig;
use sql_ext::facebook::ReadConnectionType;
use sql_ext::facebook::SharedConnectionPool;
use tokio::runtime::Handle;
use tokio::runtime::Runtime;

use crate::app::MononokeApp;
use crate::args::AclArgs;
use crate::args::ConfigArgs;
use crate::args::GFlagsArgs;
use crate::args::JustKnobsArgs;
use crate::args::MysqlArgs;
use crate::args::RemoteDiffArgs;
use crate::args::RuntimeArgs;
use crate::args::parse_config_spec_to_path;
use crate::extension::AppExtension;
use crate::extension::AppExtensionBox;
use crate::extension::BoxedAppExtension;

pub struct MononokeAppBuilder {
    fb: FacebookInit,
    extensions: Vec<(TypeId, Box<dyn BoxedAppExtension>)>,
    arg_defaults: Vec<Box<dyn ArgDefaults>>,
    cachelib_settings: CachelibSettings,
    default_scuba_dataset: Option<String>,
    defaults: HashMap<&'static str, String>,
    bookmark_cache_options: BookmarkCacheOptions,
    client_entry_point_for_service: ClientEntryPoint,
    override_cmd_args: Option<Vec<String>>,
    paged_help: bool,
    with_logging: bool,
    lazy_acl_provider: bool,
    default_service: Option<MononokeService>,
}

#[derive(Args, Debug)]
pub struct EnvironmentArgs {
    #[clap(flatten, next_help_heading = "CONFIG OPTIONS")]
    config_args: ConfigArgs,

    #[clap(flatten, next_help_heading = "LOGGING OPTIONS")]
    logging_args: LoggingArgs,

    #[clap(flatten, next_help_heading = "SCUBA LOGGING OPTIONS")]
    scuba_logging_args: ScubaLoggingArgs,

    #[clap(flatten, next_help_heading = "CACHELIB OPTIONS")]
    cachelib_args: CachelibArgs,

    #[clap(flatten, next_help_heading = "MYSQL OPTIONS")]
    mysql_args: MysqlArgs,

    #[clap(flatten, next_help_heading = "JUST KNOBS OPTIONS")]
    just_knobs_args: JustKnobsArgs,

    #[clap(flatten, next_help_heading = "BLOBSTORE OPTIONS")]
    blobstore_args: BlobstoreArgs,

    #[cfg(fbcode_build)]
    #[clap(flatten, next_help_heading = "MANIFOLD OPTIONS")]
    manifold_args: ManifoldArgs,

    #[clap(flatten, next_help_heading = "ACL OPTIONS")]
    acl_args: AclArgs,

    #[clap(flatten, next_help_heading = "REMOTE DERIVATION OPTIONS")]
    remote_derivation_args: RemoteDerivationArgs,

    #[clap(flatten, next_help_heading = "REMOTE DIFF OPTIONS")]
    remote_diff_args: RemoteDiffArgs,

    #[clap(flatten, next_help_heading = "STORAGE OPTIONS")]
    readonly_storage_args: ReadOnlyStorageArgs,

    #[clap(flatten, next_help_heading = "RENDEZ-VOUS OPTIONS")]
    rendezvous_args: RendezVousArgs,

    #[clap(flatten, next_help_heading = "MEGAREPO OPTIONS")]
    megarepo_configs_args: MegarepoConfigsArgs,

    #[clap(flatten, next_help_heading = "GFLAGS")]
    gflags_args: GFlagsArgs,

    #[clap(flatten, next_help_heading = "COMMIT GRAPH OPTIONS")]
    commit_graph_args: CommitGraphArgs,

    #[clap(flatten, next_help_heading = "DERIVATION QUEUE OPTIONS")]
    derivation_queue_args: DerivationQueueArgs,

    /// Disable content redaction. Intended for Cogwheel tests where the
    /// redaction keylist blobs are not available in the ephemeral blobstore.
    #[clap(long)]
    disable_redaction: bool,
}

impl MononokeAppBuilder {
    pub fn new(fb: FacebookInit) -> Self {
        MononokeAppBuilder {
            fb,
            extensions: Vec::new(),
            arg_defaults: Vec::new(),
            cachelib_settings: CachelibSettings::default(),
            default_scuba_dataset: None,
            defaults: HashMap::new(),
            bookmark_cache_options: Default::default(),
            client_entry_point_for_service: Default::default(),
            override_cmd_args: None,
            paged_help: false,
            with_logging: true,
            lazy_acl_provider: false,
            default_service: None,
        }
    }

    /// The service this program runs, whose view of a deployment it loads
    /// unless `--service` names another. A program that serves no single
    /// service leaves it unset and loads `COMMAND`'s.
    pub fn with_default_service(mut self, service: MononokeService) -> Self {
        self.default_service = Some(service);
        self
    }

    pub fn with_arg_defaults(mut self, arg_defaults: impl ArgDefaults + 'static) -> Self {
        self.arg_defaults.push(Box::new(arg_defaults));
        self
    }

    pub fn with_default_scuba_dataset(mut self, default: impl Into<String>) -> Self {
        self.default_scuba_dataset = Some(default.into());
        self
    }

    pub fn with_bookmarks_cache(mut self, bookmark_cache_options: BookmarkCacheOptions) -> Self {
        self.bookmark_cache_options = bookmark_cache_options;
        self
    }

    /// Give the app the option to disable setting up tracing completely.
    /// This is useful for cases where the app wants to setup its own tracing stack. You can only have
    /// one global tracing subscriber per process so if Mononoke sets one app the applications can't do
    /// later.
    pub fn with_logging(mut self, with_logging: bool) -> Self {
        self.with_logging = with_logging;
        self
    }

    /// Method for setting the service level client entry point that is not specific to any
    /// particular request
    pub fn with_entry_point(mut self, entry_point: ClientEntryPoint) -> Self {
        self.client_entry_point_for_service = entry_point;
        self
    }

    pub fn with_cachelib_settings(mut self, cachelib_settings: CachelibSettings) -> Self {
        self.cachelib_settings = cachelib_settings;
        self
    }

    pub fn with_app_extension<Ext>(mut self, ext: Ext) -> Self
    where
        Ext: AppExtension + 'static,
    {
        self.extensions
            .push((TypeId::of::<Ext>(), AppExtensionBox::new(ext)));
        self
    }

    /// Allows overriding the command line arguments and build the app with ad-hoc arguments.
    pub fn with_cmd_args(mut self, cmd_args: Vec<String>) -> Self {
        self.override_cmd_args = Some(cmd_args);
        self
    }

    /// Page help output (`--help`, `help`, and missing-subcommand help)
    /// through `$PAGER` when it is set.
    ///
    /// Paging only happens for interactive use (stdout is a terminal); when
    /// the pager cannot be started, help is printed directly as usual.
    pub fn with_paged_help(mut self, paged_help: bool) -> Self {
        self.paged_help = paged_help;
        self
    }

    /// Defer the `AccessCheckerProvider` REPO/REPO_REGION preload until the
    /// first ACL check instead of blocking startup on it. Intended for CLIs
    /// like `mononoke_admin`, where most invocations never check permissions.
    /// Services keep the eager default so they fail fast when the ACL backend
    /// is unreachable.
    pub fn with_lazy_acl_provider(mut self, lazy_acl_provider: bool) -> Self {
        self.lazy_acl_provider = lazy_acl_provider;
        self
    }

    pub fn build<AppArgs>(&mut self) -> Result<MononokeApp>
    where
        AppArgs: CommandFactory,
    {
        self.build_with_subcommands::<AppArgs>(Vec::new())
    }

    pub fn build_with_subcommands<'a, AppArgs>(
        &'a mut self,
        subcommands: Vec<Command>,
    ) -> Result<MononokeApp>
    where
        AppArgs: CommandFactory,
    {
        for (arg, default) in self.cachelib_settings.arg_defaults() {
            self.defaults.insert(arg, default);
        }

        for defaults in self.arg_defaults.iter() {
            for (arg, default) in defaults.arg_defaults() {
                self.defaults.insert(arg, default);
            }
        }

        for (_type_id, ext) in self.extensions.iter() {
            for (arg, default) in ext.arg_defaults() {
                self.defaults.insert(arg, default);
            }
        }

        let mut app = AppArgs::command();

        {
            // Save app-generated about so we can restore it.
            let about = app.get_about().cloned();
            let long_about = app.get_long_about().cloned();

            app = RuntimeArgs::augment_args_for_update(app);
            app = EnvironmentArgs::augment_args_for_update(app);
            for (_type_id, ext) in self.extensions.iter() {
                app = ext.augment_args(app);
            }

            // Adding the additional args overrode the about messages.
            // Restore them.
            if let Some(about) = about {
                app = app.about(about);
            }
            if let Some(long_about) = long_about {
                app = app.long_about(long_about);
            }
        }

        if !subcommands.is_empty() {
            app = app
                .subcommands(subcommands)
                .subcommand_required(true)
                .arg_required_else_help(true);
        }

        for (name, default) in self.defaults.iter() {
            app = app.mut_arg(name, |arg| arg.default_value(default));
        }

        let args = if let Some(ref override_cmd_args) = self.override_cmd_args {
            app.get_matches_from(override_cmd_args)
        } else if self.paged_help {
            get_matches_with_paged_help(app)
        } else {
            app.get_matches()
        };

        let extension_args = self
            .extensions
            .iter()
            .map(|(type_id, ext)| Ok((*type_id, ext.parse_args(&args)?)))
            .collect::<Result<Vec<_>>>()?;

        let runtime_args = RuntimeArgs::from_arg_matches(&args)?;
        let runtime = create_runtime(&runtime_args)?;

        let env_args = EnvironmentArgs::from_arg_matches(&args)?;
        let config_mode = env_args.config_args.mode();
        let mut env = self.build_environment(&runtime, env_args)?;

        for (_type_id, ext) in extension_args.iter() {
            ext.environment_hook(&mut env)?;
        }

        MononokeApp::new(
            self.fb,
            config_mode,
            self.default_service,
            args,
            runtime,
            env,
            extension_args.into_iter().collect(),
        )
    }

    fn build_environment(
        &self,
        runtime: &Runtime,
        env_args: EnvironmentArgs,
    ) -> Result<MononokeEnvironment> {
        let EnvironmentArgs {
            blobstore_args,
            config_args,
            logging_args,
            scuba_logging_args,
            cachelib_args,
            #[cfg(fbcode_build)]
            manifold_args,
            megarepo_configs_args,
            mysql_args,
            readonly_storage_args,
            acl_args,
            remote_derivation_args,
            remote_diff_args,
            rendezvous_args,
            just_knobs_args,
            gflags_args,
            commit_graph_args,
            derivation_queue_args,
            disable_redaction,
        } = env_args;

        gflags_args.propagate(self.fb)?;

        if self.with_logging {
            logging_args.setup_logging(self.fb)?;
        }

        let config_store = config_args
            .create_config_store(self.fb)
            .context("Failed to create config store")?;

        let observability_context = logging_args
            .create_observability_context(&config_store)
            .context("Failed to initialize observability context")?;

        let scuba_sample_builder = scuba_logging_args
            .create_scuba_sample_builder(
                self.fb,
                &observability_context,
                &self.default_scuba_dataset,
            )
            .context("Failed to create scuba sample builder")?;
        let warm_bookmarks_cache_scuba_sample_builder =
            if scuba_logging_args.warm_bookmark_cache_default_scuba_dataset {
                scuba_sample_builder.clone()
            } else {
                scuba_logging_args
                    .create_warm_bookmark_cache_scuba_sample_builder(self.fb)
                    .context("Failed to create warm bookmark cache scuba sample builder")?
            };

        let caching = init_cachelib(self.fb, &self.cachelib_settings, &cachelib_args);

        let mysql_options =
            create_mysql_options(&mysql_args, create_mysql_pool_config(&mysql_args));

        let blobstore_options = create_blobstore_options(
            &blobstore_args,
            &mysql_args,
            #[cfg(fbcode_build)]
            manifold_args,
        )
        .context("Failed to parse blobstore options")?;

        let readonly_storage = ReadOnlyStorage::from_args(&readonly_storage_args);

        let rendezvous_options = rendezvous_args.into();

        let megarepo_configs_options = MononokeMegarepoConfigsOptions::from_args(
            config_args.local_configerator_path.as_deref(),
            &megarepo_configs_args,
        );

        let remote_derivation_options = remote_derivation_args.into();

        let remote_diff_options = remote_diff_args.into();

        let acl_provider = create_acl_provider(self.fb, &acl_args, runtime, self.lazy_acl_provider)
            .context("Failed to create ACL provider")?;

        let commit_graph_options = commit_graph_args.into();

        init_just_knobs_worker(
            &just_knobs_args,
            &config_store,
            (),
            runtime.handle().clone(),
        )?;

        Ok(MononokeEnvironment {
            fb: self.fb,
            scuba_sample_builder,
            warm_bookmarks_cache_scuba_sample_builder,
            config_store,
            caching,
            observability_context,
            runtime: runtime.handle().clone(),
            mysql_options,
            blobstore_options,
            readonly_storage,
            acl_provider,
            rendezvous_options,
            megarepo_configs_options,
            remote_derivation_options,
            remote_diff_options,
            disabled_hooks: HashMap::new(),
            bookmark_cache_options: self.bookmark_cache_options.clone(),
            filter_repos: None,
            commit_graph_options,
            client_entry_point_for_service: self.client_entry_point_for_service,
            derivation_queue_namespace: derivation_queue_args.derivation_queue_namespace,
            use_pipeline_zelos_config: derivation_queue_args.use_pipeline_zelos_config,
            redaction_disabled: disable_redaction,
        })
    }
}

fn get_matches_with_paged_help(app: Command) -> ArgMatches {
    match app.try_get_matches() {
        Ok(matches) => matches,
        Err(err) => {
            // Preserve clap's exit status: `--help`/`help` succeed, while a
            // bare invocation prints help as an error.
            let exit_code = match err.kind() {
                ErrorKind::DisplayHelp => Some(0),
                ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => Some(2),
                _ => None,
            };
            let Some(exit_code) = exit_code else {
                err.exit()
            };
            if crate::pager::page_text(&err.to_string()) {
                exit(exit_code);
            }
            err.exit()
        }
    }
}

fn create_runtime(runtime_args: &RuntimeArgs) -> Result<Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    builder.thread_name("tk");
    if let Some(threads) = runtime_args.runtime_threads {
        builder.worker_threads(threads);
    }
    if let Some(thread_stack_size) = runtime_args.runtime_thread_stack_size {
        builder.thread_stack_size(thread_stack_size);
    }
    // Propagate the ambient folly RequestContext across tokio::spawn boundaries
    // (Artillery trace continuity). In OSS this is a no-op.
    request_context_ext::install_request_context_hooks(&mut builder);
    let runtime = builder.build()?;
    Ok(runtime)
}

fn create_mysql_options(mysql_args: &MysqlArgs, pool_config: PoolConfig) -> MysqlOptions {
    let pool = SharedConnectionPool::new();
    let read_connection_type = if mysql_args.mysql_master_only {
        ReadConnectionType::Master
    } else {
        ReadConnectionType::ReplicaOnly
    };
    MysqlOptions {
        pool,
        pool_config,
        read_connection_type,
    }
}

fn create_mysql_pool_config(mysql_args: &MysqlArgs) -> PoolConfig {
    PoolConfig::new(
        mysql_args.mysql_pool_limit,
        mysql_args.mysql_pool_threads_num,
        mysql_args.mysql_pool_per_key_limit,
        mysql_args.mysql_pool_age_timeout,
        mysql_args.mysql_pool_idle_timeout,
        mysql_args.mysql_conn_open_timeout,
        Duration::from_millis(mysql_args.mysql_query_time_limit),
    )
}

fn create_mysql_sqlblob_pool_config(mysql_args: &MysqlArgs) -> PoolConfig {
    PoolConfig::new(
        mysql_args.mysql_sqlblob_pool_limit,
        mysql_args.mysql_sqlblob_pool_threads_num,
        mysql_args.mysql_sqlblob_pool_per_key_limit,
        mysql_args.mysql_sqlblob_pool_age_timeout,
        mysql_args.mysql_sqlblob_pool_idle_timeout,
        mysql_args.mysql_conn_open_timeout,
        Duration::from_millis(mysql_args.mysql_query_time_limit),
    )
}

fn create_blobstore_options(
    blobstore_args: &BlobstoreArgs,
    mysql_args: &MysqlArgs,
    #[cfg(fbcode_build)] manifold_args: ManifoldArgs,
) -> Result<BlobstoreOptions> {
    let chaos_options = ChaosOptions::new(
        blobstore_args.blobstore_read_chaos_rate,
        blobstore_args.blobstore_write_chaos_rate,
    );

    let delay_options = DelayOptions {
        get_dist: blobstore_args.get_delay_distribution()?,
        put_dist: blobstore_args.put_delay_distribution()?,
    };

    let throttle_options = ThrottleOptions {
        read_qps: blobstore_args.blobstore_read_qps,
        write_qps: blobstore_args.blobstore_write_qps,
        read_bytes: blobstore_args.blobstore_read_bytes_s,
        write_bytes: blobstore_args.blobstore_write_bytes_s,
        read_burst_bytes: blobstore_args.blobstore_read_burst_bytes_s,
        write_burst_bytes: blobstore_args.blobstore_write_burst_bytes_s,
        bytes_min_count: blobstore_args.blobstore_bytes_min_throttle,
    };

    let pack_options = PackOptions::new(blobstore_args.put_format_override()?);

    let cachelib_blobstore_options =
        CachelibBlobstoreOptions::new_lazy(Some(blobstore_args.blobstore_cachelib_attempt_zstd));

    let blobstore_put_behaviour = blobstore_args.blobstore_put_behaviour;

    let mysql_sqlblob_options =
        create_mysql_options(mysql_args, create_mysql_sqlblob_pool_config(mysql_args));

    let blobstore_options = BlobstoreOptions::new(
        chaos_options,
        delay_options,
        throttle_options,
        #[cfg(fbcode_build)]
        manifold_args.into(),
        pack_options,
        cachelib_blobstore_options,
        blobstore_put_behaviour,
        mysql_sqlblob_options,
    );

    Ok(blobstore_options)
}

fn init_just_knobs_worker(
    just_knobs_args: &JustKnobsArgs,
    config_store: &ConfigStore,
    logger: impl justknobs::cached_config::IntoLogger,
    handle: Handle,
) -> Result<()> {
    if !just_knobs_args.just_knob_overrides.is_empty() {
        let overrides = just_knobs_args
            .just_knob_overrides
            .iter()
            .map(|arg| justknobs::overrides::parse_override(arg))
            .collect::<Result<_>>()?;
        justknobs::overrides::set_debug_overrides(overrides)?;
        if let Some(overrides) = justknobs::overrides::debug_overrides() {
            tracing::warn!("JustKnobs pinned by --just-knob: {:?}", overrides);
        }
    }
    if let Some(just_knobs_config_path) = &just_knobs_args.just_knobs_config_path {
        let config_handle =
            config_store.get_config_handle(parse_config_spec_to_path(just_knobs_config_path)?)?;
        justknobs::cached_config::init_just_knobs_worker(logger, config_handle, handle)
    } else {
        Ok(())
    }
}

#[cfg(fbcode_build)]
fn create_acl_provider(
    fb: FacebookInit,
    acl_args: &AclArgs,
    runtime: &Runtime,
    lazy_acl_provider: bool,
) -> Result<Arc<dyn AclProvider>> {
    if let Some(acl_file) = &acl_args.acl_file {
        return InternalAclProvider::from_file(acl_file)
            .with_context(|| format!("Failed to load ACLs from '{}'", acl_file.to_string_lossy()));
    }
    let verifier = parse_access_checker_verifier(acl_args)?;
    if lazy_acl_provider {
        return Ok(Arc::new(
            permission_checker::AccessCheckerProvider::new_lazy(fb, verifier),
        ));
    }
    runtime
        .block_on(permission_checker::AccessCheckerProvider::new(fb, verifier))
        .context("Failed to create AccessCheckerProvider")
}

#[cfg(fbcode_build)]
fn parse_access_checker_verifier(acl_args: &AclArgs) -> Result<infrasec_authorization::Identity> {
    let raw = acl_args.access_checker_verifier.as_str();
    let (id_type, id_data) = raw.split_once(':').with_context(|| {
        format!("Invalid --access-checker-verifier value '{raw}': expected '<type>:<data>'")
    })?;
    Ok(infrasec_authorization::Identity {
        id_type: id_type.to_string(),
        id_data: id_data.to_string(),
        ..Default::default()
    })
}

#[cfg(not(fbcode_build))]
fn create_acl_provider(
    fb: FacebookInit,
    acl_args: &AclArgs,
    _runtime: &Runtime,
    _lazy_acl_provider: bool,
) -> Result<Arc<dyn AclProvider>> {
    if let Some(acl_file) = &acl_args.acl_file {
        return InternalAclProvider::from_file(acl_file)
            .with_context(|| format!("Failed to load ACLs from '{}'", acl_file.to_string_lossy()));
    }
    DefaultAclProvider::new(fb).context("Failed to create DefaultAclProvider")
}
