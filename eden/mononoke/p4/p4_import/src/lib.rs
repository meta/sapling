/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Imports a Perforce depot into a repo.

use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use blobstore::Loadable;
use bonsai_git_mapping::BonsaiGitMappingRef;
use bonsai_globalrev_mapping::BonsaiGlobalrevMappingRef;
use bonsai_p4_mapping::BonsaiP4Mapping;
use bonsai_p4_mapping::BonsaiP4MappingArc;
use bonsai_p4_mapping::BonsaiP4MappingEntry;
use bonsai_p4_mapping::BonsaiP4MappingRef;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkKind;
use bookmarks::BookmarkTransactionError;
use bookmarks::BookmarkTransactionHook;
use bookmarks::BookmarkUpdateReason;
use bookmarks::BookmarksRef;
use bookmarks::Freshness;
use bytes::Bytes;
use commit_graph::CommitGraphRef;
use commit_graph::CommitGraphWriterRef;
use content_manifest_derivation::RootContentManifestId;
use context::CoreContext;
use filestore::FilestoreConfigRef;
use filestore::StoreRequest;
use futures::FutureExt;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::channel::mpsc;
use futures::future;
use futures::future::Either;
use futures::stream;
use futures_stats::TimedTryFutureExt;
pub use import_common::BackfillDerivation;
use import_common::save_and_derive_batch;
use manifest::Entry;
use manifest::ManifestOps;
use metaconfig_types::RepoConfigRef;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::ContentManifestId;
use mononoke_types::ContentMetadataV2;
use mononoke_types::FileType;
use mononoke_types::NonRootMPath;
use mononoke_types::P4ChangelistId;
use p4_transform::CopySource;
use p4_types::ChangeSource;
use p4_types::Changelist;
use p4_types::Depot;
use p4_types::DepotType;
use p4_types::FileRevision;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use repo_update_logger::BookmarkInfo;
use repo_update_logger::BookmarkOperation;
use repo_update_logger::CommitInfo;
use repo_update_logger::log_bookmark_operation;
use repo_update_logger::log_new_commits;
use stats::prelude::*;
use thiserror::Error;

define_stats! {
    prefix = "mononoke.p4_import";
    failed_batches: dynamic_timeseries("failed_batches.{}", (stage: &'static str); Rate, Sum),
    content_reuse_collisions: timeseries(Rate, Sum),
    content_reuse_lookup_failures: timeseries(Rate, Sum),
    content_reuse_digest_mismatches: timeseries(Rate, Sum),
    unreadable_revisions: timeseries(Rate, Sum),
    unreadable_revisions_skipped_after_retries: timeseries(Rate, Sum),
}

mod content_reuse;
mod fetch_ahead;
mod unreadable;

pub use content_reuse::ContentReuse;
pub use content_reuse::ContentReuseReport;
pub use unreadable::SkipReason;
pub use unreadable::SkipUnreadable;
pub use unreadable::SkippedRevision;
pub use unreadable::UnreadableRevision;
pub use unreadable::UnreadableRevisions;

/// Stores a file revision's content in the repo's filestore. `size` is the size
/// Perforce reports for the revision: content of any other length is rejected.
pub async fn upload_content(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + FilestoreConfigRef),
    size: u64,
    content: impl Stream<Item = Result<Bytes>> + Send,
) -> Result<ContentMetadataV2> {
    filestore::store(
        repo.repo_blobstore(),
        *repo.filestore_config(),
        ctx,
        &StoreRequest::new(size),
        content,
    )
    .await
}

// Only bounds memory: Perforce's default filesys.maxsymlink is 1K, OS limits are 4-8K.
// Design: designs/content_and_validation.md.
const MAX_SYMLINK_SIZE: u64 = 64 * 1024;

/// Like `upload_content`, but stores a symlink as its bare target.
/// Design: designs/content_and_validation.md.
pub async fn upload_file_content(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + FilestoreConfigRef),
    file_type: FileType,
    size: u64,
    content: impl Stream<Item = Result<Bytes>> + Send,
) -> Result<ContentMetadataV2> {
    if file_type != FileType::Symlink {
        return upload_content(ctx, repo, size, content).await;
    }
    ensure!(
        size <= MAX_SYMLINK_SIZE,
        "symlink of {size} bytes is larger than we support (64 KiB)"
    );
    let printed = read_symlink(size, content).await?;
    let target = symlink_target(size, printed)?;
    upload_content(
        ctx,
        repo,
        target.len() as u64,
        stream::once(async { Ok(target) }),
    )
    .await
}

async fn read_symlink(size: u64, content: impl Stream<Item = Result<Bytes>>) -> Result<Vec<u8>> {
    let mut content = pin!(content);
    let mut printed = Vec::new();
    while let Some(chunk) = content.try_next().await? {
        ensure!(
            (printed.len() + chunk.len()) as u64 <= size,
            "symlink content is longer than Perforce reports ({size} bytes)"
        );
        printed.extend_from_slice(&chunk);
    }
    Ok(printed)
}

// p4 print ends a symlink's target with a newline, and Perforce's size counts it.
fn symlink_target(size: u64, mut printed: Vec<u8>) -> Result<Bytes> {
    ensure!(
        printed.len() as u64 == size,
        "symlink content is {} bytes but Perforce reports {size}",
        printed.len()
    );
    printed
        .pop_if(|last| *last == b'\n')
        .context("symlink content does not end with a newline")?;
    ensure!(
        !printed.ends_with(b"\n"),
        "symlink content ends with more than one newline"
    );
    Ok(Bytes::from(printed))
}

