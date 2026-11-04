/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Imports a Perforce depot into a repo.

use std::collections::HashMap;
use std::pin::pin;
use std::sync::Arc;
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
use futures::stream;
use futures_stats::TimedTryFutureExt;
use import_common::BackfillDerivation;
use import_common::save_and_derive_batch;
use manifest::Entry;
use manifest::ManifestOps;
use metaconfig_types::RepoConfigRef;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::ContentManifestId;
use mononoke_types::ContentMetadataV2;
use mononoke_types::DerivableType;
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
}

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

/// How to import: what to derive, and how much runs at once.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Derived for every imported changeset before the bookmark moves, if the repo
    /// enables them. Keep `ContentManifests` in it: the next batch's copy-source check
    /// reads the base's content manifest, and would otherwise derive it there.
    pub derive: Vec<DerivableType>,
    /// Per changelist, the files printed and stored and the copy sources checked at once.
    /// At least 1.
    pub file_concurrency: usize,
    /// Changelists fetched at once, while an earlier one is built. At least 1.
    pub changelist_lookahead: usize,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            derive: vec![DerivableType::ContentManifests],
            file_concurrency: 16,
            changelist_lookahead: 2,
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
/// `p4_transform::changeset` takes it.
pub async fn upload_changelist_contents(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + FilestoreConfigRef),
    source: &impl ChangeSource,
    depot_root: &str,
    changelist: &Changelist,
    concurrency: usize,
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
    pub timings: ChangelistTimings,
}

/// How long each stage of building one changelist's changeset took.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangelistTimings {
    pub describe: Duration,
    /// Printing and storing the content of `uploaded_files` files.
    pub upload: Duration,
    pub uploaded_files: usize,
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
}

async fn fetch_changelist(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + FilestoreConfigRef),
    source: &impl ChangeSource,
    depot_root: &str,
    change: u64,
    concurrency: usize,
) -> Result<FetchedChangelist> {
    let (describe, changelist) = source.describe(change).try_timed().await?;
    let (upload, contents) =
        upload_changelist_contents(ctx, repo, source, depot_root, &changelist, concurrency)
            .try_timed()
            .await?;
    let (integrations, (copy_sources, ambiguous_copy_sources)) =
        copy_sources(source, depot_root, &changelist)
            .try_timed()
            .await?;
    Ok(FetchedChangelist {
        timings: ChangelistTimings {
            describe: describe.completion_time,
            upload: upload.completion_time,
            uploaded_files: contents.len(),
            integrations: integrations.completion_time,
            ..Default::default()
        },
        changelist,
        contents,
        copy_sources,
        ambiguous_copy_sources,
    })
}

async fn build_changeset(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + RepoDerivedDataRef),
    depot_root: &str,
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
    } = fetched;
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
    let changeset =
        p4_transform::changeset(depot_root, &changelist, parent, &contents, &copy_sources)?;
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
        timings,
    })
}

/// Context of a changelist's fetch or build error, which names the changelist in a batch's
/// failure.
#[derive(Debug, Error)]
#[error("building changelist {0}")]
struct BuildingChangelist(u64);

/// The changesets for `changes`, in order, each on top of the one before and the first on
/// top of `base`. Their contents are stored; the changesets themselves are not. Changelists
/// are fetched ahead while earlier ones are built, but built one at a time, in order.
pub async fn build_changesets(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + RepoDerivedDataRef + FilestoreConfigRef),
    source: &impl ChangeSource,
    depot_root: &str,
    base: Option<ChangesetId>,
    changes: &[u64],
    options: &ImportOptions,
) -> Result<Vec<BuiltChangeset>> {
    let concurrency = options.file_concurrency.max(1);
    // Room for one fetched changelist waiting for the build, besides those in flight.
    let (mut sender, mut receiver) = mpsc::channel(0);
    let fetch = async move {
        let mut fetched = pin!(
            stream::iter(changes.iter().copied())
                .map(|change| async move {
                    fetch_changelist(ctx, repo, source, depot_root, change, concurrency)
                        .await
                        .with_context(|| BuildingChangelist(change))
                })
                .buffered(options.changelist_lookahead.max(1))
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
        while let Some(fetched) = receiver.next().await {
            let fetched: FetchedChangelist = fetched?;
            let change = fetched.changelist.number;
            let changeset =
                build_changeset(ctx, repo, depot_root, base, &built, fetched, concurrency)
                    .await
                    .with_context(|| BuildingChangelist(change))?;
            built.push(changeset);
        }
        anyhow::Ok(built)
    };
    let ((), built) = future::join(fetch, build).await;
    built
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
    #[error(
        "{bookmark} was imported from another depot: changelist {change} has no files in {depot_root}"
    )]
    OtherDepot {
        bookmark: String,
        change: u64,
        depot_root: String,
    },
    #[error("changelist {change} on this server doesn't match what {bookmark} recorded")]
    RevisionsDiffer { bookmark: String, change: u64 },
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

