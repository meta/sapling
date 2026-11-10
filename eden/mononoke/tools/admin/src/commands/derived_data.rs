/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod backfill_abort;
mod backfill_enqueue;
mod backfill_reconcile_configs;
mod backfill_status;
mod count_underived;
mod derive;
mod derive_slice;
mod enabled_types;
mod exists;
mod fetch;
mod find_derivation_gaps;
mod list_manifest;
mod slice;
mod verify_aug_direct;
mod verify_aug_upload;
mod verify_manifests;
mod verify_stage_output;

use std::collections::HashMap;

use anyhow::Context;
use anyhow::Result;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use bonsai_p4_mapping::BonsaiP4Mapping;
use bonsai_svnrev_mapping::BonsaiSvnrevMapping;
use bookmarks::Bookmarks;
use clap::ArgGroup;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use commit_graph::CommitGraph;
use filenodes::Filenodes;
use filestore::FilestoreConfig;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArg;
use mononoke_app::args::RepoArgs;
use mononoke_types::DerivableType;
use mononoke_types::RepositoryId;
use repo_blobstore::RepoBlobstore;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataRef;
use repo_factory::RepoFactory;
use repo_identity::RepoIdentity;
use strum::IntoEnumIterator;

use self::backfill_abort::BackfillAbortArgs;
use self::backfill_abort::backfill_abort;
use self::backfill_enqueue::BackfillEnqueueArgs;
use self::backfill_enqueue::backfill_enqueue;
use self::backfill_reconcile_configs::BackfillReconcileConfigsArgs;
use self::backfill_reconcile_configs::backfill_reconcile_configs;
use self::backfill_status::BackfillStatusArgs;
use self::backfill_status::backfill_status;
use self::count_underived::CountUnderivedArgs;
use self::count_underived::count_underived;
use self::derive::DeriveArgs;
use self::derive::derive;
use self::derive_slice::DeriveSliceArgs;
use self::derive_slice::derive_slice;
use self::enabled_types::EnabledTypesArgs;
use self::enabled_types::enabled_types;
use self::exists::ExistsArgs;
use self::exists::exists;
use self::fetch::FetchArgs;
use self::fetch::fetch;
use self::find_derivation_gaps::FindDerivationGapsArgs;
use self::find_derivation_gaps::find_derivation_gaps;
use self::list_manifest::ListManifestArgs;
use self::list_manifest::list_manifest;
use self::slice::SliceArgs;
use self::slice::slice;
use self::verify_aug_direct::VerifyAugDirectArgs;
use self::verify_aug_direct::verify_aug_direct;
use self::verify_aug_upload::VerifyAugUploadArgs;
use self::verify_aug_upload::verify_aug_upload;
use self::verify_manifests::VerifyManifestsArgs;
use self::verify_manifests::verify_manifests;
use self::verify_stage_output::VerifyStageOutputArgs;
use self::verify_stage_output::verify_stage_output;

#[facet::container]
struct Repo {
    #[facet]
    repo_identity: RepoIdentity,
    #[facet]
    repo_derived_data: RepoDerivedData,
    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,
    #[facet]
    bonsai_git_mapping: dyn BonsaiGitMapping,
    #[facet]
    bonsai_globalrev_mapping: dyn BonsaiGlobalrevMapping,
    #[facet]
    bonsai_svnrev_mapping: dyn BonsaiSvnrevMapping,

    #[facet]
    bonsai_p4_mapping: dyn BonsaiP4Mapping,
    #[facet]
    repo_blobstore: RepoBlobstore,
    #[facet]
    bookmarks: dyn Bookmarks,
    #[facet]
    commit_graph: CommitGraph,
    #[facet]
    filenodes: dyn Filenodes,
    #[facet]
    filestore_config: FilestoreConfig,
}

#[derive(Args, Debug)]
#[clap(group(
    ArgGroup::new("repos")
        .multiple(true)
        .args(&["repo_id", "repo_name"]),
))]
struct DerivedDataRepoArgs {
    /// Numeric repository ID
    #[clap(long)]
    repo_id: Vec<i32>,

    /// Repository name
    #[clap(short = 'R', long)]
    repo_name: Vec<String>,
}