/// How to import: what to derive, how much runs at once, and where to count progress.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Derived for every imported changeset before the bookmark moves, among the types
    /// the repo enables. A narrowed list should keep `ContentManifests`: the next batch's
    /// copy-source check reads the base's content manifest, and would otherwise derive it
    /// there.
    pub backfill_derivation: BackfillDerivation,
    /// Per changelist, the files printed and stored and the copy sources checked at once.
    /// At least 1.
    pub file_concurrency: usize,
    /// Changelists fetched at once, while an earlier one is built. At least 1.
    pub changelist_lookahead: usize,
    /// How long a batch builds before it stops taking changelists. Checked between
    /// changelists: the one the build is waiting for or building when the time runs out is
    /// finished and is the batch's last, so a batch always has one, and the rest are left
    /// for the next import. A batch dropped before it commits, as a job restart does, loses
    /// about this much work plus one changelist at most.
    pub max_build_time: Duration,
    /// Incremented for each file stored and each changeset built, so a caller can tell that
    /// a long batch is moving. A failed batch's steps stay counted.
    pub progress: Arc<AtomicU64>,
    /// How many changelist numbers after the resume point `import_next`'s first listing
    /// covers (see `next_changes_in_windows`). `None` lists up to the server's latest in one
    /// listing (`next_changes`), which a server may answer no faster for a few changelists
    /// than for all of them.
    pub listing_window: Option<NonZeroU64>,
    /// Asks for a changelist's integration records while its contents are stored, not
    /// after: with many copies in a changelist, the records take long enough to matter.
    pub copy_sources_with_uploads: bool,
    /// Whether a copy whose content is already stored, found by MD5 and size, is printed.
    pub content_reuse: ContentReuse,
    /// Revisions imported as deletions if the server cannot read their archive. Any other
    /// revision the server cannot read fails its changelist, unless `skip_unreadable` is set.
    pub unreadable_revisions: Arc<HashSet<UnreadableRevision>>,
    /// Whether any other revision the server cannot read is imported as a deletion too, once
    /// its retries have failed. `None` fails its changelist.
    pub skip_unreadable: Option<SkipUnreadable>,
    /// Whether the changelists being fetched keep running while an earlier one is built.
    /// Without it they are polled only while no fetched changelist waits for the build, so
    /// a build that outlasts a p4 command's timeout fails each of their commands that still
    /// has output to read.
    pub fetch_during_build: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            backfill_derivation: BackfillDerivation::AllConfiguredTypes,
            file_concurrency: 16,
            changelist_lookahead: 2,
            max_build_time: Duration::from_mins(10),
            progress: Arc::default(),
            listing_window: None,
            copy_sources_with_uploads: false,
            content_reuse: ContentReuse::Off,
            unreadable_revisions: Arc::default(),
            skip_unreadable: None,
            fetch_during_build: false,
        }
    }
}

/// The revisions of `changelist` in the depot at `depot_root` that `keep` selects.
fn in_depot<'a>(
    depot_root: &str,
    changelist: &'a Changelist,
    keep: impl Fn(&FileRevision) -> bool,
) -> Result<Vec<&'a FileRevision>> {
    let mut revisions = Vec::new();
    for revision in &changelist.files {
        if keep(revision) && p4_transform::repo_path(depot_root, &revision.depot_path)?.is_some() {
            revisions.push(revision);
        }
    }
    Ok(revisions)
}

/// Stores the content of every revision in `changelist` that leaves content in the depot
/// at `depot_root`, `concurrency` at a time, keyed by depot path as
/// `p4_transform::changeset` takes it. Adds one to `progress` per file stored.
pub async fn upload_changelist_contents(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + FilestoreConfigRef),
    source: &impl ChangeSource,
    depot_root: &str,
    changelist: &Changelist,
    concurrency: usize,
    progress: &AtomicU64,
) -> Result<HashMap<String, ContentMetadataV2>> {
    let revisions = in_depot(depot_root, changelist, |revision| {
        revision.action.has_content()
    })?;
    // Owned items: a closure over borrowed ones makes the future not `Send` for every
    // lifetime, which a caller inside an `async_trait` (the p4_sync job) needs.
    stream::iter(revisions.into_iter().cloned())
        .map(|revision| async move {
            let name = || format!("{}#{}", revision.depot_path, revision.revision);
            let size = revision
                .size
                .with_context(|| format!("describe gave no size for {}", name()))?;
            let content = source.print(&revision).await?;
            let file_type = p4_transform::file_type(&revision.file_type);
            let metadata = upload_file_content(ctx, repo, file_type, size, content)
                .await
                .with_context(|| format!("storing {}", name()))?;
            anyhow::Ok((revision.depot_path.clone(), metadata))
        })
        .buffer_unordered(concurrency)
        .inspect_ok(|_| {
            progress.fetch_add(1, Ordering::Relaxed);
        })
        .try_collect()
        .await
}

/// The source paths of the revisions in `changelist` that were copied into the depot at
/// `depot_root`, keyed by depot path as `p4_transform::changeset` takes them, and the
/// number of copies with several sources, which get none. One `integrations` call covers
/// all of them.
async fn copy_sources(
    source: &impl ChangeSource,
    depot_root: &str,
    changelist: &Changelist,
) -> Result<(HashMap<String, NonRootMPath>, usize)> {
    let revisions = in_depot(depot_root, changelist, |revision| {
        p4_transform::can_have_copy_source(revision.action)
    })?;
    let integrations = if revisions.is_empty() {
        HashMap::new()
    } else {
        source.integrations(&revisions).await?
    };
    let mut copy_sources = HashMap::new();
    let mut ambiguous = 0;
    for revision in revisions {
        let integrations = integrations
            .get(&revision.depot_path)
            .with_context(|| format!("no integration records for {}", revision.depot_path))?;
        match p4_transform::copy_source(depot_root, revision, integrations)? {
            CopySource::From(path) => {
                copy_sources.insert(revision.depot_path.clone(), path);
            }
            CopySource::Ambiguous => ambiguous += 1,
            CopySource::NotACopy | CopySource::OtherDepot => {}
        }
    }
    Ok((copy_sources, ambiguous))
}

/// A changelist's changeset, with the number of copies it imported without `copy_from`.
#[derive(Debug)]
pub struct BuiltChangeset {
    pub change: u64,
    pub changeset: BonsaiChangeset,
    /// The changelist's files in another depot, which have no file change.
    pub other_depot_files: usize,
    /// Deletions under a file that replaced their directory, which have no file change.
    pub shadowed_deletions: usize,
    /// The source isn't a file in the parent.
    pub dropped_copy_sources: usize,
    /// Several sources, so none is used.
    pub ambiguous_copy_sources: usize,
    /// The revisions whose archive the server cannot read, imported as deletions.
    pub skipped_revisions: Vec<SkippedRevision>,
    pub timings: ChangelistTimings,
}

/// How long each stage of building one changelist's changeset took.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangelistTimings {
    pub describe: Duration,
    /// Printing and storing the content of `uploaded_files` files, and reusing stored content.
    pub upload: Duration,
    /// Files printed and stored, not those reused.
    pub uploaded_files: usize,
    pub content_reuse: ContentReuseReport,
    /// `filelog` of each branch and move/add.
    pub integrations: Duration,
    /// Whether each copy source is a file in the parent.
    pub copy_source_checks: Duration,
    /// The changeset itself, from what the stages before fetched.
    pub build: Duration,
}

