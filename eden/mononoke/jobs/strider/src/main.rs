/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use blobstore_factory::BlobstoreArgDefaults;
use blobstore_factory::ReadOnlyStorage;
use blobstore_factory::ScrubAction;
use bookmarks::BookmarkKey;
use bookmarks::Bookmarks;
use bookmarks::BookmarksRef;
use bookmarks::Freshness;
use clap::ArgGroup;
use clap::Parser;
use clap::builder::PossibleValuesParser;
use cmdlib_caching::CacheMode;
use cmdlib_caching::CachelibSettings;
use cmdlib_scrubbing::zero_scrub_stats;
use commit_graph::CommitGraph;
use context::CoreContext;
use environment::Caching;
use fbinit::FacebookInit;
use futures::TryStreamExt;
use futures::stream;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use metaconfig_types::BlobConfig;
use metaconfig_types::RepoConfig;
use metaconfig_types::RepoConfigRef;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::args::AsRepoArg;
use mononoke_app::args::RepoArgs;
use mononoke_types::ChangesetId;
use mononoke_types::audit::LoadTags;
use reads::ReadSampler;
use replicas::ReplicaCheckAppExtension;
use replicas::ReplicaChecker;
use replicas::SCRUB_STATS_KEY;
use repo_blobstore::RepoBlobstore;
use repo_derived_data::RepoDerivedData;
use repo_identity::RepoIdentity;
use repo_identity::RepoIdentityRef;
use runner::CommitReport;
use runner::FamilyStatus;
use runner::Operation;
use runner::Strider;
use runner::StriderLimits;
use sql_commit_graph_storage::CommitGraphBulkFetcher;
use sql_commit_graph_storage::CommitGraphBulkFetcherRef;
use tracing::error;
use tracing::info;
use validation::ValidationSummary;

mod families;
pub mod reads;
pub mod replicas;
pub mod runner;
#[cfg(test)]
mod test_utils;
pub mod validation;

#[facet::container]
pub(crate) struct Repo {
    #[facet]
    bookmarks: dyn Bookmarks,

    #[facet]
    commit_graph: CommitGraph,

    #[facet]
    commit_graph_bulk_fetcher: CommitGraphBulkFetcher,

    #[facet]
    repo_blobstore: RepoBlobstore,

    #[facet]
    repo_config: RepoConfig,

    #[facet]
    repo_derived_data: RepoDerivedData,

    #[facet]
    repo_identity: RepoIdentity,
}

fn family_names() -> PossibleValuesParser {
    PossibleValuesParser::new(
        families::families::<Repo>()
            .into_iter()
            .map(|family| family.name),
    )
}

#[derive(Parser)]
#[clap(
    about = "Prototype per-commit audit of reachable storage",
    group(
        ArgGroup::new("commits")
            .required(true)
            .args(["changeset_id", "bookmark", "chunk_size"])
    )
)]
struct StriderArgs {
    /// Repository to inspect.
    #[clap(flatten)]
    repo: RepoArgs,

    /// Audit what this Bonsai changeset added.
    #[clap(long)]
    changeset_id: Option<ChangesetId>,

    /// Audit what the bookmarked changeset added.
    #[clap(long)]
    bookmark: Option<BookmarkKey>,

    /// Audit every changeset in an SQL-ID range, fetching this many IDs per query.
    #[clap(long)]
    chunk_size: Option<NonZeroUsize>,

    /// Inclusive lower SQL-ID bound for `--chunk-size`. Defaults to the repository bound.
    #[clap(long, requires = "chunk_size")]
    repo_lower_bound: Option<u64>,

    /// Exclusive upper SQL-ID bound for `--chunk-size`. Defaults to the repository bound.
    #[clap(long, requires = "chunk_size")]
    repo_upper_bound: Option<u64>,

    /// Skip this changeset: every family reports its commit excluded and reads
    /// nothing for it. Repeat to exclude several.
    #[clap(long)]
    exclude_changeset: Vec<ChangesetId>,

    /// Family to audit. Repeat to select several.
    #[clap(long, required = true, value_parser = family_names())]
    family: Vec<String>,

    /// Operation to perform on each added object.
    #[clap(long, value_enum, default_value = "walk")]
    operation: Operation,

    /// Maximum number of commits audited concurrently.
    #[clap(long)]
    max_concurrency: NonZeroUsize,

    /// Maximum number of objects one commit loads concurrently, counted
    /// separately while finding what the commit added and while validating it,
    /// and for each kind of read a family makes while finding: a family that
    /// loads its own objects for another walk's entries can have twice as many.
    #[clap(long)]
    max_loads_per_commit: NonZeroUsize,

    /// Print each validated object and each commit's status per family. Holds
    /// each commit's objects until it finishes, so memory grows with commit size.
    #[clap(long)]
    print_visited_nodes: bool,

