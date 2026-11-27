/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::future::Future;
use std::num::NonZeroU32;
use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::ops::RangeInclusive;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use blobstore_factory::BlobstoreArgDefaults;
use blobstore_factory::ReadOnlyStorage;
use blobstore_factory::ScrubAction;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use bonsai_p4_mapping::BonsaiP4Mapping;
use bonsai_svnrev_mapping::BonsaiSvnrevMapping;
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
use commit_graph::CommitGraphRef;
use commit_id::parse_commit_id;
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
use runner::is_panic;
use sql_commit_graph_storage::CommitGraphBulkFetcher;
use sql_commit_graph_storage::CommitGraphBulkFetcherRef;
use tokio::signal::unix::Signal;
use tokio::signal::unix::SignalKind;
use tokio::signal::unix::signal;
use tokio::time::Instant;
use tokio::time::interval_at;
use tracing::error;
use tracing::info;
use tracing::warn;
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
    bonsai_git_mapping: dyn BonsaiGitMapping,

    #[facet]
    bonsai_globalrev_mapping: dyn BonsaiGlobalrevMapping,

    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    bonsai_p4_mapping: dyn BonsaiP4Mapping,

    #[facet]
    bonsai_svnrev_mapping: dyn BonsaiSvnrevMapping,

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
    about = "Per-commit audit of reachable storage",
    after_help = "Exit status: 0 when everything was checked and no problem was found; 1 when \
something could not be checked; 2 for a command-line error; 3 when everything was checked and \
problems were found. An unreadable object is a problem, but a failed storage read, for example \
during a storage outage, is reported the same way: its error text tells which, and the run's \
status is then `unreadable` rather than `complete`. Commits whose data is not derived yet are \
counted and do not change the exit status. A run stopped by SIGINT (Ctrl-C) or SIGTERM logs its \
summary so far and exits 1.",
    group(
        ArgGroup::new("commits")
            .required(true)
            .args(["changeset_id", "bookmark", "chunk_size", "to"])
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

    /// Audit the commits after this one, by Bonsai or Hg hash, up to and
    /// including `--to`: every ancestor of `--to` that is not an ancestor of
    /// `--from`. Commits merged in from other branches are included; other
    /// branches and drafts are not, whatever their SQL IDs. If `--from` is not
    /// an ancestor of `--to`, the range goes back to where their histories
    /// meet. The range is counted from the commit graph's segments and
    /// walked once, holding only the walk's frontier in memory.
    #[clap(long, requires = "to", conflicts_with = "chunk_size", value_parser = commit_hash)]
    from: Option<String>,

    /// Last commit of the range that `--from` starts, by Bonsai or Hg hash.
    #[clap(long, requires = "from", conflicts_with = "chunk_size", value_parser = commit_hash)]
    to: Option<String>,

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

    /// Log the run's progress this often, in seconds: commits checked of the
    /// total, and problems found so far.
    #[clap(long, default_value = "60")]
    progress_interval_secs: NonZeroU64,
}

/// A commit named by a Bonsai hash (64 hexadecimal digits) or an Hg hash (40).
fn commit_hash(value: &str) -> Result<String> {
    ensure!(
        matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "expected a Bonsai hash (64 hexadecimal digits) or an Hg hash (40)"
    );
    Ok(value.to_string())
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<ExitCode> {
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
            read_qps: NonZeroU32::new(5000),
            ..Default::default()
        })
        .build::<StriderArgs>()?;
    match app.run_basic(run) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(error) if error.is::<ProblemsFound>() => {
            eprintln!("Error: {error}");
            Ok(ExitCode::from(3))
        }
        Err(error) if error.is::<StoppedBeforeChecking>() => {
            info!(
                status = "incomplete",
                commits = 0,
                stopped_by_signal = true,
                "Strider completed"
            );
            Err(error)
        }
        Err(error) => Err(error),
    }
}

/// The error of a run stopped by SIGINT or SIGTERM before it checked any
/// commit, which exits 1.
#[derive(Debug, thiserror::Error)]
#[error("Strider was stopped by a signal before it checked any commit")]
struct StoppedBeforeChecking;

/// SIGINT and SIGTERM, which stop a run. Once registered, they no longer end
/// the process by themselves.
struct StopSignals {
    interrupt: Signal,
    terminate: Signal,
}

