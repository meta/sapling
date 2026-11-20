/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Gives draft commits augmented manifests, and the ACL manifests they depend
//! on, by walking one task's share of each repo's commits oldest first.
//!
//! Each task walks its repos one at a time, so that it has one blobstore
//! throttle at a time. It logs one row per finished window that held commits
//! of the repo (the walk skips the others) and one per commit it could not
//! derive to its own Scuba dataset, and the derivation logs to a dataset of
//! their own.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::pin::pin;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use augmented_manifest_backfill::CommitOutcome;
use augmented_manifest_backfill::Failure;
use augmented_manifest_backfill::IdBounds;
use augmented_manifest_backfill::RepoBackfill;
use augmented_manifest_backfill::RepoWalk;
use augmented_manifest_backfill::TaskShard;
use augmented_manifest_backfill::WindowReport;
use clap::Parser;
use cmdlib_caching::CacheMode;
use commit_graph::CommitGraph;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::TryStreamExt;
use metaconfig_types::RepoConfig;
use metaconfig_types::RepoConfigRef;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::args::RepoArg;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use mononoke_types::ChangesetId;
use mutable_counters::MutableCounters;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentity;
use scuba_ext::MononokeScubaSampleBuilder;
use sql_commit_graph_storage::CommitGraphBulkFetcher;
use throttledblob::ThrottleOptions;
use tracing::error;
use tracing::info;
use wait_for_replication::WaitForReplication;

const SERVICE_NAME: &str = "augmented_manifest_backfill";

/// The replication lag limit the job waits on: the derived data backfiller's.
const REPLICATION_LAG_CONFIG: &str = "derived_data_backfiller";

const LOG_TAG_WINDOW_DONE: &str = "Draft backfill window done";
const LOG_TAG_FAILURE: &str = "Draft backfill failure";
const LOG_TAG_WALK_FAILED: &str = "Draft backfill walk failed";

/// Derives augmented manifests for one task's share of each repo's commits.
#[derive(Parser)]
struct BackfillArgs {
    /// A repo to walk. Repeat it, or separate names with commas, to walk
    /// several repos one after the other.
    #[clap(
        long = "repo-name",
        short = 'R',
        required = true,
        value_delimiter = ','
    )]
    repo_names: Vec<String>,

    /// `<repo>=<id>`: the lowest commit-graph id to walk in that repo.
    /// Without one, the walk starts at the repo's lowest id. Repeat it for
    /// each repo.
    #[clap(long = "start-id", value_parser = parse_repo_id)]
    start_ids: Vec<(String, u64)>,

    /// `<repo>=<id>`: the highest commit-graph id to walk in that repo.
    /// Without one, the walk stops at the repo's newest commit when it
    /// starts. Repeat it for each repo.
    #[clap(long = "end-id", value_parser = parse_repo_id)]
    end_ids: Vec<(String, u64)>,

    /// This task's index, from 0 to the task count minus 1.
    #[clap(long)]
    task_index: u64,

    /// How many tasks share the walk.
    #[clap(long)]
    task_count: u64,

    /// How many commit-graph ids a window covers.
    #[clap(long, default_value = "1000000")]
    window_size: NonZeroU64,

    /// The most blobstore reads a second for all of this task's derivations
    /// in a repo. 0 means no limit. Unlike `--blobstore-read-qps`, which
    /// limits each blobstore of a multiplex below the caches, this limit is
    /// shared by all the task's derivations and counts reads served from the
    /// caches too. The other three limits work the same way.
    #[clap(long)]
    throttle_read_qps: u32,

    /// The most blobstore writes a second for all of this task's derivations
    /// in a repo. 0 means no limit.
    #[clap(long)]
    throttle_write_qps: u32,

    /// The most bytes read from the blobstore a second for all of this
    /// task's derivations in a repo. 0 means no limit.
    #[clap(long)]
    throttle_read_bytes_s: usize,

    /// The most bytes written to the blobstore a second for all of this
    /// task's derivations in a repo. 0 means no limit.
    #[clap(long)]
    throttle_write_bytes_s: usize,

    /// The Scuba dataset for the job's window and failure rows. A value of
    /// `file://<path>` writes them to that file instead.
    #[clap(long, default_value = "mononoke_augmented_manifest_backfill")]
    backfill_scuba_dataset: String,

    /// The Scuba dataset for the derivation logs. A value of `file://<path>`
    /// writes them to that file instead.
    #[clap(
        long,
        default_value = "mononoke_derived_data_by_augmented_manifest_backfill"
    )]
    derived_data_scuba_dataset: String,
}

