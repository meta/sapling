/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Samples configured per-repo values (mutable counters, bookmark-update-log
//! tips) and emits them as ODS gauges, plus a heartbeat.
//!
//! The selection comes from a central Configerator file (see
//! `scm/mononoke/repo_state_logger`), re-read every iteration so config
//! edits apply without a restart. Repos are selected by ID; emitted series use
//! the repo's canonical name. `--source` offers the same selection as CLI args
//! for tests and manual runs.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLog;
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::Freshness;
use clap::Parser;
use cmdlib_caching::CacheMode;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::TryStreamExt;
use futures::stream::StreamExt;
use futures::stream::{self};
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::args::SourceRepoArgs;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use mononoke_types::RepositoryId;
use mutable_counters::MutableCounters;
use mutable_counters::MutableCountersRef;
use repo_identity::RepoIdentity;
use repo_identity::RepoIdentityRef;
use repo_state_logger::RepoStateLoggerConfig;
use repo_state_logger::ValueSource;
use stats::define_stats;
use stats::prelude::*;

/// Floor for the sampling interval: never poll faster than once a minute,
// no matter what the config says.
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Max concurrent storage reads while sampling or opening repos. Each read is
/// a cheap indexed lookup, so this bound only bites at hundreds of sources —
/// growing the selection (e.g. per-repo values across the fleet) never needs
/// a code change.
const MAX_CONCURRENT_REQUESTS: usize = 16;

define_stats! {
    prefix = "mononoke.repo_state_logger";
    sampled_value: dynamic_timeseries("{}", (key: String); Average),
    heartbeat: timeseries(Average),
    sample_errors: counter(),
    missing_values: counter(),
}

#[facet::container]
#[derive(Clone)]
pub struct Repo {
    #[facet]
    repo_identity: RepoIdentity,
    #[facet]
    mutable_counters: dyn MutableCounters,
    #[facet]
    bookmark_update_log: dyn BookmarkUpdateLog,
}

#[derive(Parser)]
struct LoggerArgs {
    /// Configerator path of the selection config,
    /// e.g. scm/mononoke/repo_state_logger
    #[clap(long)]
    selection_config: Option<String>,
    /// Source specs `repo:counter:<name>` or `repo:bookmark_tip:<name>`, where
    /// `repo` is a repo name or a numeric repo ID.
    /// Used only when --selection-config is absent.
    #[clap(long)]
    source: Vec<String>,
    /// Sample once and exit.
    #[clap(long)]
    once: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Counter { name: String },
    BookmarkTip { name: String },
}

/// How a repo is selected: by stable ID (config mode) or by name (CLI only).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RepoSelector {
    Id(RepositoryId),
    Name(String),
}

impl fmt::Display for RepoSelector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            RepoSelector::Id(id) => write!(f, "{id}"),
            RepoSelector::Name(name) => write!(f, "{name}"),
        }
    }
}

#[derive(Debug, Clone)]
struct SampledRepo {
    repo: RepoSelector,
    sources: Vec<Source>,
}

fn parse_source_spec(spec: &str) -> Result<(RepoSelector, Source)> {
    let parts: Vec<&str> = spec.split(':').collect();
    let [repo, kind, name] = parts.as_slice() else {
        return Err(anyhow!(
            "invalid source spec {spec:?}: want repo:counter:<name> or repo:bookmark_tip:<name>"
        ));
    };
    if repo.is_empty() || name.is_empty() {
        return Err(anyhow!(
            "invalid source spec {spec:?}: repo and name must be non-empty"
        ));
    }
    // A numeric repo segment selects by ID (the config-mode form); anything
    // else selects by name.
    let repo = match repo.parse::<RepositoryId>() {
        Ok(id) => RepoSelector::Id(id),
        Err(_) => RepoSelector::Name(repo.to_string()),
    };
    let source = match *kind {
        "counter" => Source::Counter {
            name: name.to_string(),
        },
        "bookmark_tip" => Source::BookmarkTip {
            name: name.to_string(),
        },
        _ => {
            return Err(anyhow!(
                "invalid source spec {spec:?}: unknown kind {kind:?}"
            ));
        }
    };
    Ok((repo, source))
}

/// Stats key segment for one source. Segments keep alphanumerics and
/// underscores only, so repo names like `aosp/foo` cannot break the key.
fn stats_key(repo: &str, source: &Source) -> String {
    let (kind, name) = match source {
        Source::Counter { name } => ("counter", name),
        Source::BookmarkTip { name } => ("bookmark_tip", name),
    };
    let repo = sanitize_segment(repo);
    let name = sanitize_segment(name);
    format!("{repo}.{kind}_{name}")
}

