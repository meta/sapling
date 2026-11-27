/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::future::Future;
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
    /// Time limit, in seconds, for one try at marking one key. A try that takes longer fails and
    /// is retried like any other failed try. 0 turns the limit off.
    #[clap(long, default_value_t = 300)]
    key_timeout_secs: u64,
}

fn key_timeout(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
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

/// Without a limit, a request that never returns holds its slot, and so the whole run, until the
/// MySQL client gives up on it.
async fn within_time_limit<T>(
    limit: Option<Duration>,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    let Some(limit) = limit else {
        return fut.await;
    };
    tokio::time::timeout(limit, fut)
        .await
        .map_err(|_| anyhow!("timed out after {} seconds", limit.as_secs()))?
}

async fn mark_key<F, Fut>(key: &str, timeout: Option<Duration>, mut mark: F) -> Result<()>
where
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = Result<()>>,
{
    retry(|_| within_time_limit(timeout, mark()), BASE_RETRY_DELAY)
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
    let timeout = key_timeout(args.key_timeout_secs);

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
                    |key: String| {
                        let sqlblob = sqlblob.clone();
                        let new_ctx = new_ctx.clone();
                        async move {
                            mononoke::spawn_task(async move {
                                mark_key(&key, timeout, || {
                                    sqlblob.set_generation(
                                        &new_ctx,
                                        &key,
                                        inline_small_values,
                                        mark_generation,
                                    )
                                })
                                .await
                            })
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
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use futures::future;

    use super::*;

    const FIVE_MINUTES: Duration = Duration::from_secs(300);
    const ONE_HOUR: Duration = Duration::from_secs(3600);

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

    #[mononoke::test]
    fn key_timeout_defaults_to_five_minutes() {
        let args =
            CommandArgs::try_parse_from(["mark"]).expect("mark should parse with no options");
        assert_eq!(key_timeout(args.key_timeout_secs), Some(FIVE_MINUTES));
    }

    #[mononoke::test]
    fn zero_turns_the_key_timeout_off() {
        let args = CommandArgs::try_parse_from(["mark", "--key-timeout-secs", "0"])
            .expect("a zero key timeout should parse");
        assert_eq!(key_timeout(args.key_timeout_secs), None);
    }

    #[mononoke::test]
    async fn a_try_that_never_returns_is_retried() {
        tokio::time::pause();
        let tries = AtomicUsize::new(0);
        let marking = mark_key("key", Some(FIVE_MINUTES), || {
            let this_try = tries.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if this_try == 1 {
                    future::pending::<()>().await;
                }
                Ok(())
            }
        });

        tokio::time::timeout(ONE_HOUR, marking)
            .await
            .expect("the hung first try should time out, so marking should end within an hour")
            .expect("the second try should mark the key");
        assert_eq!(
            tries.load(Ordering::SeqCst),
            2,
            "one hung try, then one that succeeds"
        );
    }

    #[mononoke::test]
    async fn a_key_whose_every_try_hangs_fails() {
        tokio::time::pause();
        let tries = AtomicUsize::new(0);
        let marking = mark_key("key", Some(FIVE_MINUTES), || {
            tries.fetch_add(1, Ordering::SeqCst);
            future::pending::<Result<()>>()
        });

        let err = tokio::time::timeout(ONE_HOUR, marking)
            .await
            .expect("every try should time out, so marking should end within an hour")
            .expect_err("a key whose every try hangs should fail");
        assert_eq!(
            format!("{err:#}"),
            "Failed to handle key after 3 attempts: timed out after 300 seconds"
        );
        assert_eq!(tries.load(Ordering::SeqCst), RETRIES);
    }

    #[mononoke::test]
    async fn without_a_limit_a_slow_try_is_waited_for() {
        tokio::time::pause();
        let tries = AtomicUsize::new(0);
        mark_key("key", None, || {
            tries.fetch_add(1, Ordering::SeqCst);
            async {
                tokio::time::sleep(Duration::from_secs(2 * 3600)).await;
                Ok(())
            }
        })
        .await
        .expect("with the limit off, a try that takes two hours should succeed");
        assert_eq!(
            tries.load(Ordering::SeqCst),
            1,
            "the slow try is not cut off"
        );
    }
}
