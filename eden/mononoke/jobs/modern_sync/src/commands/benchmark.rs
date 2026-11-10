/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use clap::ValueEnum;
use cloned::cloned;
use context::CoreContext;
use mononoke_app::MononokeApp;
#[cfg(fbcode_build)]
use mononoke_app::args::MonitoringArgs;
use mononoke_macros::mononoke;
use mutable_counters::MutableCounters;
use mutable_counters::validate_counter_name;

#[cfg(fbcode_build)]
mod stats;

use crate::sender::edenapi::EdenapiSender;
use crate::sender::edenapi::FilterEdenapiSender;
use crate::sender::edenapi::MethodFilter;
use crate::sender::edenapi::NoopEdenapiSender;
use crate::sync::ExecutionType;
use crate::sync::get_unsharded_repo_args;

#[derive(ValueEnum, Default, Clone)]
enum BenchmarkMode {
    #[default]
    Noop,
    UploadContents,
}

/// Replays bookmark's moves
#[derive(Parser)]
pub struct CommandArgs {
    #[clap(long, help = "The name of the benchmark, used for logging and stats")]
    name: Option<String>,

    #[clap(long, default_value_t, value_enum)]
    mode: BenchmarkMode,

    #[clap(long, help = "Chunk size for the sync [default: 1000]")]
    chunk_size: Option<u64>,

    #[clap(
        long,
        help = "How long to run the benchmark for, in seconds [default: unlimited]"
    )]
    duration: Option<u64>,

    #[clap(
        long,
        default_value = "60",
        help = "How often to report stats, in seconds"
    )]
    stat_interval: u64,

    #[clap(flatten, next_help_heading = "SYNC OPTIONS")]
    sync_args: crate::sync::SyncArgs,
}

#[derive(Clone, Default)]
struct MemoryMutableCounters {
    counters: Arc<std::sync::RwLock<std::collections::HashMap<String, i64>>>,
}

impl MemoryMutableCounters {
    pub fn new() -> Self {
        Self {
            counters: Arc::new(std::sync::RwLock::new(Default::default())),
        }
    }
}

#[async_trait]
impl MutableCounters for MemoryMutableCounters {
    async fn get_counter(&self, _ctx: &CoreContext, name: &str) -> Result<Option<i64>> {
        Ok(self.counters.read().unwrap().get(name).cloned())
    }

    async fn get_maybe_stale_counter(&self, ctx: &CoreContext, name: &str) -> Result<Option<i64>> {
        self.get_counter(ctx, name).await
    }

    async fn set_counter(
        &self,
        _ctx: &CoreContext,
        name: &str,
        value: i64,
        _prev_value: Option<i64>,
    ) -> Result<bool> {
        validate_counter_name(name)?;
        self.counters
            .write()
            .unwrap()
            .insert(name.to_string(), value);
        Ok(true)
    }

    async fn set_counters(&self, _ctx: &CoreContext, counters: &[(&str, i64)]) -> Result<()> {
        counters
            .iter()
            .try_for_each(|(name, _)| validate_counter_name(name))?;
        self.counters.write().unwrap().extend(
            counters
                .iter()
                .map(|(name, value)| ((*name).to_owned(), *value)),
        );
        Ok(())
    }

    async fn set_counter_if_absent(
        &self,
        _ctx: &CoreContext,
        name: &str,
        value: i64,
    ) -> Result<bool> {
        validate_counter_name(name)?;
        let mut counters = self.counters.write().unwrap();
        if counters.contains_key(name) {
            Ok(false)
        } else {
            counters.insert(name.to_string(), value);
            Ok(true)
        }
    }

    async fn get_all_counters(&self, _ctx: &CoreContext) -> Result<Vec<(String, i64)>> {
        Ok(self
            .counters
            .read()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect())
    }
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let app = Arc::new(app);
    let (source_repo_args, source_repo_name, dest_repo_name) =
        get_unsharded_repo_args(app.clone(), &args.sync_args).await?;
    let ctx = crate::sync::build_context(app.clone(), &source_repo_name, false);

    let benchmark_mode = args.mode;
    let mc = MemoryMutableCounters::new();

    #[cfg(fbcode_build)]
    let stats = {
        let port = app.args::<MonitoringArgs>()?.fb303_thrift_port.unwrap() as u16;
        let stat_interval = Duration::from_secs(args.stat_interval);
        let stats = Arc::new(
            stats::StatsBuilder::new(
                app.fb.clone(),
                args.name.clone(),
                source_repo_name.clone(),
                port,
                stat_interval,
            )
            .build()
            .await?,
        );
        stats.run();
        stats
    };

    let now = std::time::Instant::now();
    let cancellation_requested = Arc::new(AtomicBool::new(false));

    if let Some(duration) = args.duration {
        mononoke::spawn_task({
            cloned!(cancellation_requested);
            async move {
                let mut interval = tokio::time::interval(Duration::from_secs(duration));
                interval.tick().await; // the first tick is instant
                interval.tick().await;
                cancellation_requested.store(true, Ordering::Relaxed);
            }
        });
    }

    crate::sync::sync(
        app,
        Some(0),
        source_repo_args,
        dest_repo_name.clone(),
        ExecutionType::SyncOnce,
        false,
        PathBuf::from(""),
        Some(Box::new(move |sender| {
            let sender: Arc<dyn EdenapiSender + Sync + Send> = match benchmark_mode {
                BenchmarkMode::Noop => Arc::new(NoopEdenapiSender::default()),
                BenchmarkMode::UploadContents => {
                    let allowed = HashMap::from([(MethodFilter::UploadContents, true)]);
                    Arc::new(FilterEdenapiSender::new(sender, allowed))
                }
            };
            sender
        })),
        Some(Arc::new(mc.clone())),
        cancellation_requested,
    )
    .await?;
    let elapsed = now.elapsed();

    #[cfg(fbcode_build)]
    stats.finish().await;

    tracing::info!(
        "Benchmark: Sync {} to {:?} took {}ms",
        elapsed.as_millis(),
        &source_repo_name,
        dest_repo_name,
    );

    tracing::info!("Counters:");
    let mut counters = mc.get_all_counters(&ctx).await?;
    counters.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in counters {
        tracing::info!("{}={}", k, v);
    }

    Ok(())
}
