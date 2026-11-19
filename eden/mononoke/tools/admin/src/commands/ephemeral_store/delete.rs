/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Args;
use context::CoreContext;
use ephemeral_blobstore::BubbleId;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use itertools::Itertools;
use mononoke_types::ChangesetId;
use serde::Deserialize;

use super::Repo;
use super::cleanup::delete_bubbles_and_report;

#[derive(Args)]
/// Subcommand to delete bubbles, and the blobs and metadata stored in them,
/// regardless of whether they have expired or have labels.
pub struct EphemeralStoreDeleteArgs {
    /// The ID of a bubble to delete. Can be repeated.
    #[clap(long, short = 'b')]
    bubble_id: Vec<BubbleId>,

    /// The ID of a changeset whose bubbles should be deleted: every bubble
    /// containing it, and every other changeset in those bubbles. Can be
    /// repeated.
    #[clap(long, short = 'i')]
    changeset_id: Vec<ChangesetId>,

    /// A JSON file listing bubbles to delete, in addition to the ones given
    /// on the command line. For example:
    /// {"bubble_ids": [1, 2], "changeset_ids": ["<64 hex digits>"]}
    /// Both lists are optional.
    #[clap(long)]
    ids_file: Option<PathBuf>,

    /// When set, the command won't actually delete the bubbles but instead
    /// just lists the bubbles that a run of this command without `--dry-run` will delete.
    #[clap(long, short = 'n')]
    dry_run: bool,

    /// The concurrency with which the bubbles will be looked up and deleted.
    /// Defaults to 100.
    #[clap(long, short = 'j', default_value = "100")]
    concurrency: NonZeroUsize,
}

/// The contents of the JSON file passed as `--ids-file`.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdsFile {
    #[serde(default)]
    bubble_ids: Vec<BubbleId>,
    #[serde(default)]
    changeset_ids: Vec<ChangesetId>,
}

/// Deletes the bubbles named by `args` (see `EphemeralStoreDeleteArgs`) and
/// everything in them, whether or not they have expired.
///
/// Nothing is deleted unless every requested changeset has a bubble and every
/// selected bubble exists. The changesets of each bubble are printed first, and
/// those that were not requested are called out, because they are deleted along
/// with the bubble. Meant for bubbles nobody is writing to: a client that still
/// has a bubble open can write a blob after the bubble was listed and leave it
/// behind.
pub async fn delete_bubbles(
    ctx: &CoreContext,
    repo: &Repo,
    args: EphemeralStoreDeleteArgs,
) -> Result<()> {
    let concurrency = args.concurrency.get();
    let ids_file = match &args.ids_file {
        Some(path) => {
            let contents = tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("Failed to read {}", path.display()))?;
            serde_json::from_str(&contents)
                .with_context(|| format!("Failed to parse {} as an IDs file", path.display()))?
        }
        None => IdsFile::default(),
    };
    let named_bubble_ids: BTreeSet<BubbleId> = args
        .bubble_id
        .into_iter()
        .chain(ids_file.bubble_ids)
        .collect();
    let requested_changeset_ids: BTreeSet<ChangesetId> = args
        .changeset_id
        .into_iter()
        .chain(ids_file.changeset_ids)
        .collect();
    if named_bubble_ids.is_empty() && requested_changeset_ids.is_empty() {
        bail!("Need to provide bubble IDs, changeset IDs or an IDs file");
    }

    let bubbles_of_changesets = stream::iter(requested_changeset_ids.iter().copied())
        .map(|cs_id| async move {
            let bubble_ids = repo
                .repo_ephemeral_store
                .bubbles_from_changeset(ctx, &cs_id)
                .await?;
            anyhow::Ok((cs_id, bubble_ids))
        })
        .buffer_unordered(concurrency)
        .try_collect::<Vec<_>>()
        .await?;
    let (without_bubble, with_bubbles): (Vec<_>, Vec<_>) = bubbles_of_changesets
        .into_iter()
        .partition(|(_, bubble_ids)| bubble_ids.is_empty());
    if !without_bubble.is_empty() {
        let cs_ids = without_bubble.iter().map(|(cs_id, _)| cs_id).join(", ");
        bail!("No bubble exists for changeset IDs: {cs_ids}. Nothing was deleted");
    }
    let bubble_ids: BTreeSet<BubbleId> = named_bubble_ids
        .iter()
        .copied()
        .chain(
            with_bubbles
                .into_iter()
                .flat_map(|(_, bubble_ids)| bubble_ids),
        )
        .collect();

    let changesets_of_bubbles = stream::iter(bubble_ids)
        .map(|bubble_id| async move {
            let cs_ids = async {
                repo.repo_ephemeral_store
                    .open_bubble_raw(ctx, bubble_id)
                    .await?;
                repo.repo_ephemeral_store
                    .changesets_from_bubble(ctx, &bubble_id)
                    .await
            }
            .await;
            (bubble_id, cs_ids)
        })
        .buffered(concurrency)
        .collect::<Vec<_>>()
        .await;
    let unreadable = changesets_of_bubbles
        .iter()
        .filter_map(|(bubble_id, cs_ids)| {
            cs_ids
                .as_ref()
                .err()
                .map(|err| format!("  Bubble {bubble_id}: {err:#}"))
        })
        .join("\n");
    if !unreadable.is_empty() {
        bail!("Cannot delete bubbles that cannot be read. Nothing was deleted:\n{unreadable}");
    }
    let changesets_of_bubbles = changesets_of_bubbles
        .into_iter()
        .map(|(bubble_id, cs_ids)| Ok((bubble_id, cs_ids?)))
        .collect::<Result<Vec<_>>>()?;

    changesets_of_bubbles.iter().for_each(|(bubble_id, cs_ids)| {
        println!(
            "Bubble {bubble_id} contains changesets: [{}]",
            cs_ids.iter().join(", ")
        );
        if named_bubble_ids.contains(bubble_id) {
            return;
        }
        let not_requested = cs_ids
            .iter()
            .filter(|cs_id| !requested_changeset_ids.contains(*cs_id))
            .join(", ");
        if !not_requested.is_empty() {
            println!(
                "Bubble {bubble_id} will also delete changesets that were not requested: [{not_requested}]"
            );
        }
    });
    if args.dry_run {
        println!("Executing delete in dry-run mode. None of the bubbles above were deleted.");
        return Ok(());
    }

    let bubble_ids = changesets_of_bubbles
        .into_iter()
        .map(|(bubble_id, _)| bubble_id)
        .collect();
    delete_bubbles_and_report(ctx, repo, bubble_ids, concurrency).await
}
