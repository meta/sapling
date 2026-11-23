/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Reuse of stored content by MD5 and size. A record in the repo blobstore maps the MD5 and
//! size describe gives for a revision to the content id stored for it, so a later copy of
//! that content can skip `p4 print`.

use std::collections::HashMap;
use std::fmt;
use std::hash::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use blobstore::BlobstoreBytes;
use blobstore::BlobstoreGetData;
use blobstore::KeyedBlobstore;
use context::CoreContext;
use filestore::FetchKey;
use filestore::FilestoreConfigRef;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use futures_stats::TimedFutureExt;
use md5::Digest;
use md5::Md5;
use mononoke_types::ContentId;
use mononoke_types::ContentMetadataV2;
use mononoke_types::FileType;
use p4_types::Action;
use p4_types::ChangeSource;
use p4_types::Changelist;
use p4_types::FileRevision;
use p4_types::Md5Digest;
use repo_blobstore::RepoBlobstoreRef;
use stats::prelude::*;
use tokio::sync::Mutex;

use crate::STATS;
use crate::in_depot;
use crate::upload_file_content;

/// Whether the import skips `p4 print` for content it has already stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContentReuse {
    /// Prints every revision and keeps no records.
    #[default]
    Off,
    /// Looks records up and writes them, but prints every revision, and counts the
    /// revisions `On` would not have printed.
    Verify,
    /// Does not print a branch, move/add or integrate whose MD5 and size have a record of
    /// stored content of that size, unless the MD5 and size have collided before.
    On,
}

/// What content reuse did for one changelist.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContentReuseReport {
    /// Revisions not printed: their content was found by MD5 and size (`ContentReuse::On`).
    pub reused_files: usize,
    pub reused_bytes: u64,
    /// Revisions printed that `ContentReuse::On` would not have printed, and whose printed
    /// content is the content their record names (`ContentReuse::Verify`).
    pub reusable_files: usize,
    pub reusable_bytes: u64,
    /// The record lookups that decide whether a revision is reused, summed over the
    /// revisions looked up at once.
    pub lookup: Duration,
}

/// `ContentReuse` other than `Off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Verify,
    On,
}

/// A changelist's stored contents, keyed by depot path, and how they were stored.
#[derive(Debug, Default)]
pub(crate) struct Uploaded {
    pub(crate) contents: HashMap<String, ContentMetadataV2>,
    /// Revisions printed and stored.
    pub(crate) printed_files: usize,
    pub(crate) report: ContentReuseReport,
    /// The revisions not printed, for `reprint_collided`.
    pub(crate) reused: Vec<FileRevision>,
}

impl Uploaded {
    fn add(&mut self, revision: FileRevision, stored: StoredRevision) {
        let bytes = stored.metadata.total_size;
        self.report.lookup += stored.lookup;
        self.contents
            .insert(revision.depot_path.clone(), stored.metadata);
        match stored.stored {
            Stored::Printed => self.printed_files += 1,
            Stored::PrintedReusable => {
                self.printed_files += 1;
                self.report.reusable_files += 1;
                self.report.reusable_bytes += bytes;
            }
            Stored::Reused => {
                self.report.reused_files += 1;
                self.report.reused_bytes += bytes;
                self.reused.push(revision);
            }
        }
    }
}

const KEY_PREFIX: &str = "p4.content_by_md5.v1";

/// What a record is keyed by: the MD5 and size describe gives for the bytes `p4 print`
/// sends.
#[derive(Clone, Copy, Debug)]
struct Identity {
    digest: Md5Digest,
    size: u64,
}

impl Identity {
    /// `None` for a revision whose stored bytes need not be the bytes its digest covers (a
    /// symlink, stored without its trailing newline, and a type that expands keywords), and
    /// for one without a digest or size.
    fn of(revision: &FileRevision) -> Option<Self> {
        if p4_transform::file_type(&revision.file_type) == FileType::Symlink
            || expands_keywords(&revision.file_type)
        {
            return None;
        }
        Some(Self {
            digest: revision.digest?,
            size: revision.size?,
        })
    }

    fn record_key(&self) -> String {
        format!("{KEY_PREFIX}.{}.{}", self.digest, self.size)
    }

    fn collision_key(&self) -> String {
        format!("{KEY_PREFIX}.collision.{}.{}", self.digest, self.size)
    }
}

/// `ktext`, `kxtext`, or a `k` among the modifiers (`text+k`, `text+ko`).
fn expands_keywords(file_type: &str) -> bool {
    let (base, modifiers) = file_type.split_once('+').unwrap_or((file_type, ""));
    matches!(base, "ktext" | "kxtext") || modifiers.contains('k')
}

/// Copies, whose content is usually stored already. Every other action prints.
fn may_reuse(action: Action) -> bool {
    matches!(action, Action::Branch | Action::MoveAdd | Action::Integrate)
}

/// A record's value: the content id stored for its MD5 and size, or whatever else is there.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Record {
    Content(ContentId),
    Unparsable(String),
}