    /// Print each read that reaches a store: the store, the object type it is
    /// counted under (or `untagged`, or `outside` for reads such as opening the
    /// repository), the key, and its size (or `absent`).
    #[clap(long)]
    print_reads: bool,
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    // As in Walker: the cache is local to the process and starts empty, so an
    // object's first read always reaches storage (and, when scrubbing, every
    // replica). A shared cache such as memcache could answer that read instead.
    let cachelib_settings = CachelibSettings {
        cache_size: 2 * 1024 * 1024 * 1024,
        ..Default::default()
    };
    let app = MononokeAppBuilder::new(fb)
        .with_cachelib_settings(cachelib_settings)
        .with_app_extension(ReplicaCheckAppExtension)
        .with_arg_defaults(CacheMode::LocalOnly)
        .with_arg_defaults(ReadOnlyStorage(true))
        .with_arg_defaults(BlobstoreArgDefaults {
            read_qps: NonZeroU32::new(20000),
            ..Default::default()
        })
        .build::<StriderArgs>()?;
    app.run_basic(run)
}

async fn run(app: MononokeApp) -> Result<()> {
    let args: StriderArgs = app.args()?;
    if let Caching::Enabled(_) = app.environment().caching {
        bail!(
            "a shared cache could answer for blobs that storage has lost: use --cache-mode local-only or disabled"
        );
    }
    let tags = Arc::new(LoadTags::default());
    let sampler = Arc::new(ReadSampler::new(tags.clone(), args.print_reads));
    let replicas = replica_checker(&app, &args.repo, &tags)?;
    // As in Walker: redacted content is still stored, so it is read and
    // checked like any other.
    let repo: Repo = app
        .open_repo_unredacted_with_factory_customization(&args.repo, |repo_factory| {
            repo_factory.with_blobstore_component_sampler(sampler.clone());
            if let Some(replicas) = &replicas {
                repo_factory.with_scrub_handler(replicas.clone());
            }
            repo_factory
        })
        .await
        .context("failed to open repository")?;
    let repo_name = repo.repo_identity().name().to_string();
    let ctx = app.new_basic_context();
    let operation = args.operation;
    let derived_data_config = &repo.repo_config().derived_data_config;
    let (families, unavailable): (Vec<_>, Vec<_>) = families::families::<Repo>()
        .into_iter()
        .filter(|family| args.family.iter().any(|name| name == family.name))
        .partition(|family| {
            family
                .derived_type
                .is_none_or(|derived_type| derived_data_config.is_enabled(derived_type))
        });
    for family in &unavailable {
        info!(
            repo = repo_name,
            family = family.name,
            "Strider family unavailable: the repository has not enabled its derived data type"
        );
    }
    let repo = Arc::new(repo);
    let commits = commits(&ctx, &repo, &args).await?;
    let strider = Arc::new(Strider::new(
        ctx,
        repo,
        families,
        args.exclude_changeset.iter().copied().collect(),
        operation,
        StriderLimits {
            max_concurrency: args.max_concurrency,
            max_loads_per_commit: args.max_loads_per_commit,
        },
        args.print_visited_nodes,
        tags,
    ));

    let mut summary = RunSummary::default();
    let mut reports = std::pin::pin!(strider.audit(commits));
    while let Some(report) = reports.try_next().await.transpose() {
        match report {
            Ok(report) => {
                report_commit(&repo_name, operation, &report, args.print_visited_nodes);
                summary.record(&report);
            }
            Err(error) => {
                error!(
                    repo = repo_name,
                    operation = operation.as_str(),
                    status = "incomplete",
                    cause = format!("{error:#}"),
                    "Strider failed to list or audit commits"
                );
                return Err(error);
            }
        }
    }

    for (object_type, totals) in sampler.totals() {
        info!(
            repo = repo_name,
            object_type = %object_type,
            keys = totals.keys,
            bytes = totals.bytes,
            "Strider reads"
        );
    }
    let status = if summary.incomplete_commits == 0 {
        "complete"
    } else {
        "incomplete"
    };
    info!(
        repo = repo_name,
        operation = operation.as_str(),
        status,
        commits = summary.commits,
        incomplete_commits = summary.incomplete_commits,
        not_derived_ignored = summary.not_derived,
        visited_nodes = summary.validated_objects,
        validation_passed = summary.validation.passed,
        validation_failed = summary.validation.failed,
        validation_not_applicable = summary.validation.not_applicable,
        replica_gaps = replicas
            .as_ref()
            .map_or(0, |replicas| replicas.attributed()),
        replica_gaps_unattributed = replicas
            .as_ref()
            .map_or(0, |replicas| replicas.unattributed()),
        untagged_reads = sampler.untagged_reads(),
        reads_outside_audit = sampler.reads_outside_audit(),
        "Strider completed"
    );
    if summary.incomplete_commits > 0 {
        bail!(
            "{} of {} commits are incomplete",
            summary.incomplete_commits,
            summary.commits
        );
    }
    Ok(())
}

