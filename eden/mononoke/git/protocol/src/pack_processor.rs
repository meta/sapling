/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Write;
use std::mem::size_of;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use bytes::Bytes;
use context::CoreContext;
use data::entry::Header::RefDelta;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use futures_stats::TimedTryFutureExt;
use git_types::GitIdentifier;
use git_types::HeaderState;
use git_types::ObjectContent;
use git_types::fetch_git_object_bytes;
use gix_features::progress::Discard;
use gix_hash::Kind;
use gix_hash::ObjectId;
use gix_hash::oid;
use gix_object::WriteTo;
use gix_object::encode::loose_header;
use gix_pack::cache::Never;
use gix_pack::data;
use gix_pack::data::File;
use gix_pack::data::decode::Error as PackError;
use gix_pack::data::decode::entry::ResolvedBase;
use gix_pack::data::input::Entry as InputEntry;
use gix_pack::data::input::Error as InputError;
use mononoke_macros::mononoke;
use mononoke_types::hash::GitSha1;
use rayon::iter::IntoParallelIterator;
use rayon::iter::ParallelIterator;
use repo_blobstore::RepoBlobstore;
use rustc_hash::FxHashMap;
use scuba_ext::FutureStatsScubaExt;
use sha1::Digest;
use sha1::Sha1;
use tempfile::Builder;
use weight_observer::WeightGuard;
use weight_observer::WeightObserver;

use crate::PACKFILE_SUFFIX;

const MAX_ALLOWED_DEPTH: u8 = 30;
const MAX_CACHED_PACK_ENTRIES_BYTES: usize = 8 * 1024 * 1024;
type ObjectMap = HashMap<ObjectId, ObjectContent>;

struct DecodeWeight {
    observer: Option<Arc<dyn WeightObserver>>,
    weight: AtomicUsize,
}

impl DecodeWeight {
    fn new(observer: Option<Arc<dyn WeightObserver>>) -> Self {
        Self {
            observer,
            weight: AtomicUsize::new(0),
        }
    }

    fn add(&self, weight: usize) {
        if let Some(observer) = &self.observer {
            self.weight.fetch_add(weight, Ordering::Relaxed);
            observer.on_weight_added(weight);
        }
    }

    fn into_guard(mut self) -> WeightGuard {
        WeightGuard {
            observer: self.observer.take(),
            weight: self.weight.load(Ordering::Relaxed),
        }
    }
}