/// What importing one changelist did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedChangelist {
    pub change: u64,
    pub cs_id: ChangesetId,
    pub file_changes: usize,
    pub other_depot_files: usize,
    pub shadowed_deletions: usize,
    pub dropped_copy_sources: usize,
    pub ambiguous_copy_sources: usize,
    pub unreadable_revisions: usize,
    pub timings: ChangelistTimings,
}

/// What importing a batch of changelists did, and how long its batch-wide stages took.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportedBatch {
    pub changelists: Vec<ImportedChangelist>,
    /// Building every changeset, so all of the changelists' stages.
    pub build: Duration,
    pub save_and_derive: Duration,
    /// Moving the bookmark and writing the mapping.
    pub commit: Duration,
}

/// The root content manifest of `base`, if any of `paths` isn't answered by `built` and
/// has to be looked up there. Derived once for all of a changelist's copy sources.
/// Design: designs/file_revisions_and_copy_from.md.
async fn base_root_for<'a>(
    ctx: &CoreContext,
    repo: &impl RepoDerivedDataRef,
    base: Option<ChangesetId>,
    built: &[BuiltChangeset],
    mut paths: impl Iterator<Item = &'a NonRootMPath>,
) -> Result<Option<ContentManifestId>> {
    let Some(base) = base else {
        return Ok(None);
    };
    if paths.all(|path| file_after(built.iter().map(|b| &b.changeset), path).is_some()) {
        return Ok(None);
    }
    // Locally: `derive` would go to the derived data service where the repo has one.
    let root = repo
        .repo_derived_data()
        .manager()
        .derive_locally::<RootContentManifestId>(ctx, base, None)
        .await?
        .into_content_manifest_id();
    Ok(Some(root))
}

/// The content and type of the file at `path` in the last of `built`, which is on top of
/// the saved base with root content manifest `base_root`, or `None` if it isn't a file
/// there. `built` isn't saved, so its own changes answer before the base's tree.
async fn file_in_parent(
    ctx: &CoreContext,
    repo: &impl RepoBlobstoreRef,
    base_root: Option<ContentManifestId>,
    built: &[BuiltChangeset],
    path: &NonRootMPath,
) -> Result<Option<(ContentId, FileType)>> {
    if let Some(file) = file_after(built.iter().map(|b| &b.changeset), path) {
        return Ok(file);
    }
    let Some(base_root) = base_root else {
        return Ok(None);
    };
    let entry = base_root
        .find_entry(
            ctx.clone(),
            repo.repo_blobstore().clone(),
            path.clone().into(),
        )
        .await?;
    Ok(match entry {
        Some(Entry::Leaf(file)) => Some((file.content_id, file.file_type)),
        _ => None,
    })
}

/// The content and type of the file at `path` after `changesets`, applied in order
/// (`Some(None)` if it isn't a file), or `None` if none of them touches it.
fn file_after<'a>(
    changesets: impl DoubleEndedIterator<Item = &'a BonsaiChangeset>,
    path: &NonRootMPath,
) -> Option<Option<(ContentId, FileType)>> {
    changesets.rev().find_map(|changeset| {
        let mut file = None;
        for (changed, change) in changeset.file_changes() {
            if changed == path {
                return Some(change.simplify().map(|c| (c.content_id(), c.file_type())));
            }
            // A file added above the path replaces its directory; one below makes it one.
            if change.is_changed() && changed.is_related_to(path) {
                file = Some(None);
            }
        }
        file
    })
}

/// What a changelist's changeset is built from: everything that doesn't depend on its
/// parent.
struct FetchedChangelist {
    changelist: Changelist,
    contents: HashMap<String, ContentMetadataV2>,
    copy_sources: HashMap<String, NonRootMPath>,
    ambiguous_copy_sources: usize,
    timings: ChangelistTimings,
    /// The revisions whose content was reused, not printed.
    reused: Vec<FileRevision>,
    /// The revisions whose archive the server cannot read.
    unreadable: Vec<SkippedRevision>,
}

async fn fetch_changelist(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + FilestoreConfigRef),
    source: &impl ChangeSource,
    depot_root: &str,
    change: u64,
    options: &ImportOptions,
) -> Result<FetchedChangelist> {
    let (describe, changelist) = source.describe(change).try_timed().await?;
    let upload = async {
        let concurrency = options.file_concurrency.max(1);
        let mode = match options.content_reuse {
            ContentReuse::Off
                if !options.unreadable_revisions.is_empty()
                    || options.skip_unreadable.is_some() =>
            {
                content_reuse::Mode::Off
            }
            ContentReuse::Off => {
                let contents = upload_changelist_contents(
                    ctx,
                    repo,
                    source,
                    depot_root,
                    &changelist,
                    concurrency,
                    &options.progress,
                )
                .await?;
                return anyhow::Ok(content_reuse::Uploaded {
                    printed_files: contents.len(),
                    contents,
                    ..Default::default()
                });
            }
            ContentReuse::Verify => content_reuse::Mode::Verify,
            ContentReuse::On => content_reuse::Mode::On,
        };
        content_reuse::upload_changelist_contents_reusing(
            ctx,
            repo,
            source,
            depot_root,
            &changelist,
            content_reuse::Upload {
                concurrency,
                progress: &options.progress,
                mode,
                approved_unreadable: &options.unreadable_revisions,
                skip_unreadable: options.skip_unreadable,
            },
        )
        .await
    }
    .try_timed();
    let integrations = copy_sources(source, depot_root, &changelist).try_timed();
    let ((upload, uploaded), (integrations, (copy_sources, ambiguous_copy_sources))) =
        if options.copy_sources_with_uploads {
            future::try_join(upload, integrations).await?
        } else {
            (upload.await?, integrations.await?)
        };
    Ok(FetchedChangelist {
        timings: ChangelistTimings {
            describe: describe.completion_time,
            upload: upload.completion_time,
            uploaded_files: uploaded.printed_files,
            content_reuse: uploaded.report,
            integrations: integrations.completion_time,
            ..Default::default()
        },
        changelist,
        contents: uploaded.contents,
        copy_sources,
        ambiguous_copy_sources,
        reused: uploaded.reused,
        unreadable: uploaded.unreadable,
    })
}

