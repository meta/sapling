/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use anyhow::anyhow;
use blobstore::Loadable;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future::try_join;
use futures::stream;
use futures::stream::BoxStream;
use manifest::Diff;
use manifest::Entry;
use manifest::ManifestOps;
use manifest::ManifestOrderedOps;
use manifest::PathTree;
use mononoke_types::MPath;
use repo_blobstore::RepoBlobstoreRef;

use super::ChangedPath;
use super::ChangesetContext;
use crate::MononokeError;
use crate::MononokeRepo;

/// Use the same source replacements as commit_compare, without constructing
/// enforcing content contexts. The ordinary comparison remains a separate view.
pub(super) async fn subtree_relative_changes<R: MononokeRepo>(
    target: &ChangesetContext<R>,
    base: &ChangesetContext<R>,
    sources: Arc<PathTree<Option<(ChangesetContext<R>, MPath)>>>,
    after_path: Option<MPath>,
) -> Result<BoxStream<'static, Result<ChangedPath, MononokeError>>, MononokeError> {
    let replacements: HashMap<_, _> = stream::iter(
        sources
            .as_ref()
            .clone()
            .into_iter()
            .filter_map(|(destination, source)| {
                source.map(|(changeset, path)| (destination, changeset, path))
            }),
    )
    .map(|(destination, changeset, path)| async move {
        let entry = changeset
            .root_content_manifest_id()
            .await?
            .find_entry(
                changeset.ctx().clone(),
                changeset.repo_ctx().repo().repo_blobstore().clone(),
                path.clone(),
            )
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "subtree source {} does not contain {}",
                    changeset.id(),
                    path
                )
            })?;
        let entry = match entry {
            Entry::Tree(id) => {
                let manifest = id
                    .load(
                        changeset.ctx(),
                        changeset.repo_ctx().repo().repo_blobstore(),
                    )
                    .await?;
                let counts = manifest.subentries.rollup_data().descendant_counts;
                let weight = usize::try_from(counts.files_count + counts.dirs_count)
                    .context("subtree replacement weight exceeds usize")?;
                Entry::Tree((weight, id))
            }
            Entry::Leaf(file) => Entry::Leaf(file),
        };
        Ok::<_, MononokeError>((destination, entry))
    })
    .buffer_unordered(10)
    .try_collect()
    .await?;
    if replacements.is_empty() {
        return Ok(stream::empty().boxed());
    }
    let prefixes: Vec<_> = replacements.keys().cloned().collect();
    let output_prefixes = prefixes.clone();
    let (old_root, new_root) = try_join(
        base.root_content_manifest_id(),
        target.root_content_manifest_id(),
    )
    .await?;
    Ok(old_root
        .filtered_diff_ordered(
            target.ctx().clone(),
            base.repo_ctx().repo().repo_blobstore().clone(),
            new_root,
            target.repo_ctx().repo().repo_blobstore().clone(),
            after_path,
            move |diff| {
                let (path, target_exists) = match diff {
                    Diff::Added(path, Entry::Leaf(_)) => (path, true),
                    Diff::Removed(path, Entry::Leaf(_)) => (path, false),
                    Diff::Changed(path, Entry::Leaf(_), Entry::Leaf(_)) => (path, true),
                    _ => return None,
                };
                output_prefixes
                    .iter()
                    .any(|prefix| prefix.is_prefix_of(&path))
                    .then_some(ChangedPath::SubtreeRelative {
                        path,
                        target_exists,
                    })
            },
            move |diff| {
                let path = match diff {
                    Diff::Added(path, _) | Diff::Removed(path, _) | Diff::Changed(path, _, _) => {
                        path
                    }
                };
                prefixes.iter().any(|prefix| path.is_related_to(prefix))
            },
            replacements,
        )
        .map_err(MononokeError::from)
        .boxed())
}