impl DerivedDataRepoArgs {
    fn ids_or_names(&self) -> Vec<RepoArg> {
        let mut l = Vec::new();
        for id in &self.repo_id {
            l.push(RepoArg::Id(RepositoryId::new(*id)));
        }
        for name in &self.repo_name {
            l.push(RepoArg::Name(name.clone()));
        }
        l
    }
}

/// Request information about derived data
#[derive(Parser)]
pub struct CommandArgs {
    #[clap(flatten)]
    repo: DerivedDataRepoArgs,

    /// The derived data config name to use. If not specified, the enabled config will be used
    #[clap(short, long)]
    config_name: Option<String>,

    /// Whether to bypass redaction when deriving and querying derived data.
    #[clap(long)]
    bypass_redaction: bool,

    #[clap(subcommand)]
    subcommand: DerivedDataSubcommand,
}

#[derive(Subcommand)]
enum DerivedDataSubcommand {
    /// Abort a derive_backfill async request and all its in-progress children
    BackfillAbort(BackfillAbortArgs),
    /// Enqueue derived data backfill work via async requests
    BackfillEnqueue(BackfillEnqueueArgs),
    /// Reconcile the enabled_derived_data_types table into configerator
    /// (Phase-A bridge). Dry-run by default; --apply lands config.
    BackfillReconcileConfigs(BackfillReconcileConfigsArgs),
    /// Show status of derive backfill jobs.
    /// Pass -R or --repo-id to drill down on a specific repo in a multi-repo backfill.
    BackfillStatus(BackfillStatusArgs),
    /// Count how many ancestors of a given commit weren't derived
    CountUnderived(CountUnderivedArgs),
    /// Actually derive data
    Derive(DeriveArgs),
    /// Derive data for a slice of commits
    DeriveSlice(DeriveSliceArgs),
    /// Inspect and manage the enabled_derived_data_types table
    EnabledTypes(EnabledTypesArgs),
    /// Check if derived data has been generated
    Exists(ExistsArgs),
    /// Fetch previously derived data for the given commits
    Fetch(FetchArgs),
    /// Walk back from a head sampling every Nth ancestor (by generation) and
    /// flag any sampled commit that isn't derived for the given type
    FindDerivationGaps(FindDerivationGapsArgs),
    /// List the contents of a manifest at a given path
    ListManifest(ListManifestArgs),
    /// Slice underived ancestors of given commits
    Slice(SliceArgs),
    /// Validate direct augmented-manifest derivation against stored data
    VerifyAugDirect(VerifyAugDirectArgs),
    /// Compare augmented manifests stored at tree upload with canonical derivation, in memory
    VerifyAugUpload(VerifyAugUploadArgs),
    /// Compare different manifest types to ensure they are equivalent
    VerifyManifests(VerifyManifestsArgs),
    /// Verify stage derivation output matches normal derivation output
    VerifyStageOutput(VerifyStageOutputArgs),
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    // Use the unixname-aware context so BackfillEnqueue records the invoking
    // operator in `created_by` on the async request row. Read-only subcommands
    // don't care about the identity; the extra identity is harmless there.
    let mut ctx = crate::user_ctx::new_basic_context_with_unixname(&app);

    // BackfillAbort doesn't require opening a repo
    if let DerivedDataSubcommand::BackfillAbort(backfill_abort_args) = args.subcommand {
        return backfill_abort(ctx, &app, backfill_abort_args).await;
    }

    // EnabledTypes opens its own repo per action (from its own -R flag) and
    // ignores the top-level repo args.
    if let DerivedDataSubcommand::EnabledTypes(enabled_types_args) = args.subcommand {
        return enabled_types(&ctx, &app, enabled_types_args).await;
    }

    // BackfillReconcileConfigs is cross-repo (reads the global enabled-types
    // table + all repo configs) and opens its own minimal container; it ignores
    // the top-level repo args.
    if let DerivedDataSubcommand::BackfillReconcileConfigs(reconcile_args) = args.subcommand {
        return backfill_reconcile_configs(&ctx, &app, reconcile_args).await;
    }

    let repo_arg_list = args.repo.ids_or_names();