/// Rejects resuming unless the server's describe of the bookmark's changelist, in the
/// depot at `depot_root`, gives the file revisions its changeset recorded. A changelist
/// has no parent field, so nothing else ties the bookmark's history to this depot and
/// server (see `import_changelists`): without the check, another depot's changelists
/// would build on top of it. Without the bookmark, it must be a first import (see
/// `check_first_import`). Design: designs/import_flow.md.
pub async fn check_resume_continuity(
    ctx: &CoreContext,
    repo: &(impl BookmarksRef + BonsaiP4MappingRef + RepoBlobstoreRef),
    source: &impl ChangeSource,
    depot_root: &str,
    bookmark: &BookmarkKey,
) -> Result<()> {
    let Some(changeset) = head_changeset(ctx, repo, bookmark).await? else {
        return check_first_import(ctx, repo, bookmark).await;
    };
    let change = P4ChangelistId::from_bcs(&changeset)?.id();
    let recorded = p4_transform::recorded_file_revs(&changeset).with_context(|| {
        format!(
            "{bookmark}'s changeset has no {} extra",
            p4_transform::FILE_REVS_EXTRA
        )
    })?;
    let changelist = source.describe(change).await?;
    let on_server = p4_transform::file_revs(depot_root, &changelist)?;
    if on_server == recorded {
        return Ok(());
    }
    let bookmark = bookmark.to_string();
    let error = if on_server.is_empty() {
        ResumeMismatch::OtherDepot {
            bookmark,
            change,
            depot_root: depot_root.to_owned(),
        }
    } else {
        ResumeMismatch::RevisionsDiffer { bookmark, change }
    };
    Err(error.into())
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
/// `options` say, and returns their changesets. The batch must start after the
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
    let (imported, changesets): (Vec<_>, Vec<_>) = built
        .into_iter()
        .map(|built| {
            let imported = ImportedChangelist {
                change: built.change,
                cs_id: built.changeset.get_changeset_id(),
                file_changes: built.changeset.file_changes().count(),
                other_depot_files: built.other_depot_files,
                shadowed_deletions: built.shadowed_deletions,
                dropped_copy_sources: built.dropped_copy_sources,
                ambiguous_copy_sources: built.ambiguous_copy_sources,
                timings: built.timings,
            };
            (imported, built.changeset)
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
    let derive = BackfillDerivation::OnlySpecificTypes(options.derive.clone())
        .types(&repo.repo_derived_data().active_config().types);
    let (save_and_derive, _) = save_and_derive_batch(ctx, repo, changesets, &derive)
        .try_timed()
        .await
        .with_context(|| format!("saving changelists {}..={last}", changes[0]))
        .map_err(|error| batch.failed(ctx, Stage::SaveAndDerive, error))?;

    let new = imported.last().context("built no changesets")?.cs_id;
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
    log_imported(ctx, &imported);
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

fn log_imported(ctx: &CoreContext, batch: &ImportedBatch) {
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
        check_resume_continuity(ctx, repo, source, depot_root, bookmark).await?;
        let after = resume_point(ctx, repo, bookmark).await?;
        next_changes(source, depot_root, after, limit).await
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
