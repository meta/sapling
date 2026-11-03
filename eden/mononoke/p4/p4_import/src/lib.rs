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

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use blobstore::Loadable;
use bonsai_p4_mapping::BonsaiP4Mapping;
use bonsai_p4_mapping::BonsaiP4MappingArc;
use bonsai_p4_mapping::BonsaiP4MappingEntry;
use bookmarks::BookmarkKey;
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
use futures::Stream;
use futures::TryStreamExt;
use futures::stream;
use import_common::BackfillDerivation;
use import_common::save_and_derive_batch;
use manifest::Entry;
use manifest::ManifestOps;
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
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;

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
const MAX_SYMLINK_SIZE: u64 = 64 * 1024;

/// Like `upload_content`, but stores a symlink as its bare target.
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

/// Stores the content of every revision in `changelist` that leaves content in the depot
/// at `depot_root`, keyed by depot path as `p4_transform::changeset` takes it.
pub async fn upload_changelist_contents(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + FilestoreConfigRef),
    source: &impl ChangeSource,
    depot_root: &str,
    changelist: &Changelist,
) -> Result<HashMap<String, ContentMetadataV2>> {
    let mut contents = HashMap::new();
    // One file at a time for now.
    for revision in &changelist.files {
        if !revision.action.has_content()
            || p4_transform::repo_path(depot_root, &revision.depot_path)?.is_none()
        {
            continue;
        }
        let name = || format!("{}#{}", revision.depot_path, revision.revision);
        let size = revision
            .size
            .with_context(|| format!("describe gave no size for {}", name()))?;
        let content = source.print(revision).await?;
        let file_type = p4_transform::file_type(&revision.file_type);
        let metadata = upload_file_content(ctx, repo, file_type, size, content)
            .await
            .with_context(|| format!("storing {}", name()))?;
        contents.insert(revision.depot_path.clone(), metadata);
    }
    Ok(contents)
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
    let mut revisions = Vec::new();
    for revision in &changelist.files {
        if !p4_transform::can_have_copy_source(revision.action)
            || p4_transform::repo_path(depot_root, &revision.depot_path)?.is_none()
        {
            continue;
        }
        revisions.push(revision);
    }
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
}

/// The root content manifest of `base`, if any of `paths` isn't answered by `built` and
/// has to be looked up there. Derived once for all of a changelist's copy sources.
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

async fn build_changeset(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + RepoDerivedDataRef + FilestoreConfigRef),
    source: &impl ChangeSource,
    depot_root: &str,
    base: Option<ChangesetId>,
    built: &[BuiltChangeset],
    change: u64,
) -> Result<BuiltChangeset> {
    let changelist = source.describe(change).await?;
    let contents = upload_changelist_contents(ctx, repo, source, depot_root, &changelist).await?;
    let (mut copy_sources, ambiguous_copy_sources) =
        copy_sources(source, depot_root, &changelist).await?;
    // Mononoke doesn't check that copy_from exists in the parent, but derived data and
    // blame would follow it anyway.
    let base_root = base_root_for(ctx, repo, base, built, copy_sources.values()).await?;
    let mut missing = Vec::new();
    for (target, copy_source) in &copy_sources {
        if file_in_parent(ctx, repo, base_root, built, copy_source)
            .await?
            .is_none()
        {
            missing.push(target.clone());
        }
    }
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
    Ok(BuiltChangeset {
        change,
        other_depot_files,
        shadowed_deletions: in_depot_files.saturating_sub(changeset.file_changes().count()),
        changeset,
        dropped_copy_sources: missing.len(),
        ambiguous_copy_sources,
    })
}

/// The changesets for `changes`, in order, each on top of the one before and the first on
/// top of `base`. Their contents are stored; the changesets themselves are not.
pub async fn build_changesets(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + RepoDerivedDataRef + FilestoreConfigRef),
    source: &impl ChangeSource,
    depot_root: &str,
    base: Option<ChangesetId>,
    changes: &[u64],
) -> Result<Vec<BuiltChangeset>> {
    let mut built = Vec::with_capacity(changes.len());
    for &change in changes {
        let changeset = build_changeset(ctx, repo, source, depot_root, base, &built, change)
            .await
            .with_context(|| format!("building changelist {change}"))?;
        built.push(changeset);
    }
    Ok(built)
}

/// How many changelist numbers one `ChangeSource::submitted_changes` call covers.
pub const CHANGES_WINDOW: u64 = 10_000;