async fn build_changeset(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + RepoDerivedDataRef),
    depot_root: &str,
    commit_server_id: &str,
    base: Option<ChangesetId>,
    built: &[BuiltChangeset],
    fetched: FetchedChangelist,
    concurrency: usize,
) -> Result<BuiltChangeset> {
    let FetchedChangelist {
        changelist,
        contents,
        mut copy_sources,
        ambiguous_copy_sources,
        mut timings,
        reused: _,
        unreadable,
    } = fetched;
    let unreadable_paths: HashSet<String> = unreadable
        .iter()
        .map(|skipped| skipped.depot_path.clone())
        .collect();
    // Unreadable revisions are deletions, so have no copy source to check.
    copy_sources.retain(|target, _| !unreadable_paths.contains(target));
    // Mononoke doesn't check that copy_from exists in the parent, but derived data and
    // blame would follow it anyway.
    let (copy_source_checks, missing) = async {
        let base_root = base_root_for(ctx, repo, base, built, copy_sources.values()).await?;
        // Owned, as for the uploads: see upload_changelist_contents.
        stream::iter(copy_sources.clone())
            .map(|(target, copy_source)| async move {
                let parent_file = file_in_parent(ctx, repo, base_root, built, &copy_source).await?;
                anyhow::Ok(parent_file.is_none().then_some(target))
            })
            .buffer_unordered(concurrency)
            .try_filter_map(|missing| async move { Ok(missing) })
            .try_collect::<Vec<_>>()
            .await
    }
    .try_timed()
    .await?;
    timings.copy_source_checks = copy_source_checks.completion_time;
    let build_started = Instant::now();
    for target in &missing {
        copy_sources.remove(target);
    }
    let parent = built
        .last()
        .map(|b| b.changeset.get_changeset_id())
        .or(base);
    let changeset = p4_transform::changeset_with_unreadable(
        depot_root,
        commit_server_id,
        &changelist,
        parent,
        &contents,
        &copy_sources,
        &unreadable_paths,
    )?;
    let mut other_depot_files = 0;
    for revision in &changelist.files {
        if p4_transform::repo_path(depot_root, &revision.depot_path)?.is_none() {
            other_depot_files += 1;
        }
    }
    let in_depot_files = changelist.files.len() - other_depot_files;
    timings.build = build_started.elapsed();
    Ok(BuiltChangeset {
        change: changelist.number,
        other_depot_files,
        shadowed_deletions: in_depot_files.saturating_sub(changeset.file_changes().count()),
        changeset,
        dropped_copy_sources: missing.len(),
        ambiguous_copy_sources,
        skipped_revisions: unreadable,
        timings,
    })
}

/// Context of a changelist's fetch or build error, which names the changelist in a batch's
/// failure.
#[derive(Debug, Error)]
#[error("building changelist {0}")]
struct BuildingChangelist(u64);

/// The changesets for `changes`, in order, each on top of the one before and the first on
/// top of `base`, each recording the source's commit server's id (see `commit_server_id`).
/// Their contents are stored; the changesets themselves are not. Changelists are fetched ahead while earlier ones are
/// built, but built one at a time, in order. Once `options.max_build_time` has passed, the
/// changesets built so far are returned, at least one, and the fetches in flight are dropped.
pub async fn build_changesets(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + RepoDerivedDataRef + FilestoreConfigRef),
    source: &impl ChangeSource,
    depot_root: &str,
    base: Option<ChangesetId>,
    changes: &[u64],
    options: &ImportOptions,
) -> Result<Vec<BuiltChangeset>> {
    // Tokio's clock, which tests can pause.
    let started = tokio::time::Instant::now();
    let commit_server_id = commit_server_id(source).await?;
    let commit_server_id = commit_server_id.as_str();
    let concurrency = options.file_concurrency.max(1);
    // Room for one fetched changelist waiting for the build, besides those in flight.
    let (mut sender, mut receiver) = mpsc::channel(0);
    let fetch = async move {
        let fetch_one = |change: u64| async move {
            fetch_changelist(ctx, repo, source, depot_root, change, options)
                .await
                .with_context(|| BuildingChangelist(change))
        };
        let lookahead =
            NonZeroUsize::new(options.changelist_lookahead).unwrap_or(NonZeroUsize::MIN);
        if options.fetch_during_build {
            let fetches = changes.iter().copied().map(fetch_one);
            return fetch_ahead::send_in_order(fetches, lookahead, sender).await;
        }
        let mut fetched = pin!(
            stream::iter(changes.iter().copied())
                .map(fetch_one)
                .buffered(lookahead.get())
        );
        while let Some(fetched) = fetched.next().await {
            // Fails only once the build has stopped on an error.
            if sender.send(fetched).await.is_err() {
                break;
            }
        }
    };
    let build = async move {
        let mut built = Vec::with_capacity(changes.len());
        // Always the first changelist, then more for as long as `max_build_time` allows.
        while (built.is_empty() || started.elapsed() < options.max_build_time)
            && let Some(fetched) = receiver.next().await
        {
            let mut fetched: FetchedChangelist = fetched?;
            let change = fetched.changelist.number;
            // Every earlier changelist has been fetched by now.
            let reused = std::mem::take(&mut fetched.reused);
            let reprinted = content_reuse::reprint_collided(ctx, repo, source, reused, concurrency)
                .await
                .with_context(|| BuildingChangelist(change))?;
            for (depot_path, metadata) in reprinted {
                let reuse = &mut fetched.timings.content_reuse;
                reuse.reused_files -= 1;
                reuse.reused_bytes -= metadata.total_size;
                fetched.timings.uploaded_files += 1;
                fetched.contents.insert(depot_path, metadata);
            }
            let changeset = build_changeset(
                ctx,
                repo,
                depot_root,
                commit_server_id,
                base,
                &built,
                fetched,
                concurrency,
            )
            .await
            .with_context(|| BuildingChangelist(change))?;
            built.push(changeset);
            options.progress.fetch_add(1, Ordering::Relaxed);
        }
        anyhow::Ok(built)
    };
    // The build ends first when it fails or `max_build_time` has passed: returning then
    // drops the fetches in flight, however long they still have to run.
    let (build, fetch) = (pin!(build), pin!(fetch));
    match future::select(build, fetch).await {
        Either::Left((built, _fetch)) => built,
        Either::Right(((), build)) => build.await,
    }
}