#[facet::container]
struct Repo {
    #[facet]
    repo_identity: RepoIdentity,
    #[facet]
    repo_config: RepoConfig,
    #[facet]
    commit_graph: CommitGraph,
    #[facet]
    commit_graph_bulk_fetcher: CommitGraphBulkFetcher,
    #[facet]
    mutable_counters: dyn MutableCounters,
    #[facet]
    repo_derived_data: RepoDerivedData,
}

/// What a task does in every repo it walks.
struct Task<'a> {
    app: &'a MononokeApp,
    ctx: CoreContext,
    shard: TaskShard,
    window_size: NonZeroU64,
    throttle: ThrottleOptions,
    derived_data_scuba_dataset: String,
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    let app = MononokeAppBuilder::new(fb)
        .with_arg_defaults(CacheMode::LocalOnly)
        .with_app_extension(MonitoringAppExtension {})
        .build::<BackfillArgs>()?;
    app.run_with_monitoring_and_logging(async_main, SERVICE_NAME, AliveService)
}

async fn async_main(app: MononokeApp) -> Result<()> {
    let args: BackfillArgs = app.args()?;
    let bounds = repo_bounds(&args.repo_names, &args.start_ids, &args.end_ids)?;
    let task = Task {
        app: &app,
        ctx: app.new_basic_context(),
        shard: TaskShard::try_new(args.task_index, args.task_count)?,
        window_size: args.window_size,
        throttle: ThrottleOptions {
            read_qps: NonZeroU32::new(args.throttle_read_qps),
            write_qps: NonZeroU32::new(args.throttle_write_qps),
            read_bytes: NonZeroUsize::new(args.throttle_read_bytes_s),
            write_bytes: NonZeroUsize::new(args.throttle_write_bytes_s),
            ..Default::default()
        },
        derived_data_scuba_dataset: args.derived_data_scuba_dataset.clone(),
    };
    let mut scuba = MononokeScubaSampleBuilder::new(app.fb, &args.backfill_scuba_dataset)
        .with_context(|| format!("Failed to open the dataset {}", args.backfill_scuba_dataset))?;
    scuba.add_common_server_data();
    scuba
        .add("task_index", args.task_index)
        .add("task_count", args.task_count)
        .add("window_size", args.window_size.get());

    // A repo whose walk fails does not hold up the others. The task fails at
    // the end, and once restarted, resumes each repo where it left off.
    let mut failed_repos = Vec::new();
    for repo_name in &args.repo_names {
        let mut scuba = scuba.clone();
        scuba.add("repo", repo_name.as_str());
        let result = task
            .backfill_repo(repo_name, bounds[repo_name], &scuba)
            .await;
        if let Err(e) = result {
            error!("The walk of {repo_name} failed: {e:#}");
            scuba
                .clone()
                .add("error", format!("{e:#}"))
                .log_with_msg(LOG_TAG_WALK_FAILED, None);
            failed_repos.push(repo_name.as_str());
        }
    }
    ensure!(
        failed_repos.is_empty(),
        "The walks of these repos failed: {}",
        failed_repos.join(", "),
    );
    Ok(())
}

impl Task<'_> {
    /// Walks this task's share of `repo_name` and logs each window the walk
    /// reports and each commit that could not be derived to `scuba`.
    async fn backfill_repo(
        &self,
        repo_name: &str,
        bounds: IdBounds,
        scuba: &MononokeScubaSampleBuilder,
    ) -> Result<()> {
        // Redaction is off, as in the derived data backfill worker, so that
        // derivation can read redacted files.
        let repo: Repo = self
            .app
            .open_repo_unredacted_with_factory_customization(
                &RepoArg::Name(repo_name.to_string()),
                |factory| {
                    factory.with_derived_data_scuba_table(self.derived_data_scuba_dataset.clone())
                },
            )
            .await
            .with_context(|| format!("Failed to open {repo_name}"))?;
        let replication = WaitForReplication::new(
            self.app.fb,
            self.app.config_store(),
            repo.repo_config().storage_config.clone(),
            REPLICATION_LAG_CONFIG,
        )
        .with_context(|| format!("Failed to set up the replication lag wait for {repo_name}"))?;
        let backfill = RepoBackfill::new(
            repo.repo_derived_data().manager().clone(),
            self.throttle,
            Arc::new(replication),
        );
        let walk = RepoWalk::new(self.shard, self.window_size, bounds);

        info!("Walking {repo_name}");
        let mut totals = WindowCounts::default();
        let mut reports = pin!(walk.walk(&self.ctx, &repo, &backfill));
        while let Some(report) = reports.try_next().await? {
            let counts = log_window(scuba, &report);
            info!(
                "{repo_name}: finished the ids {:?}: {counts:?}",
                report.window
            );
            totals.add(&counts);
        }
        info!("{repo_name}: finished this task's share: {totals:?}");
        Ok(())
    }
}