/// The handler for missing copies when `--check-replicas` turned on scrub
/// reads, with the repo's counters set to zero for each store.
fn replica_checker(
    app: &MononokeApp,
    repo_args: &RepoArgs,
    tags: &Arc<LoadTags>,
) -> Result<Option<Arc<ReplicaChecker>>> {
    let Some(scrub_options) = &app.blobstore_options().scrub_options else {
        return Ok(None);
    };
    if scrub_options.scrub_action != ScrubAction::ReportOnly {
        bail!("Strider only reports missing copies, it never repairs them");
    }
    let (repo_name, repo_config) = app.repo_config(repo_args.as_repo_arg())?;
    let BlobConfig::MultiplexedWal { blobstores, .. } = &repo_config.storage_config.blobstore
    else {
        bail!("replica checks need a multiplexed blobstore: {repo_name} has one store");
    };
    zero_scrub_stats(
        SCRUB_STATS_KEY,
        &repo_name,
        blobstores.iter().map(|(id, _store_type, _config)| *id),
    );
    Ok(Some(Arc::new(ReplicaChecker::new(
        tags.clone(),
        app.environment().scuba_sample_builder.clone(),
        repo_config.repoid,
        repo_name,
    ))))
}

/// The commits selected by the command line, in SQL-ID order for a range.
async fn commits(
    ctx: &CoreContext,
    repo: &Arc<Repo>,
    args: &StriderArgs,
) -> Result<BoxStream<'static, Result<ChangesetId>>> {
    if let Some(id) = args.changeset_id {
        return Ok(stream::once(async move { Ok(id) }).boxed());
    }
    if let Some(bookmark) = &args.bookmark {
        let id = repo
            .bookmarks()
            .get(ctx.clone(), bookmark, Freshness::MostRecent)
            .await
            .with_context(|| format!("failed to resolve bookmark {bookmark}"))?
            .with_context(|| format!("bookmark {bookmark} does not exist"))?;
        return Ok(stream::once(async move { Ok(id) }).boxed());
    }
    // The `commits` group requires exactly one source, so this is `--chunk-size`.
    let chunk_size = args
        .chunk_size
        .context("a commit source is required")?
        .get();
    let fetcher = repo.commit_graph_bulk_fetcher();
    let repo_bounds = fetcher.repo_bounds(ctx, false).await?;
    let bounds = args.repo_lower_bound.unwrap_or(repo_bounds.start)
        ..args.repo_upper_bound.unwrap_or(repo_bounds.end);
    // The range is read from a replica, which may not have the newest commits
    // yet: a range past what it has would silently skip them.
    ensure!(
        bounds.end <= repo_bounds.end,
        "upper bound {} is past the replica's exclusive upper bound, {}",
        bounds.end,
        repo_bounds.end
    );
    let explicit = args.repo_lower_bound.is_some() || args.repo_upper_bound.is_some();
    ensure!(
        !explicit || bounds.start < bounds.end,
        "SQL-ID range {bounds:?} is empty"
    );
    info!(
        lower = bounds.start,
        upper = bounds.end,
        "Strider SQL-ID range"
    );
    Ok(fetcher
        .oldest_first_stream(ctx, bounds, chunk_size as u64, false)
        .map_ok(|item| item.cs_id)
        .boxed())
}

#[derive(Default)]
struct RunSummary {
    commits: u64,
    incomplete_commits: u64,
    /// Commit and family pairs whose own mapping is absent.
    not_derived: u64,
    validated_objects: u64,
    validation: ValidationSummary,
}

impl RunSummary {
    fn record(&mut self, report: &CommitReport) {
        self.commits += 1;
        if report.is_incomplete() {
            self.incomplete_commits += 1;
        }
        for family in &report.families {
            if matches!(family.status, FamilyStatus::NotDerived) {
                self.not_derived += 1;
            }
            self.validated_objects += family.validated_objects;
            self.validation.passed += family.validation.passed;
            self.validation.failed += family.validation.failed;
            self.validation.not_applicable += family.validation.not_applicable;
        }
    }
}

fn report_commit(
    repo_name: &str,
    operation: Operation,
    report: &CommitReport,
    print_visited_nodes: bool,
) {
    let changeset_id = report.changeset_id;
    for family in &report.families {
        if print_visited_nodes {
            for object in &family.validated {
                println!("{changeset_id} {} {object}", family.family);
            }
            let status = match family.status {
                FamilyStatus::Validated => "validated",
                FamilyStatus::NotDerived => "not_derived",
                FamilyStatus::Excluded => "excluded",
                FamilyStatus::Incomplete(_) => "incomplete",
            };
            println!("{changeset_id} {} status={status}", family.family);
        }
        if let Some(failure) = family.failure() {
            error!(
                repo = repo_name,
                operation = operation.as_str(),
                changeset_id = %changeset_id,
                family = family.family,
                failure_stage = failure.stage.as_str(),
                failure_class = failure.class,
                object = failure
                    .object
                    .as_ref()
                    .map_or_else(|| "none".to_string(), ToString::to_string),
                storage_key = failure.storage_key.as_deref().unwrap_or("none"),
                cause = failure.cause,
                "Strider family incomplete"
            );
        }
    }
}