/// The first `limit` changelists of the depot after `after`, up to the server's latest, in one
/// listing: a quiet depot on a busy server costs one empty listing, not a walk over every
/// changelist number the server has issued since.
pub async fn next_changes(
    source: &impl ChangeSource,
    depot_root: &str,
    after: Option<u64>,
    limit: usize,
) -> Result<Vec<u64>> {
    let latest = source.latest_change().await?;
    if limit == 0 || after.is_some_and(|after| after >= latest) {
        return Ok(Vec::new());
    }
    source
        .submitted_changes(depot_root, after, latest, limit)
        .await
}

/// What `next_changes` returns, from listings that each cover a range of changelist numbers:
/// the first covers `first_window` numbers after `after`, and each one that leaves the batch
/// short starts where it ended and covers twice as many, up to the server's latest. A
/// listing short of what it was asked for holds every changelist the depot has in its
/// range, so the ranges together list what one listing up to the latest would. The latest is
/// read once. A quiet depot on a busy server costs a few listings, the last of them wide.
pub async fn next_changes_in_windows(
    source: &impl ChangeSource,
    depot_root: &str,
    after: Option<u64>,
    limit: usize,
    first_window: NonZeroU64,
) -> Result<Vec<u64>> {
    let latest = source.latest_change().await?;
    let mut changes = Vec::new();
    let mut listed_upto = after;
    let mut window = first_window.get();
    while changes.len() < limit && listed_upto.is_none_or(|upto| upto < latest) {
        let upto = listed_upto.unwrap_or(0).saturating_add(window).min(latest);
        let wanted = limit - changes.len();
        let listed = source
            .submitted_changes(depot_root, listed_upto, upto, wanted)
            .await?;
        ensure!(
            listed.len() <= wanted,
            "listing {depot_root} after {listed_upto:?} up to {upto} returned {} changelists, \
             {wanted} asked for",
            listed.len()
        );
        changes.extend(listed);
        listed_upto = Some(upto);
        window = window.saturating_mul(2);
    }
    Ok(changes)
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum UnknownDepotRoot {
    #[error("the server has no depot {given}")]
    NotListed { given: String },
    #[error("the server has no depot {given}; did you mean {server_spelling}?")]
    WrongCase {
        given: String,
        server_spelling: String,
    },
    #[error("{given} is a {depot_type} depot; only local and stream depots can be imported")]
    UnsupportedType {
        given: String,
        depot_type: DepotType,
    },
}

/// Rejects a depot root that isn't exactly, case included, one of the server's depots, or
/// is a depot other than local or stream. A misspelt root would list no changelists, or on
/// a case-insensitive server list them but match none of their files and import each as an
/// empty commit. Another type of depot would fail somewhere less obvious.
/// Design: designs/import_flow.md.
pub async fn check_depot_root(source: &impl ChangeSource, depot_root: &str) -> Result<()> {
    let depots = source.depots().await?;
    let root = |depot: &Depot| format!("//{}/", depot.name);
    let given = depot_root.to_owned();
    let error = if let Some(depot) = depots.iter().find(|depot| root(depot) == depot_root) {
        match depot.depot_type {
            DepotType::Local | DepotType::Stream => return Ok(()),
            _ => UnknownDepotRoot::UnsupportedType {
                given,
                depot_type: depot.depot_type.clone(),
            },
        }
    } else {
        match depots
            .iter()
            .map(root)
            .find(|root| root.eq_ignore_ascii_case(depot_root))
        {
            Some(server_spelling) => UnknownDepotRoot::WrongCase {
                given,
                server_spelling,
            },
            None => UnknownDepotRoot::NotListed { given },
        }
    };
    Err(error.into())
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error(
    "the Perforce server at {address} has no ServerID (set with `p4 serverid`), so its history can't be told from another server's"
)]
pub struct NoServerId {
    pub address: String,
}

/// The commit server's id, which each imported changeset records: a replica's commit server,
/// or the server itself when standalone. A failover or a move to another replica keeps it.
async fn commit_server_id(source: &impl ChangeSource) -> Result<String> {
    let info = source.server_info().await?;
    info.commit_server_id.or(info.server_id).ok_or_else(|| {
        NoServerId {
            address: info.address,
        }
        .into()
    })
}

async fn head_changeset(
    ctx: &CoreContext,
    repo: &(impl BookmarksRef + RepoBlobstoreRef),
    bookmark: &BookmarkKey,
) -> Result<Option<BonsaiChangeset>> {
    let Some(cs_id) = repo
        .bookmarks()
        .get(ctx.clone(), bookmark, Freshness::MostRecent)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(cs_id.load(ctx, repo.repo_blobstore()).await?))
}

/// The bookmark's changeset and the changelist it was imported from, or `None` before
/// the first import.
async fn head(
    ctx: &CoreContext,
    repo: &(impl BookmarksRef + RepoBlobstoreRef),
    bookmark: &BookmarkKey,
) -> Result<Option<(ChangesetId, u64)>> {
    let Some(changeset) = head_changeset(ctx, repo, bookmark).await? else {
        return Ok(None);
    };
    let change = P4ChangelistId::from_bcs(&changeset)?.id();
    Ok(Some((changeset.get_changeset_id(), change)))
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResumeMismatch {
    #[error("{bookmark} was imported from Perforce server {recorded}, not {current}")]
    OtherServer {
        bookmark: String,
        recorded: String,
        current: String,
    },
    #[error(
        "{bookmark}'s changeset records no Perforce server ({})",
        p4_transform::SERVER_ID_EXTRA
    )]
    NoServerRecorded { bookmark: String },
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error(
    "the repo has p4 mappings up to changelist {max_change} but no {bookmark}: refusing a first import"
)]
pub struct MappedWithoutBookmark {
    pub bookmark: String,
    pub max_change: u64,
}

/// Rejects a first import onto `bookmark` in a repo that already maps changelists: they were
/// imported onto another bookmark, or this one was deleted. A fresh history would either
/// mix with them in the mapping, or conflict with it after the whole batch is built.
/// Design: designs/import_flow.md.
pub async fn check_first_import(
    ctx: &CoreContext,
    repo: &impl BonsaiP4MappingRef,
    bookmark: &BookmarkKey,
) -> Result<()> {
    match repo.bonsai_p4_mapping().get_max(ctx).await? {
        None => Ok(()),
        Some(max) => Err(MappedWithoutBookmark {
            bookmark: bookmark.to_string(),
            max_change: max.id(),
        }
        .into()),
    }
}