impl Record {
    fn parse(data: BlobstoreGetData) -> Self {
        let bytes = data.into_raw_bytes();
        match std::str::from_utf8(&bytes).map(ContentId::from_str) {
            Ok(Ok(id)) => Record::Content(id),
            _ => Record::Unparsable(String::from_utf8_lossy(&bytes).into_owned()),
        }
    }
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Record::Content(id) => write!(f, "{id}"),
            Record::Unparsable(text) => write!(f, "unparsable record {text:?}"),
        }
    }
}

/// The stored content `identity`'s record names, unless a collision marker exists, or the
/// record is missing or unparsable, or names content that is not stored or is of another
/// size.
async fn look_up(
    ctx: &CoreContext,
    repo: &impl RepoBlobstoreRef,
    identity: Identity,
) -> Result<Option<ContentMetadataV2>> {
    let blobstore = repo.repo_blobstore();
    let (record, marker) = future::try_join(
        blobstore.get(ctx, &identity.record_key()),
        blobstore.get(ctx, &identity.collision_key()),
    )
    .await?;
    match (record.map(Record::parse), marker) {
        (Some(Record::Content(id)), None) => {
            Ok(
                filestore::get_metadata(blobstore, ctx, &FetchKey::Canonical(id))
                    .await?
                    .filter(|metadata| metadata.total_size == identity.size),
            )
        }
        _ => Ok(None),
    }
}

/// How a revision's content was stored.
#[derive(Debug, PartialEq, Eq)]
enum Stored {
    Printed,
    /// Printed, but `ContentReuse::On` would have reused the same content.
    PrintedReusable,
    Reused,
}

struct StoredRevision {
    metadata: ContentMetadataV2,
    stored: Stored,
    lookup: Duration,
}

/// Like `upload_changelist_contents`, but each revision that `mode` allows to be reused is
/// looked up by MD5 and size first, and the content of each revision printed is recorded
/// under its MD5 and size if its own MD5 is describe's. Adds one to `progress` per file
/// stored or reused.
pub(crate) async fn upload_changelist_contents_reusing(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + FilestoreConfigRef),
    source: &impl ChangeSource,
    depot_root: &str,
    changelist: &Changelist,
    concurrency: usize,
    progress: &AtomicU64,
    mode: Mode,
) -> Result<Uploaded> {
    let revisions = in_depot(depot_root, changelist, |revision| {
        revision.action.has_content()
    })?;
    // Owned items, as in `upload_changelist_contents`.
    stream::iter(revisions.into_iter().cloned())
        .map(|revision| async move {
            let stored = store_revision(ctx, repo, source, &revision, mode).await?;
            anyhow::Ok((revision, stored))
        })
        .buffer_unordered(concurrency)
        .inspect_ok(|_| {
            progress.fetch_add(1, Ordering::Relaxed);
        })
        .try_fold(
            Uploaded::default(),
            |mut uploaded, (revision, stored)| async move {
                uploaded.add(revision, stored);
                Ok(uploaded)
            },
        )
        .await
}

/// Prints each of `reused` whose MD5 and size have a collision marker by now, or whose
/// marker cannot be read, and returns what was stored for them by depot path. A changelist
/// is looked up while earlier ones are still printing, so before one of those could find a
/// collision: this is for when every earlier changelist has been fetched.
pub(crate) async fn reprint_collided(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + FilestoreConfigRef),
    source: &impl ChangeSource,
    reused: Vec<FileRevision>,
    concurrency: usize,
) -> Result<Vec<(String, ContentMetadataV2)>> {
    stream::iter(reused)
        .map(|revision| async move {
            let marked = match Identity::of(&revision) {
                Some(identity) => repo
                    .repo_blobstore()
                    .get(ctx, &identity.collision_key())
                    .await
                    .map_or(true, |marker| marker.is_some()),
                None => true,
            };
            if !marked {
                return anyhow::Ok(None);
            }
            // Its lookup misses, with the marker there: it prints and checks its record.
            let stored = store_revision(ctx, repo, source, &revision, Mode::Verify).await?;
            Ok(Some((revision.depot_path, stored.metadata)))
        })
        .buffer_unordered(concurrency)
        .try_filter_map(|reprinted| async move { Ok(reprinted) })
        .try_collect()
        .await
}