/// The first `limit` changelists of the depot after `after`, listed `window` changelist
/// numbers at a time up to the server's latest, so no listing grows with the depot.
pub async fn next_changes(
    source: &impl ChangeSource,
    depot_root: &str,
    after: Option<u64>,
    limit: usize,
    window: u64,
) -> Result<Vec<u64>> {
    ensure!(window > 0, "the changelist window must not be empty");
    let latest = source.latest_change().await?;
    let mut changes = Vec::new();
    let mut from = after.unwrap_or(0);
    while changes.len() < limit && from < latest {
        let upto = from.saturating_add(window).min(latest);
        changes.extend(source.submitted_changes(depot_root, from, upto).await?);
        from = upto;
    }
    changes.truncate(limit);
    Ok(changes)
}

/// The bookmark's changeset and the changelist it was imported from, or `None` before
/// the first import.
async fn head(
    ctx: &CoreContext,
    repo: &(impl BookmarksRef + RepoBlobstoreRef),
    bookmark: &BookmarkKey,
) -> Result<Option<(ChangesetId, u64)>> {
    let Some(cs_id) = repo
        .bookmarks()
        .get(ctx.clone(), bookmark, Freshness::MostRecent)
        .await?
    else {
        return Ok(None);
    };
    let changeset = cs_id.load(ctx, repo.repo_blobstore()).await?;
    Ok(Some((cs_id, P4ChangelistId::from_bcs(&changeset)?.id())))
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

/// Imports `changes`, in order, onto `bookmark` (created by the first import), deriving
/// the types in `derive` that the repo has enabled, and returns their changesets. The
/// batch must start after the `resume_point`, so nothing is imported twice.
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
         + BonsaiP4MappingArc
         + CommitGraphRef
         + CommitGraphWriterRef
         + FilestoreConfigRef
         + RepoBlobstoreRef
         + RepoDerivedDataRef
         + RepoIdentityRef
         + Send
         + Sync
     ),
    source: &impl ChangeSource,
    depot_root: &str,
    bookmark: &BookmarkKey,
    changes: &[u64],
    derive: &[DerivableType],
) -> Result<Vec<ImportedChangelist>> {
    let Some(&last) = changes.last() else {
        return Ok(Vec::new());
    };
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
    let old = head.map(|(cs_id, _)| cs_id);
    let built = build_changesets(ctx, repo, source, depot_root, old, changes).await?;
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
            };
            (imported, built.changeset)
        })
        .unzip();
    let derive = BackfillDerivation::OnlySpecificTypes(derive.to_vec())
        .types(&repo.repo_derived_data().active_config().types);
    save_and_derive_batch(ctx, repo, changesets, &derive).await?;

    let new = imported.last().context("built no changesets")?.cs_id;
    let mut txn = repo.bookmarks().create_transaction(ctx.clone());
    match old {
        None => txn.create(bookmark, new, BookmarkUpdateReason::Blobimport)?,
        Some(old) => txn.update(bookmark, new, old, BookmarkUpdateReason::Blobimport)?,
    }
    let mapping = mapping_txn_hook(repo.bonsai_p4_mapping_arc(), &imported);
    // Hooks run before the bookmark's compare-and-swap, so a concurrent import of the same
    // changelists fails as a mapping `Conflict` rather than here. Both roll back everything.
    ensure!(
        txn.commit_with_hooks(vec![mapping]).await?.is_some(),
        "{bookmark} moved while importing changelists {}..={last}",
        changes[0]
    );
    Ok(imported)
}

/// Imports the first `limit` changelists of the depot after the bookmark's `resume_point`,
/// as `import_changelists` does.
pub async fn import_next(
    ctx: &CoreContext,
    repo: &(
         impl BookmarksRef
         + BonsaiP4MappingArc
         + CommitGraphRef
         + CommitGraphWriterRef
         + FilestoreConfigRef
         + RepoBlobstoreRef
         + RepoDerivedDataRef
         + RepoIdentityRef
         + Send
         + Sync
     ),
    source: &impl ChangeSource,
    depot_root: &str,
    bookmark: &BookmarkKey,
    limit: usize,
    derive: &[DerivableType],
) -> Result<Vec<ImportedChangelist>> {
    let after = resume_point(ctx, repo, bookmark).await?;
    let changes = next_changes(source, depot_root, after, limit, CHANGES_WINDOW).await?;
    import_changelists(ctx, repo, source, depot_root, bookmark, &changes, derive).await
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