/// Rejects a server without a `ServerID`, and resuming unless the bookmark's changeset
/// recorded this server's commit server id (see `commit_server_id`). Changelist numbers are
/// only unique per commit server and a changelist has no parent field, so nothing else ties
/// the bookmark's history to the server (see `import_changelists`): without the check,
/// another server's changelists would build on top of it. The depot isn't checked: it is
/// the repo config's to change. Without the bookmark, it must be a first import (see
/// `check_first_import`).
/// Design: designs/import_flow.md.
pub async fn check_resume_continuity(
    ctx: &CoreContext,
    repo: &(impl BookmarksRef + BonsaiP4MappingRef + RepoBlobstoreRef),
    source: &impl ChangeSource,
    bookmark: &BookmarkKey,
) -> Result<()> {
    let current = commit_server_id(source).await?;
    let Some(changeset) = head_changeset(ctx, repo, bookmark).await? else {
        return check_first_import(ctx, repo, bookmark).await;
    };
    let bookmark = bookmark.to_string();
    match p4_transform::recorded_server_id(&changeset) {
        Some(recorded) if recorded == current => Ok(()),
        Some(recorded) => Err(ResumeMismatch::OtherServer {
            bookmark,
            recorded: recorded.to_owned(),
            current,
        }
        .into()),
        None => Err(ResumeMismatch::NoServerRecorded { bookmark }.into()),
    }
}

/// The last changelist imported onto `bookmark`: the next import starts after it. The
/// bookmark is the checkpoint, so nothing else needs to record it.
pub async fn resume_point(
    ctx: &CoreContext,
    repo: &(impl BookmarksRef + RepoBlobstoreRef),
    bookmark: &BookmarkKey,
) -> Result<Option<u64>> {
    Ok(head(ctx, repo, bookmark).await?.map(|(_, change)| change))
}

/// Imports `changes`, in order, onto `bookmark` (created by the first import) as
/// `options` say, and returns their changesets: of the first of them only, if
/// `options.max_build_time` ends the batch early. The batch must start after the
/// `resume_point`, so nothing is imported twice. `depot_root` must pass `check_depot_root`
/// and the bookmark `check_resume_continuity`, as `import_next` makes sure. Logs a row
/// per changelist to the context's Scuba table and, once the bookmark has moved, the
/// changesets and the move to the repo's update logs (`log_repo_update`).
///
/// A changelist has no parent field, unlike a git commit: its parent is whatever the
/// bookmark points at, so the bookmark is the only record of where the import is. That is
/// why the bookmark and the mapping move in one transaction, with a compare-and-swap: if
/// they disagreed, or another import had moved the bookmark, the next changelist would get
/// the wrong parent and every later commit would build on the wrong history.
pub async fn import_changelists(
    ctx: &CoreContext,
    repo: &(
         impl BookmarksRef
         + BonsaiGitMappingRef
         + BonsaiGlobalrevMappingRef
         + BonsaiP4MappingArc
         + CommitGraphRef
         + CommitGraphWriterRef
         + FilestoreConfigRef
         + RepoBlobstoreRef
         + RepoConfigRef
         + RepoDerivedDataRef
         + RepoIdentityRef
         + Send
         + Sync
     ),
    source: &impl ChangeSource,
    depot_root: &str,
    bookmark: &BookmarkKey,
    changes: &[u64],
    options: &ImportOptions,
) -> Result<ImportedBatch> {
    let Some(&last) = changes.last() else {
        return Ok(ImportedBatch::default());
    };
    let batch = Batch {
        repo: repo.repo_identity().name(),
        depot_root,
        bookmark,
        changes,
    };
    let head = async {
        ensure!(
            changes.is_sorted_by(|a, b| a < b),
            "changelists to import are not in increasing order"
        );
        let head = head(ctx, repo, bookmark).await?;
        if let Some((_, checkpoint)) = head {
            ensure!(
                changes[0] > checkpoint,
                "{bookmark} is already at changelist {checkpoint}, but the batch starts at {}",
                changes[0]
            );
        }
        anyhow::Ok(head)
    }
    .await
    .map_err(|error| batch.failed(ctx, Stage::Prepare, error))?;
    tracing::info!(
        "importing changelists {}..={last} ({}) of {depot_root} into {} {bookmark}, after changelist {}; session {}",
        changes[0],
        changes.len(),
        batch.repo,
        head.map_or_else(
            || "none".to_owned(),
            |(_, checkpoint)| checkpoint.to_string()
        ),
        ctx.metadata().session_id(),
    );
    let old = head.map(|(cs_id, _)| cs_id);
    let (build, built) = build_changesets(ctx, repo, source, depot_root, old, changes, options)
        .try_timed()
        .await
        .map_err(|error| batch.failed(ctx, Stage::Build, error))?;
    // `max_build_time` can end the batch early: from here on it is the changelists built.
    let (last, new) = built
        .last()
        .map(|built| (built.change, built.changeset.get_changeset_id()))
        .context("built no changesets")?;
    if built.len() < changes.len() {
        tracing::info!(
            "batch cut after changelist {last} ({} of {}): build took {:?}, over max_build_time {:?}; session {}",
            built.len(),
            changes.len(),
            build.completion_time,
            options.max_build_time,
            ctx.metadata().session_id(),
        );
    }
    let changes = &changes[..built.len()];
    let batch = Batch { changes, ..batch };
    let ((imported, changesets), skipped): ((Vec<_>, Vec<_>), Vec<_>) = built
        .into_iter()
        .map(|built| {
            let cs_id = built.changeset.get_changeset_id();
            let imported = ImportedChangelist {
                change: built.change,
                cs_id,
                file_changes: built.changeset.file_changes().count(),
                other_depot_files: built.other_depot_files,
                shadowed_deletions: built.shadowed_deletions,
                dropped_copy_sources: built.dropped_copy_sources,
                ambiguous_copy_sources: built.ambiguous_copy_sources,
                unreadable_revisions: built.skipped_revisions.len(),
                timings: built.timings,
            };
            (
                (imported, built.changeset),
                (built.change, cs_id, built.skipped_revisions),
            )
        })
        .unzip();
    // Each `CommitInfo` holds up to 10k paths; `log_new_commits` ignores them without a destination.
    let commit_infos = if repo
        .repo_config()
        .update_logging_config
        .new_commit_logging_destination
        .is_some()
    {
        changesets
            .iter()
            .map(|bcs| CommitInfo::new(bcs, None))
            .collect()
    } else {
        Vec::new()
    };
    let derive = options
        .backfill_derivation
        .types(&repo.repo_derived_data().active_config().types);
    let (save_and_derive, _) = save_and_derive_batch(ctx, repo, changesets, &derive)
        .try_timed()
        .await
        .with_context(|| format!("saving changelists {}..={last}", changes[0]))
        .map_err(|error| batch.failed(ctx, Stage::SaveAndDerive, error))?;

    let (commit, ()) = async {
        let mut txn = repo.bookmarks().create_transaction(ctx.clone());
        match old {
            None => txn.create(bookmark, new, IMPORT_REASON)?,
            Some(old) => txn.update(bookmark, new, old, IMPORT_REASON)?,
        }
        let mapping = mapping_txn_hook(repo.bonsai_p4_mapping_arc(), &imported);
        // Hooks run before the bookmark's compare-and-swap, so a concurrent import of the same
        // changelists fails as a mapping `Conflict` rather than here. Both roll back everything.
        ensure!(
            txn.commit_with_hooks(vec![mapping]).await?.is_some(),
            "{bookmark} moved while importing changelists {}..={last}",
            changes[0]
        );
        anyhow::Ok(())
    }
    .try_timed()
    .await
    .with_context(|| format!("committing changelists {}..={last}", changes[0]))
    .map_err(|error| batch.failed(ctx, Stage::Commit, error))?;
    let imported = ImportedBatch {
        changelists: imported,
        build: build.completion_time,
        save_and_derive: save_and_derive.completion_time,
        commit: commit.completion_time,
    };
    // Synchronous, so a stop during the update-feed await below can't skip them.
    log_imported(ctx, &imported, options.content_reuse);
    log_skipped(ctx, &skipped);
    trace_imported(ctx, depot_root, bookmark, old, new, &imported);
    log_repo_update(ctx, repo, bookmark, old, new, commit_infos).await;
    Ok(imported)
}