    // BackfillStatus: repo is optional (enriches boundary derivation checks)
    if let DerivedDataSubcommand::BackfillStatus(status_args) = args.subcommand {
        let opt_repo: Option<Repo> = match repo_arg_list.as_slice() {
            [single] => {
                let ra = match single {
                    RepoArg::Id(id) => RepoArgs::from_repo_id(id.id()),
                    RepoArg::Name(name) => RepoArgs::from_repo_name(name.clone()),
                };
                Some(
                    open_repo_for_derive(&app, &ra, false, args.bypass_redaction)
                        .await
                        .context("Failed to open repo")?,
                )
            }
            _ => None,
        };
        let opt_manager = match (&opt_repo, &args.config_name) {
            (Some(repo), Some(cfg)) => {
                let mgr = repo.repo_derived_data().manager_for_config(cfg)?;
                let mut config = mgr.config().clone();
                config.types = DerivableType::iter().collect();
                Some(mgr.with_replaced_config(mgr.config_name(), config))
            }
            (Some(repo), None) => {
                let mgr = repo.repo_derived_data().manager();
                let mut config = mgr.config().clone();
                config.types = DerivableType::iter().collect();
                Some(mgr.with_replaced_config(mgr.config_name(), config))
            }
            _ => None,
        };
        let sql_queue = async_requests_client::open_sql_connection(ctx.fb, &app).await?;
        let blobstore = async_requests_client::open_blobstore(ctx.fb, &app).await?;
        // Deep-sharded repos aren't in the legacy config blob backing
        // repo_configs(); their names live only in the tier manifest. Union both
        // so the status table can resolve ids repo_configs() doesn't know.
        // Manifest entries come first so repo_configs() entries win on collect.
        let repo_configs = app.repo_configs();
        let manifest = app.configs().manifest();
        let repo_names: HashMap<RepositoryId, String> = manifest
            .iter()
            .flat_map(|manifest| &manifest.repos)
            .map(|entry| (RepositoryId::new(entry.repo_id), entry.repo_name.clone()))
            .chain(
                repo_configs
                    .iter()
                    .map(|(name, repo_config)| (repo_config.repoid, name.clone())),
            )
            .collect();
        return backfill_status(
            &ctx,
            &app,
            sql_queue,
            blobstore,
            repo_names,
            status_args,
            opt_repo.as_ref(),
            opt_manager.as_ref(),
        )
        .await;
    }

    // BackfillEnqueue supports multiple repos
    if let DerivedDataSubcommand::BackfillEnqueue(enqueue_args) = args.subcommand {
        let queue = async_requests_client::build(ctx.fb, &app, None)
            .await
            .context("acquiring the async requests queue")?;
        return backfill_enqueue(
            &ctx,
            &app,
            queue,
            enqueue_args,
            &repo_arg_list,
            args.config_name.as_deref(),
            args.bypass_redaction,
        )
        .await;
    }

    // All remaining subcommands require exactly one repo
    let repo_args = match repo_arg_list.as_slice() {
        [] => anyhow::bail!("--repo-id or --repo-name is required for this subcommand"),
        [RepoArg::Id(id)] => RepoArgs::from_repo_id(id.id()),
        [RepoArg::Name(name)] => RepoArgs::from_repo_name(name.clone()),
        _ => anyhow::bail!("this subcommand requires exactly one repo"),
    };

    let repo: Repo = match &args.subcommand {
        DerivedDataSubcommand::BackfillAbort(_)
        | DerivedDataSubcommand::BackfillEnqueue(_)
        | DerivedDataSubcommand::BackfillReconcileConfigs(_)
        | DerivedDataSubcommand::BackfillStatus(_)
        | DerivedDataSubcommand::EnabledTypes(_) => {
            unreachable!("handled above")
        }
        DerivedDataSubcommand::Exists(_)
        | DerivedDataSubcommand::Fetch(_)
        | DerivedDataSubcommand::CountUnderived(_)
        | DerivedDataSubcommand::FindDerivationGaps(_)
        | DerivedDataSubcommand::VerifyAugDirect(_)
        | DerivedDataSubcommand::VerifyAugUpload(_)
        | DerivedDataSubcommand::VerifyManifests(_)
        | DerivedDataSubcommand::VerifyStageOutput(_)
        | DerivedDataSubcommand::ListManifest(_)
        | DerivedDataSubcommand::Slice(_) => {
            open_repo_for_derive(&app, &repo_args, false, args.bypass_redaction)
                .await
                .context("Failed to open repo")?
        }
        DerivedDataSubcommand::Derive(DeriveArgs { rederive, .. })
        | DerivedDataSubcommand::DeriveSlice(DeriveSliceArgs { rederive, .. }) => {
            open_repo_for_derive(&app, &repo_args, *rederive, args.bypass_redaction)
                .await
                .context("Failed to open repo")?
        }
    };