/// How many commits a window checked, and what was done with them.
#[derive(Debug, Default, Eq, PartialEq)]
struct WindowCounts {
    checked: u64,
    already_derived: u64,
    derived: u64,
    failed: u64,
    /// The commits the derivations derived: those that were derived, and
    /// any of their ancestors that lacked augmented manifests.
    derived_including_ancestors: u64,
}

impl WindowCounts {
    /// Counts `outcomes`, and returns the failures to log.
    fn of(outcomes: &HashMap<ChangesetId, CommitOutcome>) -> (Self, Vec<(ChangesetId, &Failure)>) {
        let mut counts = Self::default();
        let mut failures = Vec::new();
        for (cs_id, outcome) in outcomes {
            match outcome {
                CommitOutcome::AlreadyDerived => counts.already_derived += 1,
                CommitOutcome::Derived { count } => {
                    counts.derived += 1;
                    counts.derived_including_ancestors += count;
                }
                CommitOutcome::Failed(failure) => {
                    counts.failed += 1;
                    failures.push((*cs_id, failure));
                }
            }
            counts.checked += 1;
        }
        (counts, failures)
    }

    fn add(&mut self, other: &Self) {
        self.checked += other.checked;
        self.already_derived += other.already_derived;
        self.derived += other.derived;
        self.failed += other.failed;
        self.derived_including_ancestors += other.derived_including_ancestors;
    }
}

/// Logs one row for each commit of `report` that was not derived, then one
/// row with the window's counts, and returns the counts.
///
/// Across restarts, a commit's failure can be logged more than once, and a
/// commit that one task derives as an ancestor can be counted again by the
/// task whose window lists it. A failure row stays when a later attempt
/// derives the commit, so only the existence check shows which commits still
/// lack augmented manifests.
fn log_window(scuba: &MononokeScubaSampleBuilder, report: &WindowReport) -> WindowCounts {
    let mut scuba = scuba.clone();
    scuba
        .add("window_start", report.window.start)
        .add("window_end", report.window.end);

    let (counts, failures) = WindowCounts::of(&report.outcomes);
    for (cs_id, failure) in failures {
        scuba
            .clone()
            .add("changeset_id", cs_id.to_string())
            .add("reason", failure_reason(failure))
            .add("error", format!("{failure:#}"))
            .log_with_msg(LOG_TAG_FAILURE, None);
    }

    scuba
        .add("checked", counts.checked)
        .add("already_derived", counts.already_derived)
        .add("derived", counts.derived)
        .add("failed", counts.failed)
        .add(
            "derived_including_ancestors",
            counts.derived_including_ancestors,
        )
        .log_with_msg(LOG_TAG_WINDOW_DONE, None);
    counts
}

fn failure_reason(failure: &Failure) -> &'static str {
    match failure {
        Failure::Error(_) => "error",
        Failure::ParentFailed { .. } => "parent_failed",
        Failure::Blocked { .. } => "blocked",
    }
}

/// Parses `<repo>=<id>`.
fn parse_repo_id(arg: &str) -> Result<(String, u64)> {
    let (repo, id) = arg
        .split_once('=')
        .with_context(|| format!("{arg} is not <repo>=<id>"))?;
    ensure!(!repo.is_empty(), "{arg} names no repo");
    let id = id
        .parse()
        .with_context(|| format!("{id} is not a commit-graph id"))?;
    Ok((repo.to_string(), id))
}

/// Each repo's start and end ids. No repo may be named twice, every id must
/// name one of `repos`, and no repo may have two start ids or two end ids.
fn repo_bounds(
    repos: &[String],
    start_ids: &[(String, u64)],
    end_ids: &[(String, u64)],
) -> Result<HashMap<String, IdBounds>> {
    let mut bounds = HashMap::new();
    for repo in repos {
        ensure!(
            bounds.insert(repo.clone(), IdBounds::default()).is_none(),
            "{repo} is named more than once",
        );
    }
    set_ids(&mut bounds, start_ids, "start", |bounds| {
        &mut bounds.start_id
    })?;
    set_ids(&mut bounds, end_ids, "end", |bounds| &mut bounds.end_id)?;
    Ok(bounds)
}