async fn store_revision(
    ctx: &CoreContext,
    repo: &(impl RepoBlobstoreRef + FilestoreConfigRef),
    source: &impl ChangeSource,
    revision: &FileRevision,
    mode: Mode,
) -> Result<StoredRevision> {
    let name = || format!("{}#{}", revision.depot_path, revision.revision);
    let size = revision
        .size
        .with_context(|| format!("describe gave no size for {}", name()))?;
    let identity = Identity::of(revision);
    let (lookup, reusable) = match identity.filter(|_| may_reuse(revision.action)) {
        Some(identity) => {
            let (stats, found) = look_up(ctx, repo, identity).timed().await;
            // A record that cannot be looked up is a record not found: the revision prints.
            let reusable = found.unwrap_or_else(|error| {
                STATS::content_reuse_lookup_failures.add_value(1);
                tracing::warn!(
                    "P4 content record for MD5 {} and size {} could not be looked up: {error:#}",
                    identity.digest,
                    identity.size,
                );
                None
            });
            (stats.completion_time, reusable)
        }
        None => (Duration::ZERO, None),
    };
    if mode == Mode::On
        && let Some(metadata) = reusable
    {
        return Ok(StoredRevision {
            metadata,
            stored: Stored::Reused,
            lookup,
        });
    }

    let content = source.print(revision).await?;
    let mut md5 = identity.map(|_| Md5::new());
    let content = content.inspect_ok(|chunk| {
        if let Some(md5) = &mut md5 {
            md5.update(chunk);
        }
    });
    let file_type = p4_transform::file_type(&revision.file_type);
    let metadata = upload_file_content(ctx, repo, file_type, size, content)
        .await
        .with_context(|| format!("storing {}", name()))?;
    let printed = md5.map(|md5| Md5Digest::new(md5.finalize().into()));

    if let Some(identity) = identity
        && let Some(printed) = printed
    {
        if printed == identity.digest {
            check_record(ctx, repo, revision, identity, metadata.content_id, mode)
                .await
                .with_context(|| format!("recording the content of {}", name()))?;
        } else {
            // A source that checks its prints has failed the stream before this.
            STATS::content_reuse_digest_mismatches.add_value(1);
            tracing::warn!(
                "P4 content printed with MD5 {printed} where describe gave {}; it is not recorded",
                identity.digest,
            );
        }
    }
    let stored = match reusable {
        Some(reusable) if reusable.content_id == metadata.content_id => Stored::PrintedReusable,
        _ => Stored::Printed,
    };
    Ok(StoredRevision {
        metadata,
        stored,
        lookup,
    })
}

const RECORD_LOCKS: usize = 64;

/// One of these is held while a record is read and then written, so that of two contents
/// with one MD5 and size printed at once in this process, the second finds the first's
/// record. A blobstore put is not a compare-and-set, and a caching layer can read back a
/// value the store did not keep, so a read after the put would not tell.
static RECORD_LOCK: [Mutex<()>; RECORD_LOCKS] = [const { Mutex::const_new(()) }; RECORD_LOCKS];

fn record_lock(identity: Identity) -> &'static Mutex<()> {
    let mut hasher = DefaultHasher::new();
    identity.digest.hash(&mut hasher);
    identity.size.hash(&mut hasher);
    &RECORD_LOCK[hasher.finish() as usize % RECORD_LOCKS]
}

/// Records `stored` as the content for `identity` unless a record exists, and reports a
/// record that names other content as a collision. Fails, and so fails the batch, if the
/// record cannot be read or written: content without its record would let a later content
/// with the same MD5 and size be recorded unseen.
async fn check_record(
    ctx: &CoreContext,
    repo: &impl RepoBlobstoreRef,
    revision: &FileRevision,
    identity: Identity,
    stored: ContentId,
    mode: Mode,
) -> Result<()> {
    let blobstore = repo.repo_blobstore();
    let key = identity.record_key();
    let _one_at_a_time = record_lock(identity).lock().await;
    let record = blobstore
        .get(ctx, &key)
        .await
        .context("reading the record")?
        .map(Record::parse);
    match record {
        None => {
            let value = BlobstoreBytes::from_bytes(stored.to_string());
            blobstore
                .put(ctx, key, value)
                .await
                .context("writing the record")
        }
        Some(Record::Content(id)) if id == stored => Ok(()),
        Some(record) => collision(ctx, repo, revision, identity, &record, stored, mode).await,
    }
}

/// Marks `identity` so its record is never reused, then logs that the record names other
/// content than `printed`. The printed content is what the import keeps. Fails, and so
/// fails the batch, if the marker cannot be written: the retry prints the revision again.
async fn collision(
    ctx: &CoreContext,
    repo: &impl RepoBlobstoreRef,
    revision: &FileRevision,
    identity: Identity,
    record: &Record,
    printed: ContentId,
    mode: Mode,
) -> Result<()> {
    repo.repo_blobstore()
        .put(
            ctx,
            identity.collision_key(),
            BlobstoreBytes::from_bytes(printed.to_string()),
        )
        .await
        .context("marking the collision")?;
    STATS::content_reuse_collisions.add_value(1);
    // Numbers and ids only, as in the rest of the job's log; the path goes to Scuba.
    tracing::error!(
        "P4 content digest collision: MD5 {} and size {} are recorded as {record}, but printed as content {printed}; importing the printed content",
        identity.digest,
        identity.size,
    );
    let mut scuba = ctx.scuba().clone();
    scuba
        .add("depot_path", revision.depot_path.as_str())
        .add("revision", revision.revision)
        .add("p4_action", revision.action.as_p4())
        .add("content_reuse", format!("{mode:?}").to_lowercase())
        .add("digest", identity.digest.to_string())
        .add("size", identity.size)
        .add("recorded_content_id", record.to_string())
        .add("printed_content_id", printed.to_string());
    scuba.log_with_msg("P4 content digest collision", None);
    Ok(())
}