    let manager = if let Some(config_name) = args.config_name {
        repo.repo_derived_data().manager_for_config(&config_name)?
    } else {
        repo.repo_derived_data().manager()
    };

    let is_read_only = matches!(
        &args.subcommand,
        DerivedDataSubcommand::Exists(_)
            | DerivedDataSubcommand::Fetch(_)
            | DerivedDataSubcommand::CountUnderived(_)
            | DerivedDataSubcommand::FindDerivationGaps(_)
            | DerivedDataSubcommand::ListManifest(_)
            | DerivedDataSubcommand::Slice(_)
            | DerivedDataSubcommand::VerifyAugDirect(_)
            | DerivedDataSubcommand::VerifyAugUpload(_)
            | DerivedDataSubcommand::VerifyStageOutput(_)
    );

    let manager = if is_read_only {
        let mut config = manager.config().clone();
        config.types = DerivableType::iter().collect();
        manager.with_replaced_config(manager.config_name(), config)
    } else {
        manager.clone()
    };

    match args.subcommand {
        DerivedDataSubcommand::BackfillAbort(_)
        | DerivedDataSubcommand::BackfillEnqueue(_)
        | DerivedDataSubcommand::BackfillReconcileConfigs(_)
        | DerivedDataSubcommand::BackfillStatus(_)
        | DerivedDataSubcommand::EnabledTypes(_) => {
            unreachable!("handled above")
        }
        DerivedDataSubcommand::Exists(args) => exists(&ctx, &repo, &manager, args).await?,
        DerivedDataSubcommand::Fetch(args) => fetch(&ctx, &repo, &manager, args).await?,
        DerivedDataSubcommand::CountUnderived(args) => {
            count_underived(&ctx, &repo, &manager, args).await?
        }
        DerivedDataSubcommand::FindDerivationGaps(args) => {
            find_derivation_gaps(&ctx, &repo, &manager, args).await?
        }
        DerivedDataSubcommand::VerifyAugDirect(args) => {
            verify_aug_direct(&ctx, &repo, &manager, args).await?
        }
        DerivedDataSubcommand::VerifyAugUpload(args) => {
            verify_aug_upload(&ctx, &repo, &manager, args).await?
        }
        DerivedDataSubcommand::VerifyManifests(args) => verify_manifests(&ctx, &repo, args).await?,
        DerivedDataSubcommand::ListManifest(args) => {
            list_manifest(&ctx, &repo, &manager, args).await?
        }
        DerivedDataSubcommand::Derive(args) => derive(&mut ctx, &repo, &manager, args).await?,
        DerivedDataSubcommand::Slice(args) => slice(&ctx, &repo, &manager, args).await?,
        DerivedDataSubcommand::DeriveSlice(args) => {
            derive_slice(&ctx, &repo, &manager, args).await?
        }
        DerivedDataSubcommand::VerifyStageOutput(args) => {
            verify_stage_output(&ctx, &repo, &manager, args).await?
        }
    }

    Ok(())
}

async fn open_repo_for_derive(
    app: &MononokeApp,
    repo: &RepoArgs,
    rederive: bool,
    bypass_redaction: bool,
) -> Result<Repo> {
    let repo_customization: Box<dyn Fn(&mut RepoFactory) -> &mut RepoFactory + Send> = if rederive {
        Box::new(|repo_factory| repo_factory.with_bonsai_hg_mapping_override())
    } else {
        Box::new(|repo_factory| repo_factory)
    };

    if bypass_redaction {
        app.open_repo_unredacted_with_factory_customization(repo, repo_customization)
            .await
    } else {
        app.open_repo_with_factory_customization(repo, repo_customization)
            .await
    }
}