fn sanitize_segment(segment: &str) -> String {
    segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn clamp_poll_interval(config: &RepoStateLoggerConfig) -> Duration {
    let secs = u64::try_from(config.poll_interval_secs).unwrap_or(0);
    Duration::from_secs(secs).max(MIN_POLL_INTERVAL)
}

fn selection_from_config(config: &RepoStateLoggerConfig) -> Vec<SampledRepo> {
    config
        .repos
        .iter()
        .filter_map(|(repo, selection)| {
            // Config keys are i64 to survive the repo-ID format change;
            // RepositoryId is still i32, so reject out-of-range keys.
            let Ok(id) = i32::try_from(*repo) else {
                tracing::warn!(
                    repo_id = *repo,
                    "skipping repo ID outside i32 range (newer format?)"
                );
                STATS::sample_errors.increment_value(1);
                return None;
            };
            let sources = selection
                .sources
                .iter()
                .filter_map(|source| match source {
                    ValueSource::counter(counter) => Some(Source::Counter {
                        name: counter.counter_name.clone(),
                    }),
                    ValueSource::bookmark_tip(tip) => Some(Source::BookmarkTip {
                        name: tip.bookmark.clone(),
                    }),
                    _ => {
                        tracing::warn!(
                            repo_id = *repo,
                            "skipping unsupported value source variant (newer config?)"
                        );
                        STATS::sample_errors.increment_value(1);
                        None
                    }
                })
                .collect();
            Some(SampledRepo {
                repo: RepoSelector::Id(RepositoryId::new(id)),
                sources,
            })
        })
        .collect()
}

fn selection_from_sources(specs: &[String]) -> Result<Vec<SampledRepo>> {
    let mut by_repo: HashMap<RepoSelector, Vec<Source>> = HashMap::new();
    for spec in specs {
        let (repo, source) = parse_source_spec(spec)?;
        by_repo.entry(repo).or_default().push(source);
    }
    Ok(by_repo
        .into_iter()
        .map(|(repo, sources)| SampledRepo { repo, sources })
        .collect())
}

async fn sample_counter(ctx: &CoreContext, repo: &Repo, counter_name: &str) -> Result<Option<i64>> {
    repo.mutable_counters()
        .get_counter(ctx, counter_name)
        .await
        .with_context(|| format!("reading counter {counter_name:?}"))
}

async fn sample_bookmark_tip(
    ctx: &CoreContext,
    repo: &Repo,
    bookmark: &str,
) -> Result<Option<i64>> {
    let key =
        BookmarkKey::new(bookmark).with_context(|| format!("invalid bookmark {bookmark:?}"))?;
    let mut entries = repo.bookmark_update_log().list_bookmark_log_entries(
        ctx.clone(),
        key,
        1,
        None,
        Freshness::MostRecent,
    );
    match entries
        .try_next()
        .await
        .with_context(|| format!("reading log tip of bookmark {bookmark:?}"))?
    {
        Some((id, _, _, _)) => Ok(Some(i64::try_from(id).unwrap_or(i64::MAX))),
        None => Ok(None),
    }
}

async fn sample_source(ctx: &CoreContext, repo: &Repo, source: &Source) -> Result<Option<i64>> {
    match source {
        Source::Counter { name } => sample_counter(ctx, repo, name).await,
        Source::BookmarkTip { name } => sample_bookmark_tip(ctx, repo, name).await,
    }
}

/// Samples every source once, concurrently. Per-source failures are logged and
/// counted, never fatal: one bad repo or counter must not silence the rest.
async fn sample_all(
    ctx: &CoreContext,
    repos: &HashMap<RepoSelector, Repo>,
    selection: &[SampledRepo],
) {
    // (canonical name, repo, source) triples for every selectable source.
    // Series carry the canonical name no matter how the repo was selected.
    let samples = selection.iter().filter_map(|sampled| {
        let repo = repos.get(&sampled.repo)?;
        let name = repo.repo_identity().name();
        Some(
            sampled
                .sources
                .iter()
                .map(move |source| (name, repo, source)),
        )
    });
    stream::iter(samples.flatten())
        .map(|(name, repo, source)| async move {
            let key = stats_key(name, source);
            match sample_source(ctx, repo, source).await {
                Ok(Some(value)) => {
                    tracing::info!(repo = name, key, value, "sampled");
                    STATS::sampled_value.add_value(value, (key,));
                }
                Ok(None) => {
                    tracing::info!(repo = name, key, "no value (absent counter or bookmark)");
                    STATS::missing_values.increment_value(1);
                }
                Err(e) => {
                    tracing::warn!(repo = name, key, "sample failed: {e:?}");
                    STATS::sample_errors.increment_value(1);
                }
            }
        })
        .buffer_unordered(MAX_CONCURRENT_REQUESTS)
        .for_each(|()| async {})
        .await;
    STATS::heartbeat.add_value(1);
}

async fn async_main(app: MononokeApp) -> Result<()> {
    let args: LoggerArgs = app.args()?;
    if args.selection_config.is_some() == !args.source.is_empty() {
        anyhow::bail!("pass exactly one of --selection-config or --source");
    }
    let ctx = CoreContext::new(app.fb);

    let config_handle = match &args.selection_config {
        Some(path) => Some(
            app.config_store()
                .get_config_handle::<RepoStateLoggerConfig>(path.clone())
                .with_context(|| format!("loading logger config {path:?}"))?,
        ),
        None => None,
    };
    // CLI mode uses the floor; config mode re-reads the period every sweep.
    let cli_selection = if config_handle.is_none() {
        Some(selection_from_sources(&args.source)?)
    } else {
        None
    };

    let mut repos: HashMap<RepoSelector, Repo> = HashMap::new();
    // --once samples a single sweep and exits before the first sleep.
    let iterations: Option<usize> = args.once.then_some(1);
    let mut done = 0;
    loop {
        // Config and period are re-read every sweep, so edits apply to the
        // next sweep and sleep without a restart.
        let (period, selection) = if let Some(handle) = &config_handle {
            let config = handle.get();
            (clamp_poll_interval(&config), selection_from_config(&config))
        } else {
            (MIN_POLL_INTERVAL, cli_selection.clone().unwrap_or_default())
        };

        let unopened: Vec<&SampledRepo> = selection
            .iter()
            .filter(|sampled| !repos.contains_key(&sampled.repo))
            .collect();
        let opened: Vec<(RepoSelector, Repo)> = stream::iter(unopened)
            .map(|sampled| async {
                let repo_args = match &sampled.repo {
                    RepoSelector::Id(id) => SourceRepoArgs::with_id(id.id()),
                    RepoSelector::Name(name) => SourceRepoArgs::with_name(name.clone()),
                };
                match app.open_repo::<Repo>(&repo_args).await {
                    Ok(repo) => Some((sampled.repo.clone(), repo)),
                    Err(e) => {
                        tracing::warn!(repo = %sampled.repo, "cannot open repo: {e:?}");
                        STATS::sample_errors.increment_value(1);
                        None
                    }
                }
            })
            .buffer_unordered(MAX_CONCURRENT_REQUESTS)
            .filter_map(|opened| async { opened })
            .collect()
            .await;
        for (selector, repo) in opened {
            repos.insert(selector, repo);
        }

        sample_all(&ctx, &repos, &selection).await;

        done += 1;
        if iterations == Some(done) {
            return Ok(());
        }

        // Sleep after sampling rather than ticking an interval: sweep starts
        // are always at least `period` apart, so a sweep can never run twice
        // in one window, no matter how long a sweep takes or when the period
        // changes. No interval object, no missed-tick semantics to reason about.
        tokio::time::sleep(period).await;
    }
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    // The sampler reads a handful of values a minute; skip cachelib entirely
    // rather than paying for (and initializing) the default gigabyte cache.
    // Pass --cache-mode explicitly to override.
    let app = MononokeAppBuilder::new(fb)
        .with_arg_defaults(CacheMode::Disabled)
        .with_app_extension(MonitoringAppExtension {})
        .build::<LoggerArgs>()?;
    app.run_with_monitoring_and_logging(async_main, "repo_state_logger", AliveService)
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;
    use repo_state_logger::LatestBookmarkUpdateLogId;
    use repo_state_logger::MutableCounterValue;
    use repo_state_logger::RepoSelection;

    use super::*;

    #[mononoke::test]
    fn parses_counter_spec() {
        let (repo, source) =
            parse_source_spec("opsfiles:counter:modern_sync").expect("valid counter spec parses");
        assert_eq!(repo, RepoSelector::Name("opsfiles".to_string()));
        assert_eq!(
            source,
            Source::Counter {
                name: "modern_sync".to_string()
            }
        );
    }

    #[mononoke::test]
    fn parses_bookmark_tip_spec() {
        let (repo, source) =
            parse_source_spec("fbsource:bookmark_tip:master").expect("valid bookmark spec parses");
        assert_eq!(repo, RepoSelector::Name("fbsource".to_string()));
        assert_eq!(
            source,
            Source::BookmarkTip {
                name: "master".to_string()
            }
        );
    }

    #[mononoke::test]
    fn parses_numeric_id_spec() {
        let (repo, source) =
            parse_source_spec("2100:counter:modern_sync").expect("numeric repo selects by ID");
        assert_eq!(repo, RepoSelector::Id(RepositoryId::new(2100)));
        assert_eq!(
            source,
            Source::Counter {
                name: "modern_sync".to_string()
            }
        );
    }

    #[mononoke::test]
    fn rejects_malformed_specs() {
        for spec in [
            "opsfiles:counter",
            "opsfiles:counter:a:b",
            "opsfiles:gauge:modern_sync",
            ":counter:modern_sync",
            "opsfiles:counter:",
            "just-a-string",
            "",
        ] {
            assert!(
                parse_source_spec(spec).is_err(),
                "spec {spec:?} must be rejected"
            );
        }
    }

    #[mononoke::test]
    fn keys_encode_repo_kind_and_name() {
        assert_eq!(
            stats_key(
                "opsfiles",
                &Source::Counter {
                    name: "modern_sync".to_string()
                }
            ),
            "opsfiles.counter_modern_sync"
        );
        assert_eq!(
            stats_key(
                "fbsource",
                &Source::BookmarkTip {
                    name: "master".to_string()
                }
            ),
            "fbsource.bookmark_tip_master"
        );
    }

    #[mononoke::test]
    fn keys_sanitize_special_characters() {
        assert_eq!(
            stats_key(
                "aosp/platform/vendor",
                &Source::Counter {
                    name: "weird.name:x".to_string()
                }
            ),
            "aosp_platform_vendor.counter_weird_name_x"
        );
    }

    #[mononoke::test]
    fn selection_from_config_keys_repos_by_id() {
        let config = RepoStateLoggerConfig {
            poll_interval_secs: 60,
            repos: [(
                2100,
                RepoSelection {
                    sources: vec![
                        ValueSource::counter(MutableCounterValue {
                            counter_name: "modern_sync".to_string(),
                            ..Default::default()
                        }),
                        ValueSource::bookmark_tip(LatestBookmarkUpdateLogId {
                            bookmark: "master".to_string(),
                            ..Default::default()
                        }),
                    ],
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let selection = selection_from_config(&config);
        assert_eq!(selection.len(), 1);
        assert_eq!(selection[0].repo, RepoSelector::Id(RepositoryId::new(2100)));
        assert_eq!(
            selection[0].sources,
            vec![
                Source::Counter {
                    name: "modern_sync".to_string()
                },
                Source::BookmarkTip {
                    name: "master".to_string()
                },
            ]
        );
    }

    #[mononoke::test]
    fn selection_from_config_skips_out_of_range_ids() {
        let config = RepoStateLoggerConfig {
            poll_interval_secs: 60,
            repos: [
                (
                    2100i64,
                    RepoSelection {
                        sources: vec![],
                        ..Default::default()
                    },
                ),
                (
                    i64::from(i32::MAX) + 1,
                    RepoSelection {
                        sources: vec![],
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let selection = selection_from_config(&config);
        assert_eq!(selection.len(), 1);
        assert_eq!(selection[0].repo, RepoSelector::Id(RepositoryId::new(2100)));
    }

    #[mononoke::test]
    fn poll_interval_clamps_to_floor() {
        let config = |secs| RepoStateLoggerConfig {
            poll_interval_secs: secs,
            repos: Default::default(),
            ..Default::default()
        };
        assert_eq!(clamp_poll_interval(&config(0)), MIN_POLL_INTERVAL);
        assert_eq!(clamp_poll_interval(&config(-5)), MIN_POLL_INTERVAL);
        assert_eq!(clamp_poll_interval(&config(30)), MIN_POLL_INTERVAL);
        assert_eq!(clamp_poll_interval(&config(300)), Duration::from_secs(300));
    }
}
