/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use context::CoreContext;
use futures::channel::mpsc;
use futures::sink::SinkExt;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures_retry::retry;
use mononoke_app::MononokeApp;
use mononoke_macros::mononoke;
use sqlblob::Sqlblob;
use tracing::info;

use crate::MononokeSQLBlobGCArgs;
use crate::utils;

const BASE_RETRY_DELAY: Duration = Duration::from_secs(1);
const RETRIES: usize = 3;

/// mark referenced blobs as not safe to delete
#[derive(Parser)]
pub struct CommandArgs {
    /// Only set generation on blobs that have no generation set yet. Do not do a full mark.
    #[clap(long)]
    initial_generation_only: bool,
    /// Only do the mark; do not set generation on blobs with no generation set yet.
    #[clap(long)]
    skip_initial_generation: bool,
    /// Only set the generation, don't inline small values
    #[clap(long)]
    skip_inline_small_values: bool,
    /// Refuse to start, before touching any shard, unless the configured mark generation is this one
    #[clap(long)]
    expected_mark_generation: Option<u64>,
}

/// A marker that runs with a stale mark generation stamps live chunks too low, and a later erase
/// deletes them.
fn check_mark_generation(configured: u64, expected: Option<u64>) -> Result<()> {
    match expected {
        Some(expected) if expected != configured => Err(anyhow!(
            "Refusing to mark: the configured mark generation is {configured}, expected {expected}"
        )),
        _ => Ok(()),
    }
}

async fn handle_one_key(
    ctx: CoreContext,
    key: String,
    store: Arc<Sqlblob>,
    inline_small_values: bool,
    mark_generation: u64,
) -> Result<()> {
    retry(
        |_| store.set_generation(&ctx, &key, inline_small_values, mark_generation),
        BASE_RETRY_DELAY,
    )
    .binary_exponential_backoff()
    .max_attempts(RETRIES)
    .inspect_err(|attempt, _err| info!("attempt {attempt} of {RETRIES} failed"))
    .await
    .with_context(|| anyhow!("Failed to handle {key} after {RETRIES} attempts"))?;
    Ok(())
}

async fn handle_initial_generation(ctx: &CoreContext, store: &Sqlblob, shard: usize) -> Result<()> {
    retry(
        |_| store.set_initial_generation(ctx, shard),
        BASE_RETRY_DELAY,
    )
    .binary_exponential_backoff()
    .max_attempts(RETRIES)
    .inspect_err(|attempt, _err| info!("attempt {attempt} of {RETRIES} failed"))
    .await
    .with_context(|| {
        anyhow!("Failed to handle initial generation on shard {shard} after {RETRIES} retries",)
    })?;
    info!("Completed initial generation handling on shard {}", shard);
    Ok(())
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let common_args: MononokeSQLBlobGCArgs = app.args()?;
    let ctx = app.new_basic_context();

    let max_parallelism: usize = common_args.scheduled_max;

    let (sqlblob, shard_range) = utils::get_sqlblob_and_shard_range(&app).await?;

    check_mark_generation(sqlblob.get_mark_generation(), args.expected_mark_generation)?;

    if !args.skip_initial_generation {
        info!("Starting initial generation set");
        let set_initial_generation_futures: Vec<_> = shard_range
            .clone()
            .map(|shard| Ok(handle_initial_generation(&ctx, &sqlblob, shard)))
            .collect();
        stream::iter(set_initial_generation_futures)
            .try_for_each_concurrent(max_parallelism, |fut| fut)
            .await?;
        info!("Completed initial generation set");
    }

    if args.initial_generation_only {
        return Ok(());
    }

    let sqlblob = Arc::new(sqlblob);

    let inline_small_values = !args.skip_inline_small_values;

    // Hold mark generation constant for run
    let mark_generation = sqlblob.get_mark_generation();
    check_mark_generation(mark_generation, args.expected_mark_generation)?;

    info!("Starting marking generation {}", mark_generation);
    // Set up a task to process each key in parallel in its own task.
    let (key_channel, processor) = {
        let sqlblob = Arc::clone(&sqlblob);
        let (tx, rx) = mpsc::channel(10);
        let new_ctx = ctx.clone();
        let task = mononoke::spawn_task(async move {
            rx.map(Ok)
                .try_for_each_concurrent(max_parallelism, {
                    |key| {
                        let sqlblob = sqlblob.clone();
                        let new_ctx = new_ctx.clone();
                        async move {
                            mononoke::spawn_task(handle_one_key(
                                new_ctx,
                                key,
                                sqlblob,
                                inline_small_values,
                                mark_generation,
                            ))
                            .await?
                        }
                    }
                })
                .await
        });
        (tx, task)
    };

    // Foreach shard in shard_range
    for shard in shard_range {
        info!("Starting mark on data keys from shard {}", shard);
        let res = sqlblob
            .get_keys_from_shard(&ctx, shard)
            .forward(key_channel.clone().sink_err_into())
            .await;
        // Report processing errors ahead of key errors - that way, we don't lose the error if the channel goes away because of an error
        if res.is_err() {
            std::mem::drop(key_channel);
            processor.await??;
            return res;
        }
    }

    // Drop the spare sender so that the processor task can exit
    std::mem::drop(key_channel);

    processor.await??;
    info!("Completed marking generation {}", mark_generation);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[mononoke::test]
    fn marks_without_an_expected_generation() {
        assert!(check_mark_generation(199, None).is_ok());
    }

    #[mononoke::test]
    fn marks_at_the_expected_generation() {
        assert!(check_mark_generation(201, Some(201)).is_ok());
    }

    #[mononoke::test]
    fn refuses_another_generation() {
        let err = check_mark_generation(199, Some(201))
            .expect_err("a marker that still sees 199 must not mark when 201 is expected");
        assert_eq!(
            err.to_string(),
            "Refusing to mark: the configured mark generation is 199, expected 201"
        );
    }
}