/// Where a batch failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Checking the depot and the bookmark, and listing the changelists.
    Prepare,
    /// Fetching the changelists from Perforce, storing their contents, building the changesets.
    Build,
    SaveAndDerive,
    Commit,
}

impl Stage {
    fn as_str(self) -> &'static str {
        match self {
            Stage::Prepare => "prepare",
            Stage::Build => "build",
            Stage::SaveAndDerive => "save_and_derive",
            Stage::Commit => "commit",
        }
    }
}

#[derive(Debug, Error)]
#[error("failed in {}", .0.as_str())]
struct FailedIn(Stage);

/// The stage an `import_next` error came from.
pub fn failed_stage(error: &anyhow::Error) -> Option<Stage> {
    error.downcast_ref::<FailedIn>().map(|failed| failed.0)
}

/// Keeps a row readable when the error quotes a long p4 command or output.
const MAX_LOGGED_ERROR_CHARS: usize = 4096;

/// A batch as its failure reports it.
struct Batch<'a> {
    repo: &'a str,
    depot_root: &'a str,
    bookmark: &'a BookmarkKey,
    changes: &'a [u64],
}

impl Batch<'_> {
    /// Logs the failure to the job's log, Scuba and ODS, and returns it with its stage (see
    /// `failed_stage`). The error names the changelist when one failed: fetching and building
    /// run one changelist at a time.
    fn failed(&self, ctx: &CoreContext, stage: Stage, error: anyhow::Error) -> anyhow::Error {
        STATS::failed_batches.add_value(1, (stage.as_str(),));
        let failed_change = error.downcast_ref::<BuildingChangelist>().map(|c| c.0);
        let error_text: String = format!("{error:#}")
            .chars()
            .take(MAX_LOGGED_ERROR_CHARS)
            .collect();
        let mut scuba = ctx.scuba().clone();
        scuba
            .add("repo", self.repo)
            .add("depot_root", self.depot_root)
            .add("bookmark", self.bookmark.to_string())
            .add("stage", stage.as_str())
            .add("batch_changelists", self.changes.len())
            .add_opt("first_change", self.changes.first().copied())
            .add_opt("last_change", self.changes.last().copied())
            .add_opt("failed_change", failed_change)
            .add("error", error_text);
        scuba.log_with_msg("P4 batch failed", None);
        let range = match (self.changes.first(), self.changes.last()) {
            (Some(first), Some(last)) => format!(" changelists {first}..={last}"),
            _ => String::new(),
        };
        let at =
            failed_change.map_or_else(String::new, |change| format!(" at changelist {change}"));
        tracing::error!(
            "importing{range} of {} into {} {} failed in {}{at}: {error:#}; session {}",
            self.depot_root,
            self.repo,
            self.bookmark,
            stage.as_str(),
            ctx.metadata().session_id(),
        );
        error.context(FailedIn(stage))
    }
}

const IMPORT_REASON: BookmarkUpdateReason = BookmarkUpdateReason::Blobimport;

/// Feeds the repo's new-commit and bookmark logs, which the bookmark transaction doesn't
/// write: bookmarks_movement does it for pushes, but the import moves the bookmark itself.
/// Like there, failures only reach Scuba: the move has already committed.
async fn log_repo_update(
    ctx: &CoreContext,
    repo: &(
         impl BonsaiGitMappingRef
         + BonsaiGlobalrevMappingRef
         + CommitGraphRef
         + RepoConfigRef
         + RepoIdentityRef
     ),
    bookmark: &BookmarkKey,
    old: Option<ChangesetId>,
    new: ChangesetId,
    commit_infos: Vec<CommitInfo>,
) {
    // `create` makes a pull-default bookmark, and `update` keeps the kind.
    let kind = BookmarkKind::PullDefaultPublishing;
    log_new_commits(ctx, repo, Some((bookmark, kind)), commit_infos).await;
    let operation = match old {
        None => BookmarkOperation::Create(new),
        Some(old) => BookmarkOperation::Update(old, new),
    };
    let info = BookmarkInfo {
        bookmark_name: bookmark.clone(),
        bookmark_kind: kind,
        operation,
        reason: IMPORT_REASON,
    };
    log_bookmark_operation(ctx, repo, &info).await;
}

