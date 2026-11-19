/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::bail;
use clap::Args;
use context::CoreContext;
use ephemeral_blobstore::BubbleId;
use ephemeral_blobstore::UnlinkPolicy;
use futures::StreamExt;
use mononoke_macros::mononoke;

use super::Repo;

// By default, at most 50 expired bubbles will be cleaned up in one go.
const DEFAULT_MAX_BUBBLES_FOR_CLEANUP: u32 = 50;
const DEFAULT_CUTOFF_FOR_CLEANUP: u32 = 24 * 60 * 60;

#[derive(Args)]
/// Subcommand to cleanup expired bubbles within the ephemeral store.
pub struct EphemeralStoreCleanUpArgs {
    /// Duration in seconds for which the bubbles should already
    /// be expired. Defaults to the number of seconds in a day.
    #[clap(long, short = 'c', default_value_t = DEFAULT_CUTOFF_FOR_CLEANUP)]
    cutoff: u32,

    /// The maximum number of bubbles that can be cleaned up in
    /// one run of the command.
    #[clap(long, short = 'l', default_value_t = DEFAULT_MAX_BUBBLES_FOR_CLEANUP)]
    limit: u32,

    /// When set, the command won't actually cleanup the bubbles but
    /// instead just lists the bubble IDs that will be cleaned-up on
    /// a non-dryrun of this command.
    #[clap(long, short = 'n')]
    dryrun: bool,

    /// The concurrency with which the bubbles will be cleaned up. Defaults to 100.
    #[clap(long, short = 'j', default_value_t = 100)]
    concurrency: usize,
}

pub async fn clean_bubbles(
    ctx: &CoreContext,
    repo: &Repo,
    args: EphemeralStoreCleanUpArgs,
) -> Result<()> {
    let cutoff_duration = Duration::from_secs(args.cutoff.into());
    let expired_bubbles = repo
        .repo_ephemeral_store
        .get_expired_bubbles(ctx, cutoff_duration, args.limit)
        .await?;
    if expired_bubbles.is_empty() {
        println!("No expired bubbles found for deletion based on input provided");
        return Ok(());
    } else {
        println!(
            "Fetched {} expired bubbles for deletion",
            expired_bubbles.len()
        );
    }
    if !args.dryrun {
        delete_bubbles_and_report(ctx, repo, expired_bubbles, args.concurrency).await?;
    } else {
        println!(
            "Executing cleanup in dry-run mode. The following bubbles were fetched for deletion:"
        );
        println!("{expired_bubbles:?}");
    }
    Ok(())
}

/// Deletes the given bubbles regardless of their expiry status, printing the
/// outcome of each one and a final summary. Fails if any deletion failed.
pub(super) async fn delete_bubbles_and_report(
    ctx: &CoreContext,
    repo: &Repo,
    bubbles: Vec<BubbleId>,
    concurrency: usize,
) -> Result<()> {
    let total = bubbles.len();
    let started = Instant::now();
    let deletions = repo
        .repo_ephemeral_store
        .delete_bubbles(ctx, bubbles, concurrency, UnlinkPolicy::default())
        .fold(
            (0, 0, 0),
            |(finished, already_deleted, failed), (id, result)| async move {
                match result {
                    Ok(outcome) => {
                        println!(
                            "Cleaned up bubble {id} and deleted {} blob keys contained in it",
                            outcome.keys_deleted
                        );
                        let already_deleted =
                            already_deleted + usize::from(outcome.keys_already_deleted > 0);
                        (finished + 1, already_deleted, failed)
                    }
                    Err(err) => {
                        eprintln!("Failed to clean up bubble {id}: {err:#}");
                        (finished, already_deleted, failed + 1)
                    }
                }
            },
        );
    let (finished, already_deleted, failed) = mononoke::spawn_task(deletions).await?;
    println!(
        "Deleted {finished} of {total} bubbles in {:.1?}, {already_deleted} of them had blob keys that were already deleted, {failed} failed",
        started.elapsed()
    );
    if failed > 0 {
        bail!("Failed to clean up {failed} of {total} bubbles");
    }
    Ok(())
}