impl StopSignals {
    fn new() -> Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    /// Waits for either signal.
    async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }

    /// Runs `work` to its end, unless a signal comes first.
    async fn unless_stopped<T>(&mut self, work: impl Future<Output = Result<T>>) -> Result<T> {
        tokio::select! {
            biased;
            _ = self.recv() => Err(StoppedBeforeChecking.into()),
            result = work => result,
        }
    }
}

/// The error of a run that checked everything and found problems, which
/// exits 3 rather than 1.
#[derive(Debug, thiserror::Error)]
#[error("Strider found {0} problems")]
struct ProblemsFound(u64);

async fn run(app: MononokeApp) -> Result<()> {
    let mut stop = StopSignals::new()?;
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
    let repo: Repo = stop
        .unless_stopped(async {
            app.open_repo_unredacted_with_factory_customization(&args.repo, |repo_factory| {
                repo_factory.with_blobstore_component_sampler(sampler.clone());
                if let Some(replicas) = &replicas {
                    repo_factory.with_scrub_handler(replicas.clone());
                }
                repo_factory
            })
            .await
            .context("failed to open repository")
        })
        .await?;
    let repo_name = repo.repo_identity().name().to_string();
    info!(
        repo = repo_name,
        read_qps_per_store = app
            .blobstore_options()
            .throttle_options
            .read_qps
            .map_or_else(|| "unlimited".to_string(), |qps| qps.to_string()),
        "Strider read rate"
    );
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
        error!(
            repo = repo_name,
            family = family.name,
            "Strider family unavailable: the repository has not enabled its derived data type, so the family cannot be checked"
        );
    }
    let repo = Arc::new(repo);
    let (commits, total_commits) = stop.unless_stopped(commits(&ctx, &repo, &args)).await?;
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
    let progress_interval = Duration::from_secs(args.progress_interval_secs.get());
    let mut progress = interval_at(Instant::now() + progress_interval, progress_interval);
    loop {
        let report = tokio::select! {
            biased;
            _ = stop.recv() => {
                summary.interrupted = true;
                break;
            }
            _ = progress.tick() => {
                info!(
                    repo = repo_name,
                    commits = summary.commits,
                    total_commits = total_commits
                        .map_or_else(|| "unknown".to_string(), |total| total.to_string()),
                    problems = summary.problems() + replica_gaps(replicas.as_deref()),
                    "Strider progress"
                );
                continue;
            }
            report = reports.next() => report,
        };
        let Some(report) = report else {
            break;
        };
        match report {
            Ok(report) => {
                report_commit(&repo_name, operation, &report, args.print_visited_nodes);
                summary.record(&report);
            }
            // The other commits are still audited.
            Err(error) if is_panic(&error) => {
                error!(
                    repo = repo_name,
                    operation = operation.as_str(),
                    cause = format!("{error:#}"),
                    "Strider audit panicked"
                );
                summary.record_panic();
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
    summary.unavailable_families = unavailable.len() as u64;
    summary.replica_gaps = replica_gaps(replicas.as_deref());
    let outcome = summary.outcome();
    info!(
        repo = repo_name,
        operation = operation.as_str(),
        status = summary.status(),
        commits = summary.commits,
        problems = summary.problems(),
        unreadable_problems = summary.unreadable_problems,
        incomplete_commits = summary.incomplete_commits,
        panicked_commits = summary.panicked_commits,
        unavailable_families = summary.unavailable_families,
        not_derived_ignored = summary.not_derived,
        excluded = summary.excluded,
        stopped_by_signal = summary.interrupted,
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
    match outcome {
        Outcome::Clean => Ok(()),
        Outcome::ProblemsFound => Err(ProblemsFound(summary.problems()).into()),
        Outcome::NotChecked if summary.interrupted => bail!(
            "Strider was stopped by a signal after checking {} commits",
            summary.commits
        ),
        Outcome::NotChecked => bail!(
            "Strider could not check everything: {} of {} commits incomplete, {} requested families unavailable",
            summary.incomplete_commits,
            summary.commits,
            summary.unavailable_families
        ),
    }
}

/// Missing replica copies found so far, with or without an audited object.
fn replica_gaps(replicas: Option<&ReplicaChecker>) -> u64 {
    replicas.map_or(0, |replicas| {
        replicas.attributed() + replicas.unattributed()
    })
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

/// The commits selected by the command line, and how many there are.
async fn commits(
    ctx: &CoreContext,
    repo: &Arc<Repo>,
    args: &StriderArgs,
) -> Result<(BoxStream<'static, Result<ChangesetId>>, Option<u64>)> {
    if let Some(id) = args.changeset_id {
        return Ok((stream::once(async move { Ok(id) }).boxed(), Some(1)));
    }
    if let Some(bookmark) = &args.bookmark {
        let id = repo
            .bookmarks()
            .get(ctx.clone(), bookmark, Freshness::MostRecent)
            .await
            .with_context(|| format!("failed to resolve bookmark {bookmark}"))?
            .with_context(|| format!("bookmark {bookmark} does not exist"))?;
        return Ok((stream::once(async move { Ok(id) }).boxed(), Some(1)));
    }
    if let (Some(from), Some(to)) = (&args.from, &args.to) {
        let from = parse_commit_id(ctx, repo, from)
            .await
            .context("Failed to resolve --from")?;
        let to = parse_commit_id(ctx, repo, to)
            .await
            .context("Failed to resolve --to")?;
        let (commits, count) = history_range(ctx, repo.commit_graph(), from, to).await?;
        return Ok((commits, Some(count)));
    }
    // The `commits` group requires exactly one source, so this is `--chunk-size`.
    let chunk_size = args
        .chunk_size
        .context("a commit source is required")?
        .get();
    let fetcher = repo.commit_graph_bulk_fetcher();
    let bounds = resolve_range(ctx, fetcher, args.repo_lower_bound, args.repo_upper_bound).await?;
    // Only for the log, so read from a replica, and a failure, such as a
    // count of a whole large repository running past the query time limit,
    // only leaves the count unknown.
    let ids = match fetcher.ids_in_range(ctx, bounds.clone(), false).await {
        Ok(ids) => Some(ids),
        Err(error) => {
            warn!(
                cause = format!("{error:#}"),
                "Strider could not count the SQL-ID range's commits"
            );
            None
        }
    };
    let ids_field = |id: fn(&RangeInclusive<u64>) -> u64| {
        ids.as_ref()
            .and_then(|ids| ids.ids.as_ref())
            .map_or_else(|| "unknown".to_string(), |ids| id(ids).to_string())
    };
    info!(
        lower = bounds.start,
        upper = bounds.end,
        first_sql_id = ids_field(|ids| *ids.start()),
        last_sql_id = ids_field(|ids| *ids.end()),
        commits = ids
            .as_ref()
            .map_or_else(|| "unknown".to_string(), |ids| ids.count.to_string()),
        "Strider SQL-ID range"
    );
    let commits = range_commits(ctx, fetcher, bounds, chunk_size as u64);
    Ok((commits, ids.map(|ids| ids.count)))
}

/// Every commit with an SQL ID in `bounds`, in SQL-ID order, read from the
/// primary database.
fn range_commits(
    ctx: &CoreContext,
    fetcher: &CommitGraphBulkFetcher,
    bounds: Range<u64>,
    chunk_size: u64,
) -> BoxStream<'static, Result<ChangesetId>> {
    fetcher
        .oldest_first_stream(ctx, bounds, chunk_size, true)
        .map_ok(|item| item.cs_id)
        .boxed()
}

/// The commits after `from` up to and including `to`, newest first: the
/// ancestors of `to` that are not ancestors of `from`, and how many there
/// are, counted from the commit graph's segments of them.
async fn history_range(
    ctx: &CoreContext,
    commit_graph: &CommitGraph,
    from: ChangesetId,
    to: ChangesetId,
) -> Result<(BoxStream<'static, Result<ChangesetId>>, u64)> {
    for (flag, id) in [("--from", from), ("--to", to)] {
        ensure!(
            commit_graph.exists(ctx, id).await?,
            "{flag}: commit {id} is not in the repository"
        );
    }
    let commits = commit_graph
        .ancestors_difference_segments(ctx, vec![to], vec![from])
        .await?
        .iter()
        .map(|segment| segment.length)
        .sum::<u64>();
    ensure!(
        commits > 0,
        "commit range is empty: --to {to} is --from {from} or one of its ancestors"
    );
    info!(from = %from, to = %to, commits, "Strider commit range");
    let stream = commit_graph
        .ancestors_difference_stream(ctx, vec![to], vec![from])
        .await?;
    Ok((stream, commits))
}

/// The SQL IDs `[lower, upper)`, each bound defaulting to the repository's,
/// read from the primary database, as the range's commits are: a replica can
/// hold a later SQL ID without an earlier one, and the range would then skip
/// the earlier commit.
async fn resolve_range(
    ctx: &CoreContext,
    fetcher: &CommitGraphBulkFetcher,
    lower: Option<u64>,
    upper: Option<u64>,
) -> Result<Range<u64>> {
    let repo_bounds = fetcher.repo_bounds(ctx, true).await?;
    let bounds: Range<u64> = lower.unwrap_or(repo_bounds.start)..upper.unwrap_or(repo_bounds.end);
    ensure!(
        bounds.end <= repo_bounds.end,
        "upper bound {} is past the repository's exclusive upper bound, {}",
        bounds.end,
        repo_bounds.end
    );
    ensure!(
        bounds.start < bounds.end,
        "SQL-ID range {bounds:?} is empty"
    );
    Ok(bounds)
}

/// How a run ends, which sets its exit status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    /// Everything was checked and no problem was found: exit 0.
    Clean,
    /// Something could not be checked: exit 1, whatever else was found.
    NotChecked,
    /// Everything was checked and problems were found: exit 3.
    ProblemsFound,
}

#[derive(Default)]
struct RunSummary {
    commits: u64,
    /// Commits with a family that could not be checked, or whose audit
    /// panicked.
    incomplete_commits: u64,
    panicked_commits: u64,
    /// Requested families the repository has not enabled.
    unavailable_families: u64,
    /// Commit and family pairs that ended at a missing or unreadable object.
    family_problems: u64,
    /// Those of `family_problems` whose object was unreadable, which a failed
    /// storage read also is.
    unreadable_problems: u64,
    /// Commit and family pairs whose own mapping is absent. They do not
    /// change the outcome.
    not_derived: u64,
    /// Commit and family pairs skipped by `--exclude-changeset`.
    excluded: u64,
    /// Whether a signal stopped the run before it checked every commit.
    interrupted: bool,
    replica_gaps: u64,
    validated_objects: u64,
    validation: ValidationSummary,
}

impl RunSummary {
    fn record(&mut self, report: &CommitReport) {
        self.commits += 1;
        let mut incomplete = false;
        for family in &report.families {
            match &family.status {
                FamilyStatus::NotDerived => self.not_derived += 1,
                FamilyStatus::Excluded => self.excluded += 1,
                FamilyStatus::Incomplete(failure) if failure.found_problem() => {
                    self.family_problems += 1;
                    if failure.class == "unreadable" {
                        self.unreadable_problems += 1;
                    }
                }
                FamilyStatus::Incomplete(_) => incomplete = true,
                FamilyStatus::Validated => {}
            }
            self.validated_objects += family.validated_objects;
            self.validation.passed += family.validation.passed;
            self.validation.failed += family.validation.failed;
            self.validation.not_applicable += family.validation.not_applicable;
        }
        if incomplete {
            self.incomplete_commits += 1;
        }
    }

    /// Records a commit whose audit panicked, so that nothing is known of it.
    fn record_panic(&mut self) {
        self.commits += 1;
        self.incomplete_commits += 1;
        self.panicked_commits += 1;
    }

    /// Missing or unreadable objects, failed checks and missing replica
    /// copies.
    fn problems(&self) -> u64 {
        self.family_problems + self.validation.failed + self.replica_gaps
    }

    /// `complete` when everything was checked, except that a run with an
    /// unreadable object is `unreadable`, since a failed storage read is
    /// reported the same way; otherwise `incomplete`.
    fn status(&self) -> &'static str {
        match self.outcome() {
            Outcome::NotChecked => "incomplete",
            Outcome::ProblemsFound if self.unreadable_problems > 0 => "unreadable",
            Outcome::Clean | Outcome::ProblemsFound => "complete",
        }
    }

    fn outcome(&self) -> Outcome {
        if self.interrupted || self.incomplete_commits > 0 || self.unavailable_families > 0 {
            Outcome::NotChecked
        } else if self.problems() > 0 {
            Outcome::ProblemsFound
        } else {
            Outcome::Clean
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
        match &family.status {
            FamilyStatus::Validated => {}
            FamilyStatus::NotDerived => info!(
                repo = repo_name,
                changeset_id = %changeset_id,
                family = family.family,
                "Strider family not derived"
            ),
            FamilyStatus::Excluded => info!(
                repo = repo_name,
                changeset_id = %changeset_id,
                family = family.family,
                "Strider family excluded"
            ),
            FamilyStatus::Incomplete(failure) => error!(
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
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use anyhow::Result;
    use commit_graph::BaseCommitGraphWriter;
    use commit_graph::ChangesetParents;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use commit_graph_testlib::utils::cs_id_name;
    use commit_graph_testlib::utils::from_dag;
    use commit_graph_testlib::utils::name_cs_id;
    use commit_graph_testlib::utils::test_repo_identity;
    use context::CoreContext;
    use fbinit::FacebookInit;
    use futures::TryStreamExt;
    use in_memory_commit_graph_storage::InMemoryCommitGraphStorage;
    use mononoke_macros::mononoke;
    use mononoke_types::audit::ObjectId;
    use mononoke_types::audit::WalkStage;
    use rendezvous::RendezVousOptions;
    use sql_commit_graph_storage::CommitGraphBulkFetcher;
    use sql_commit_graph_storage::IdsInRange;
    use sql_commit_graph_storage::SqlCommitGraphStorageBuilder;
    use sql_construct::SqlConstruct;
    use sql_ext::SqlConnections;
    use sql_ext::open_sqlite_in_memory;

    use super::*;
    use crate::runner::Failure;
    use crate::runner::FamilyReport;

    /// An empty commit graph database.
    fn commit_graph_database() -> Result<sql_ext::Connection> {
        let conn = open_sqlite_in_memory()?;
        let shard_name = conn.db_name(0)?;
        conn.execute_batch(SqlCommitGraphStorageBuilder::CREATION_QUERY)?;
        Ok(sql_ext::Connection {
            inner: sql::Connection::with_sqlite(conn),
            shard_name,
        })
    }

    /// A commit graph of A, B and C in a line, with SQL IDs 1 to 3, that only
    /// its primary database holds: the replica has not seen any commit yet.
    async fn unreplicated_linear_graph(
        ctx: &CoreContext,
    ) -> Result<(CommitGraphBulkFetcher, [ChangesetId; 3])> {
        let primary = commit_graph_database()?;
        let storage = Arc::new(
            SqlCommitGraphStorageBuilder::from_sql_connections(SqlConnections {
                write_connection: primary.clone(),
                read_connection: commit_graph_database()?,
                read_master_connection: primary,
            })
            .build(RendezVousOptions::for_test(), test_repo_identity()),
        );
        let writer = BaseCommitGraphWriter::new(CommitGraph::new(storage.clone()));
        let (a, b, c) = (name_cs_id("A"), name_cs_id("B"), name_cs_id("C"));
        for (commit, parents) in [(a, vec![]), (b, vec![a]), (c, vec![b])] {
            let parents = ChangesetParents::from_vec(parents);
            writer.add(ctx, commit, parents, Default::default()).await?;
        }
        let fetcher = CommitGraphBulkFetcher::new(storage);
        Ok((fetcher, [a, b, c]))
    }

    #[mononoke::fbinit_test]
    async fn a_range_includes_commits_the_replica_has_not_seen(fb: FacebookInit) -> Result<()> {
        // Given a commit graph whose replica has not seen any of its three
        // commits yet
        let ctx = CoreContext::test_mock(fb);
        let (fetcher, [a, b, c]) = unreplicated_linear_graph(&ctx).await?;

        // When the whole range is listed
        let bounds = resolve_range(&ctx, &fetcher, None, None).await?;
        let commits: Vec<_> = range_commits(&ctx, &fetcher, bounds, 2)
            .try_collect()
            .await?;

        // Then every commit is in it
        assert_eq!(commits, vec![a, b, c]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_range_is_resolved_from_the_primary_and_counted(fb: FacebookInit) -> Result<()> {
        // Given a commit graph whose replica has not seen any of its three
        // commits yet, with SQL IDs 1 to 3
        let ctx = CoreContext::test_mock(fb);
        let (fetcher, _) = unreplicated_linear_graph(&ctx).await?;

        // When the range from SQL ID 2 to the newest commit is resolved
        let bounds = resolve_range(&ctx, &fetcher, Some(2), None).await?;

        // Then it ends after C, and holds B and C, with SQL IDs 2 and 3
        assert_eq!(bounds, 2..4);
        assert_eq!(
            fetcher.ids_in_range(&ctx, bounds, true).await?,
            IdsInRange {
                count: 2,
                ids: Some(2..=3)
            }
        );
        Ok(())
    }

    /// A and B, then C and D on two branches merged by M, then F; W, a
    /// draft on B; and X and Y, a branch with no common history.
    async fn branching_graph(ctx: &CoreContext) -> Result<CommitGraph> {
        from_dag(
            ctx,
            r"
                A-B-C-M-F
                   \ /
                    D
                B-W
                X-Y
            ",
            Arc::new(InMemoryCommitGraphStorage::new(test_repo_identity())),
        )
        .await
    }

    /// The commits `history_range` lists from `from` to `to`, by name, and
    /// the count it gave for them.
    async fn history(
        ctx: &CoreContext,
        graph: &CommitGraph,
        from: &str,
        to: &str,
    ) -> Result<(BTreeSet<String>, u64)> {
        let (commits, count) = history_range(ctx, graph, name_cs_id(from), name_cs_id(to)).await?;
        let names = commits.map_ok(cs_id_name).try_collect().await?;
        Ok((names, count))
    }

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[mononoke::fbinit_test]
    async fn a_history_range_is_what_to_adds_to_from(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let graph = branching_graph(&ctx).await?;

        // A linear range excludes `--from` and includes `--to`.
        assert_eq!(
            history(&ctx, &graph, "A", "C").await?,
            (names(&["B", "C"]), 2)
        );

        // Both sides of a merge inside the range are in it, and the draft W
        // and the branch X-Y are not, though their SQL IDs fall inside it.
        assert_eq!(
            history(&ctx, &graph, "B", "F").await?,
            (names(&["C", "D", "M", "F"]), 4)
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_history_range_from_a_commit_off_the_history_starts_where_they_meet(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let graph = branching_graph(&ctx).await?;

        // W is not an ancestor of F; their histories meet at B.
        assert_eq!(
            history(&ctx, &graph, "W", "F").await?,
            (names(&["C", "D", "M", "F"]), 4)
        );

        // Y shares no history with F, so the range is all of F's history.
        assert_eq!(
            history(&ctx, &graph, "Y", "F").await?,
            (names(&["A", "B", "C", "D", "M", "F"]), 6)
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn an_empty_or_unknown_history_range_is_an_error(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let graph = branching_graph(&ctx).await?;

        let empty = history(&ctx, &graph, "F", "B")
            .await
            .expect_err("B is an ancestor of F");
        assert!(format!("{empty:#}").contains("is empty"), "{empty:#}");

        let unknown = history(&ctx, &graph, "A", "Z")
            .await
            .expect_err("Z is not in the graph");
        assert!(
            format!("{unknown:#}").contains("is not in the repository"),
            "{unknown:#}"
        );
        Ok(())
    }

    fn report(statuses: impl IntoIterator<Item = FamilyStatus>) -> CommitReport {
        CommitReport {
            changeset_id: name_cs_id("A"),
            families: statuses
                .into_iter()
                .map(|status| FamilyReport {
                    status,
                    ..FamilyReport::new("core")
                })
                .collect(),
        }
    }

    fn failure(object: Option<ObjectId>, class: &'static str) -> FamilyStatus {
        FamilyStatus::Incomplete(Failure {
            stage: WalkStage::Load,
            object,
            class,
            cause: class.to_string(),
            storage_key: None,
        })
    }

    fn object() -> Option<ObjectId> {
        Some(ObjectId::of::<Repo, _>(&name_cs_id("A")))
    }

    fn outcome_of(reports: impl IntoIterator<Item = CommitReport>) -> (Outcome, RunSummary) {
        let mut summary = RunSummary::default();
        for report in reports {
            summary.record(&report);
        }
        (summary.outcome(), summary)
    }

    #[mononoke::test]
    fn everything_validated_is_clean() {
        let (outcome, _) = outcome_of([report([FamilyStatus::Validated, FamilyStatus::Excluded])]);
        assert_eq!(outcome, Outcome::Clean);
    }

    #[mononoke::test]
    fn excluded_families_are_counted_and_still_clean() {
        let (outcome, summary) = outcome_of([
            report([FamilyStatus::Excluded, FamilyStatus::Excluded]),
            report([FamilyStatus::Validated]),
        ]);
        assert_eq!(outcome, Outcome::Clean);
        assert_eq!(summary.excluded, 2, "one per excluded commit and family");
    }

    #[mononoke::test]
    fn an_interrupted_run_is_not_checked() {
        let (_, mut summary) = outcome_of([report([FamilyStatus::Validated])]);
        summary.interrupted = true;
        assert_eq!(summary.outcome(), Outcome::NotChecked);
        assert_eq!(summary.status(), "incomplete");
    }

    #[mononoke::test]
    fn commits_not_derived_are_counted_and_still_clean() {
        let (outcome, summary) = outcome_of([
            report([FamilyStatus::NotDerived]),
            report([FamilyStatus::NotDerived, FamilyStatus::Validated]),
        ]);
        assert_eq!(outcome, Outcome::Clean);
        assert_eq!(summary.not_derived, 2);
    }

    #[mononoke::test]
    fn a_missing_or_unreadable_object_is_a_problem() {
        for class in ["missing", "unreadable"] {
            let (outcome, summary) = outcome_of([report([failure(object(), class)])]);
            assert_eq!(outcome, Outcome::ProblemsFound, "{class}");
            assert_eq!(summary.problems(), 1, "{class}");
        }
    }

    #[mononoke::test]
    fn the_status_is_complete_only_when_no_problem_is_unreadable() {
        let (_, missing) = outcome_of([report([failure(object(), "missing")])]);
        assert_eq!(missing.status(), "complete");
        let (outcome, unreadable) = outcome_of([report([failure(object(), "unreadable")])]);
        assert_eq!(outcome, Outcome::ProblemsFound);
        assert_eq!(unreadable.status(), "unreadable");
        let (_, not_checked) = outcome_of([
            report([failure(object(), "unreadable")]),
            report([failure(None, "unreadable")]),
        ]);
        assert_eq!(not_checked.status(), "incomplete");
    }

    #[mononoke::test]
    fn a_failed_check_is_a_problem() {
        let mut checked = report([FamilyStatus::Validated]);
        checked.families[0].validation.failed = 1;
        let (outcome, _) = outcome_of([checked]);
        assert_eq!(outcome, Outcome::ProblemsFound);
    }

    #[mononoke::test]
    fn a_missing_replica_copy_is_a_problem() {
        let (_, mut summary) = outcome_of([report([FamilyStatus::Validated])]);
        summary.replica_gaps = 1;
        assert_eq!(summary.outcome(), Outcome::ProblemsFound);
    }

    #[mononoke::test]
    fn a_failure_naming_no_object_is_not_checked() {
        // Such as a commit whose parents could not be listed
        let (outcome, summary) = outcome_of([report([failure(None, "unreadable")])]);
        assert_eq!(outcome, Outcome::NotChecked);
        assert_eq!(summary.incomplete_commits, 1);
    }

    #[mononoke::test]
    fn an_unavailable_family_is_not_checked() {
        let (_, mut summary) = outcome_of([report([FamilyStatus::Validated])]);
        summary.unavailable_families = 1;
        assert_eq!(summary.outcome(), Outcome::NotChecked);
    }

    #[mononoke::test]
    fn a_panicked_audit_is_not_checked_and_counted() {
        let (_, mut summary) = outcome_of([report([FamilyStatus::Validated])]);
        summary.record_panic();
        assert_eq!(summary.outcome(), Outcome::NotChecked);
        assert_eq!(
            (
                summary.commits,
                summary.incomplete_commits,
                summary.panicked_commits
            ),
            (2, 1, 1)
        );
    }

    #[mononoke::test]
    fn not_checked_wins_over_problems() {
        let (outcome, _) = outcome_of([
            report([failure(object(), "missing")]),
            report([failure(None, "unreadable")]),
        ]);
        assert_eq!(outcome, Outcome::NotChecked);
    }
}