impl Drop for DecodeWeight {
    fn drop(&mut self) {
        if let Some(observer) = &self.observer {
            let weight = self.weight.load(Ordering::Relaxed);
            if weight > 0 {
                observer.on_weight_removed(weight);
            }
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum PackEntry {
    Pending(InputEntry),
    Processed((ObjectId, ObjectContent)),
}

#[derive(Debug, Clone, Default)]
struct PackEntries {
    entries: HashSet<PackEntry>,
}

impl PackEntries {
    fn from_pending_and_processed(pending: Vec<InputEntry>, processed: ObjectMap) -> Self {
        let mut entries = HashSet::new();
        for entry in pending {
            entries.insert(PackEntry::Pending(entry));
        }
        for (id, object_content) in processed {
            entries.insert(PackEntry::Processed((id, object_content)));
        }
        Self { entries }
    }

    fn from_entries(entries: HashSet<PackEntry>) -> Self {
        Self { entries }
    }

    fn into_pending_and_processed(self) -> (Vec<InputEntry>, ObjectMap) {
        let mut pending = Vec::new();
        let mut processed = HashMap::new();
        for entry in self.entries {
            match entry {
                PackEntry::Pending(entry) => pending.push(entry),
                PackEntry::Processed((id, content)) => {
                    processed.insert(id, content);
                }
            }
        }
        (pending, processed)
    }

    fn into_processed(self) -> Result<FxHashMap<ObjectId, ObjectContent>> {
        let mut object_map = FxHashMap::default();
        for entry in self.entries {
            match entry {
                PackEntry::Processed((id, content)) => {
                    object_map.insert(id, content);
                }
                _ => anyhow::bail!("Packfile entries are not completely processed"),
            }
        }
        Ok(object_map)
    }

    fn is_processed(&self) -> bool {
        self.entries.iter().all(|entry| match entry {
            PackEntry::Processed(_) => true,
            _ => false,
        })
    }
}

fn into_data_entry(pack_entry: InputEntry) -> data::Entry {
    data::Entry {
        header: pack_entry.header,
        decompressed_size: pack_entry.decompressed_size,
        data_offset: pack_entry.pack_offset + pack_entry.header_size as u64,
        encoded_header_size: pack_entry.header_size,
    }
}

/// Generates the full bytes of a git object including its header
fn git_object_bytes(
    headerless_object_bytes: Vec<u8>,
    kind: gix_object::Kind,
    size: usize,
) -> Vec<u8> {
    let mut object_bytes = loose_header(kind, size as u64).into_vec();
    object_bytes.extend(headerless_object_bytes);
    object_bytes
}

fn resolve_delta(
    oid: &gix_hash::oid,
    out: &mut Vec<u8>,
    known_objects: &ObjectMap,
) -> Option<ResolvedBase> {
    known_objects.get(oid).map(|object_content| {
        object_content
            .with_parsed(|parsed| parsed.write_to(out.by_ref()))
            .unwrap();
        ResolvedBase::OutOfPack {
            kind: object_content.with_parsed(|parsed| parsed.kind()).clone(),
            end: out.len(),
        }
    })
}

fn new_entry_cache(entry_count: usize) -> Option<Vec<InputEntry>> {
    let max_entries = MAX_CACHED_PACK_ENTRIES_BYTES
        .saturating_sub(size_of::<Option<Vec<InputEntry>>>())
        / size_of::<InputEntry>();
    if entry_count > max_entries {
        return None;
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(entry_count.min(max_entries))
        .ok()?;
    (entries.capacity() <= max_entries).then_some(entries)
}

async fn load_entries_and_prereq_objects(
    pack_file: &data::File,
    ctx: &CoreContext,
    blobstore: Arc<RepoBlobstore>,
    concurrency: usize,
) -> Result<(Option<Vec<InputEntry>>, ObjectMap)> {
    let mut base_items = HashSet::new();
    let pack_stream = pack_file
        .streaming_iter()
        .context("Failure in iterating packfile")?;
    let mut pending_entries = new_entry_cache(pack_stream.size_hint().0);
    for entry in pack_stream {
        let mut entry = entry.context("Invalid packfile entry")?;
        if let RefDelta { base_id } = entry.header {
            base_items.insert(base_id);
        }
        entry.compressed = None;
        if let Some(entries) = &mut pending_entries {
            if entries.len() < entries.capacity() {
                entries.push(entry);
            } else {
                pending_entries = None;
            }
        }
    }
    let prereq_objects = stream::iter(base_items)
        .map(async |object_id| {
            let git_identifier = GitIdentifier::Basic(GitSha1::from_object_id(object_id.as_ref())?);
            anyhow::Ok(
                fetch_git_object_bytes(
                    ctx,
                    blobstore.clone(),
                    &git_identifier,
                    HeaderState::Included,
                )
                .await
                .ok()
                .and_then(|git_bytes| {
                    ObjectContent::try_from_loose(git_bytes)
                        .ok()
                        .map(|content| (object_id, content))
                }),
            )
        })
        .buffer_unordered(concurrency)
        .try_filter_map(|maybe_object| future::ready(Ok(maybe_object)))
        .try_collect::<HashMap<_, _>>()
        .await?;
    Ok((pending_entries, prereq_objects))
}

/// Method responsible for parsing the packfile provided as part of push, verifying its correctness
/// and returning an object map containing all the objects present in packfile mapped to their IDs
/// Returns the parsed objects and their memory-accounting guard. Keep the guard
/// alive for as long as the objects are held in memory.
pub async fn parse_pack(
    pack_bytes: &[u8],
    ctx: &CoreContext,
    blobstore: Arc<RepoBlobstore>,
    concurrency: usize,
    weight_observer: Option<Arc<dyn WeightObserver>>,
) -> Result<(FxHashMap<ObjectId, ObjectContent>, WeightGuard)> {
    // If the packfile is empty, return an empty object map. This can happen when the push only has ref create/update
    // pointing to existing commit or just ref deletes
    if pack_bytes.is_empty() {
        return Ok((
            FxHashMap::default(),
            DecodeWeight::new(weight_observer).into_guard(),
        ));
    }
    let mut raw_file = Builder::new()
        .suffix(PACKFILE_SUFFIX)
        .rand_bytes(8)
        .tempfile()?;
    raw_file.write_all(pack_bytes)?;
    raw_file.flush()?;
    let response = parse_stored_pack(
        raw_file.path(),
        ctx,
        blobstore,
        concurrency,
        weight_observer,
    )
    .await;
    raw_file.close().unwrap_or_default();
    response
}

fn process_pack_entries(
    pack_file: &data::File,
    entries: PackEntries,
    weight: &DecodeWeight,
) -> Result<PackEntries> {
    let (pending_entries, prereq_objects) = entries.into_pending_and_processed();
    let output_entries = pending_entries
        .into_par_iter()
        .map(|entry| {
            let mut output = vec![];
            let err_context = format!("Error in decoding packfile entry: {:?}", entry.header);
            let outcome = pack_file.decode_entry(
                into_data_entry(entry.clone()),
                &mut output,
                &mut Default::default(),
                &|oid, out| resolve_delta(oid, out, &prereq_objects),
                &mut Never,
            );
            match outcome {
                Ok(outcome) => {
                    let object_size = outcome.object_size as usize;
                    // Track weight as objects are decoded — uses the object_size
                    // already computed by decode_entry, zero extra cost.
                    weight.add(object_size);
                    let object_bytes =
                        Bytes::from(git_object_bytes(output, outcome.kind, object_size));
                    let mut hasher = Sha1::new();
                    hasher.update(&object_bytes);
                    let hash_bytes = hasher.finalize();
                    // Create the Git object from raw bytes
                    let id = oid::try_from_bytes(hash_bytes.as_ref())
                        .context("Failed to convert packfile item hash to Git Object ID")?
                        .into();
                    let object = ObjectContent::try_from_loose(object_bytes)?;
                    let processed_entry = PackEntry::Processed((id, object));
                    anyhow::Ok(processed_entry)
                }
                Err(e) => match e {
                    PackError::DeltaBaseUnresolved(_) => anyhow::Ok(PackEntry::Pending(entry)),
                    _ => Err(e).context(err_context),
                },
            }
        })
        .collect::<Result<HashSet<PackEntry>>>()
        .context("Failure in decoding packfile entries")?;
    let output_entries = prereq_objects
        .into_iter()
        .map(|(id, object_content)| PackEntry::Processed((id, object_content)))
        .chain(output_entries)
        .collect();
    Ok(PackEntries::from_entries(output_entries))
}

async fn parse_stored_pack(
    pack_path: &Path,
    ctx: &CoreContext,
    blobstore: Arc<RepoBlobstore>,
    concurrency: usize,
    weight_observer: Option<Arc<dyn WeightObserver>>,
) -> Result<(FxHashMap<ObjectId, ObjectContent>, WeightGuard)> {
    let pack_file = Arc::new(File::at(pack_path, Kind::Sha1).with_context(|| {
        format!(
            "Error while opening packfile for push at {}",
            pack_path.display()
        )
    })?);
    // Verify that the packfile is valid
    mononoke::spawn_blocking({
        let pack_file = pack_file.clone();
        move || {
            pack_file
                .verify_checksum(&mut Discard, &AtomicBool::new(false))
                .context("The checksum of the packfile is invalid")
        }
    })
    .try_timed()
    .await?
    .log_future_stats(
        ctx.scuba().clone(),
        "Verified Packfile Checksum",
        "Push".to_string(),
    )?;

    let (cached_entries, prereq_objects) =
        load_entries_and_prereq_objects(&pack_file, ctx, blobstore.clone(), concurrency)
            .try_timed()
            .await?
            .log_future_stats(
                ctx.scuba().clone(),
                "Fetched Prerequisite Objects",
                "Push".to_string(),
            );
    let pending_entries = match cached_entries {
        Some(entries) => entries,
        None => pack_file
            .streaming_iter()
            .context("Failure in iterating packfile")?
            .collect::<Result<Vec<_>, InputError>>()?,
    };

    // Process all the entries. Weight is tracked inline inside
    // process_pack_entries using outcome.object_size from decode_entry —
    // zero extra passes or overhead beyond the decoding itself.
    mononoke::spawn_blocking({
        let pack_file = pack_file.clone();
        move || {
            let mut pack_entries =
                PackEntries::from_pending_and_processed(pending_entries, prereq_objects);
            let weight = DecodeWeight::new(weight_observer);
            let mut counter = 0;
            while !pack_entries.is_processed() {
                if counter > MAX_ALLOWED_DEPTH {
                    anyhow::bail!(
                        "Maximum allowed depth reached while processing packfile entries"
                    );
                }
                counter += 1;
                pack_entries = process_pack_entries(&pack_file, pack_entries, &weight)?;
            }
            Ok((pack_entries.into_processed()?, weight.into_guard()))
        }
    })
    .try_timed()
    .await?
    .log_future_stats(
        ctx.scuba().clone(),
        "Decoded objects from Packfile",
        "Push".to_string(),
    )
}

#[cfg(test)]
mod tests;