/// Sets the bound that `bound` picks to each repo's id in `ids`.
fn set_ids(
    bounds: &mut HashMap<String, IdBounds>,
    ids: &[(String, u64)],
    kind: &str,
    bound: impl Fn(&mut IdBounds) -> &mut Option<u64>,
) -> Result<()> {
    for (repo, id) in ids {
        let repo_bounds = bounds
            .get_mut(repo)
            .with_context(|| format!("There is a {kind} id for {repo}, which is not walked"))?;
        let bound = bound(repo_bounds);
        ensure!(bound.is_none(), "{repo} has more than one {kind} id");
        *bound = Some(*id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mononoke_macros::mononoke;

    use super::*;

    fn repos(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn ids(ids: &[(&str, u64)]) -> Vec<(String, u64)> {
        ids.iter()
            .map(|(repo, id)| (repo.to_string(), *id))
            .collect()
    }

    #[mononoke::test]
    fn parses_a_repo_and_an_id() -> Result<()> {
        assert_eq!(parse_repo_id("fbsource=42")?, ("fbsource".to_string(), 42));
        assert!(parse_repo_id("fbsource").is_err(), "no id");
        assert!(parse_repo_id("=42").is_err(), "no repo");
        assert!(parse_repo_id("fbsource=-1").is_err(), "a negative id");
        Ok(())
    }

    #[mononoke::test]
    fn gives_each_repo_its_own_start_and_end_ids() -> Result<()> {
        let bounds = repo_bounds(
            &repos(&["fbsource", "www", "aosp"]),
            &ids(&[("fbsource", 10), ("www", 20)]),
            &ids(&[("fbsource", 100)]),
        )?;

        assert_eq!(
            bounds,
            HashMap::from([
                (
                    "fbsource".to_string(),
                    IdBounds {
                        start_id: Some(10),
                        end_id: Some(100),
                    }
                ),
                (
                    "www".to_string(),
                    IdBounds {
                        start_id: Some(20),
                        end_id: None,
                    }
                ),
                ("aosp".to_string(), IdBounds::default()),
            ]),
            "repos without ids walk from their lowest id to their newest commit",
        );
        Ok(())
    }

    #[mononoke::test]
    fn rejects_ids_for_repos_not_walked_and_repeated_ids() {
        assert!(
            repo_bounds(&repos(&["fbsource"]), &ids(&[("fbsourse", 1)]), &[]).is_err(),
            "a start id for a repo that is not walked, such as a typo",
        );
        assert!(
            repo_bounds(&repos(&["fbsource"]), &[], &ids(&[("www", 1)])).is_err(),
            "an end id for a repo that is not walked",
        );
        assert!(
            repo_bounds(
                &repos(&["fbsource"]),
                &ids(&[("fbsource", 1), ("fbsource", 2)]),
                &[],
            )
            .is_err(),
            "two start ids for one repo",
        );
        assert!(
            repo_bounds(&repos(&["fbsource", "fbsource"]), &[], &[]).is_err(),
            "a repo named twice",
        );
    }

    fn cs_id(byte: u8) -> ChangesetId {
        ChangesetId::from_str(&format!("{byte:02x}").repeat(32))
            .expect("the test's changeset id should parse")
    }

    fn parent_failed(parent: u8) -> CommitOutcome {
        CommitOutcome::Failed(Failure::ParentFailed {
            parent: cs_id(parent),
        })
    }

    #[mononoke::test]
    fn logs_a_commit_blocked_in_the_repo_config_with_its_own_reason() {
        let blocked = Failure::Blocked {
            derived_data_type: "acl_manifests",
            commit: cs_id(1),
        };
        assert_eq!(failure_reason(&blocked), "blocked");
    }

    #[mononoke::test]
    fn counts_what_a_window_did_with_each_commit() {
        let outcomes = HashMap::from([
            (cs_id(1), CommitOutcome::AlreadyDerived),
            (cs_id(2), CommitOutcome::Derived { count: 3 }),
            (cs_id(3), CommitOutcome::Derived { count: 0 }),
            (cs_id(4), parent_failed(5)),
        ]);

        let (counts, failures) = WindowCounts::of(&outcomes);

        assert_eq!(
            counts,
            WindowCounts {
                checked: 4,
                already_derived: 1,
                derived: 2,
                failed: 1,
                // One derivation derived its commit and two ancestors, and
                // the other found its commit derived by then.
                derived_including_ancestors: 3,
            },
        );
        assert_eq!(
            failures.iter().map(|(cs_id, _)| *cs_id).collect::<Vec<_>>(),
            vec![cs_id(4)],
            "the failed commit should be logged",
        );
    }
}