/// One Scuba row per revision imported without content, once its changelist is committed:
/// the list to go by if they are ever to be imported again.
fn log_skipped(ctx: &CoreContext, skipped: &[(u64, ChangesetId, Vec<SkippedRevision>)]) {
    for (change, cs_id, revisions) in skipped {
        for revision in revisions {
            if revision.reason == SkipReason::RetriesFailed {
                STATS::unreadable_revisions_skipped_after_retries.add_value(1);
            }
            let mut scuba = ctx.scuba().clone();
            scuba
                .add("p4_change", *change)
                .add("changeset_id", cs_id.to_string())
                .add("depot_path", revision.depot_path.as_str())
                .add("revision", revision.revision)
                .add("skip_reason", revision.reason.as_str());
            scuba.log_with_msg("P4 revision skipped", None);
        }
    }
}

fn log_imported(ctx: &CoreContext, batch: &ImportedBatch, content_reuse: ContentReuse) {
    let micros = |duration: Duration| duration.as_micros() as u64;
    for imported in &batch.changelists {
        let timings = &imported.timings;
        let mut scuba = ctx.scuba().clone();
        scuba
            .add("p4_change", imported.change)
            .add("changeset_id", imported.cs_id.to_string())
            .add("file_changes", imported.file_changes)
            .add("uploaded_files", timings.uploaded_files)
            .add("describe_us", micros(timings.describe))
            .add("upload_us", micros(timings.upload))
            .add("integrations_us", micros(timings.integrations))
            .add("copy_source_checks_us", micros(timings.copy_source_checks))
            .add("build_us", micros(timings.build))
            .add("batch_changelists", batch.changelists.len())
            .add("batch_build_us", micros(batch.build))
            .add("batch_save_and_derive_us", micros(batch.save_and_derive))
            .add("batch_commit_us", micros(batch.commit));
        if imported.unreadable_revisions > 0 {
            STATS::unreadable_revisions.add_value(imported.unreadable_revisions as i64);
            scuba.add("unreadable_revisions", imported.unreadable_revisions);
        }
        let reuse = &timings.content_reuse;
        if content_reuse != ContentReuse::Off {
            scuba
                .add("content_reuse", format!("{content_reuse:?}").to_lowercase())
                .add("reused_files", reuse.reused_files)
                .add("reused_bytes", reuse.reused_bytes)
                .add("reusable_files", reuse.reusable_files)
                .add("reusable_bytes", reuse.reusable_bytes)
                .add("content_reuse_lookup_us", micros(reuse.lookup));
        }
        scuba.log_with_msg("P4 changelist imported", None);
    }
}

/// For the job's log: one line per batch, a line per changelist at debug. Numbers and ids
/// only; paths and descriptions stay in Scuba, behind its ACL.
fn trace_imported(
    ctx: &CoreContext,
    depot_root: &str,
    bookmark: &BookmarkKey,
    old: Option<ChangesetId>,
    new: ChangesetId,
    batch: &ImportedBatch,
) {
    for imported in &batch.changelists {
        tracing::debug!(
            "changelist {} -> {}: {} file changes, {} uploaded in {:?}",
            imported.change,
            imported.cs_id,
            imported.file_changes,
            imported.timings.uploaded_files,
            imported.timings.upload,
        );
        if imported.unreadable_revisions > 0 {
            tracing::warn!(
                "changelist {} -> {}: {} revisions the server cannot read imported as deletions; session {}",
                imported.change,
                imported.cs_id,
                imported.unreadable_revisions,
                ctx.metadata().session_id(),
            );
        }
    }
    let (Some(first), Some(last)) = (batch.changelists.first(), batch.changelists.last()) else {
        return;
    };
    let old = old.map_or_else(|| "nothing".to_owned(), |old| old.to_string());
    tracing::info!(
        "imported changelists {}..={} ({}) of {depot_root}: {bookmark} {old} -> {new}; build {:?}, save+derive {:?}, commit {:?}; session {}",
        first.change,
        last.change,
        batch.changelists.len(),
        batch.build,
        batch.save_and_derive,
        batch.commit,
        ctx.metadata().session_id(),
    );
}

/// Imports the first `limit` changelists of the depot after the bookmark's `resume_point`,
/// as `import_changelists` does. Nothing is listed or fetched before `check_depot_root`
/// accepts `depot_root` and `check_resume_continuity` the bookmark.
pub async fn import_next(
    ctx: &CoreContext,
    repo: &(
         impl BookmarksRef
         + BonsaiGitMappingRef
         + BonsaiGlobalrevMappingRef
         + BonsaiP4MappingArc
         + CommitGraphRef
         + CommitGraphWriterRef
         + FilestoreConfigRef
         + RepoBlobstoreRef
         + RepoConfigRef
         + RepoDerivedDataRef
         + RepoIdentityRef
         + Send
         + Sync
     ),
    source: &impl ChangeSource,
    depot_root: &str,
    bookmark: &BookmarkKey,
    limit: usize,
    options: &ImportOptions,
) -> Result<ImportedBatch> {
    let changes = async {
        check_depot_root(source, depot_root).await?;
        check_resume_continuity(ctx, repo, source, bookmark).await?;
        let after = resume_point(ctx, repo, bookmark).await?;
        match options.listing_window {
            None => next_changes(source, depot_root, after, limit).await,
            Some(first_window) => {
                next_changes_in_windows(source, depot_root, after, limit, first_window).await
            }
        }
    }
    .await
    .map_err(|error| {
        let batch = Batch {
            repo: repo.repo_identity().name(),
            depot_root,
            bookmark,
            changes: &[],
        };
        batch.failed(ctx, Stage::Prepare, error)
    })?;
    import_changelists(ctx, repo, source, depot_root, bookmark, &changes, options).await
}

/// Writes the batch's mapping in the bookmark move's transaction, so the bookmark never
/// points at a changeset whose changelist isn't mapped, nor the reverse.
fn mapping_txn_hook(
    mapping: Arc<dyn BonsaiP4Mapping>,
    imported: &[ImportedChangelist],
) -> BookmarkTransactionHook {
    let entries: Arc<[_]> = imported
        .iter()
        .map(|imported| {
            BonsaiP4MappingEntry::new(imported.cs_id, P4ChangelistId::new(imported.change))
        })
        .collect();
    Arc::new(move |ctx, txn| {
        let mapping = mapping.clone();
        let entries = entries.clone();
        async move {
            mapping
                .bulk_import_in_transaction(&ctx, &entries, txn)
                .await
                .map_err(|error| BookmarkTransactionError::Other(error.into()))
        }
        .boxed()
    })
}

#[cfg(test)]
mod tests;
