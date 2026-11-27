/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Accounting for the reads that reach storage, by the type of object each
//! read was for.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Result;
use blobstore::BlobstoreGetData;
use context::CoreContext;
use metaconfig_types::BlobstoreId;
use mononoke_types::audit::LoadTags;
use mononoke_types::audit::ObjectType;
use samplingblob::ComponentSamplingHandler;

/// The keys and bytes read for one object type.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReadTotals {
    /// Keys found in storage, each counted once per load that read it,
    /// whichever stores it was read from.
    pub keys: u64,
    /// The uncompressed size of each counted key, as Walker counts it.
    pub bytes: u64,
}

/// Sees each store's reads, below the cache, and attributes each one to the
/// load whose tag its context carries.
///
/// As in Walker, only a read that reaches storage is seen: with the
/// process-local cache, a key is normally read once per process.
#[derive(Debug)]
pub struct ReadSampler {
    tags: Arc<LoadTags>,
    totals: Mutex<BTreeMap<ObjectType, ReadTotals>>,
    /// Reads whose context carries a sampling key but no live tag: the
    /// runner's own context, or a load's after its tag was dropped. Every
    /// load is tagged, so any is a bug.
    untagged: AtomicU64,
    /// Reads whose context carries no sampling key, which no audit context
    /// lacks: opening the repository, or loading its redaction configuration.
    outside_audit: AtomicU64,
    print_reads: bool,
}

impl ReadSampler {
    /// Reads are attributed to the tags in `tags`. With `print_reads`, each
    /// read is also printed to stdout.
    pub fn new(tags: Arc<LoadTags>, print_reads: bool) -> Self {
        Self {
            tags,
            totals: Mutex::new(BTreeMap::new()),
            untagged: AtomicU64::new(0),
            outside_audit: AtomicU64::new(0),
            print_reads,
        }
    }

    /// The totals so far, by object type.
    pub fn totals(&self) -> BTreeMap<ObjectType, ReadTotals> {
        self.lock_totals().clone()
    }

    pub fn untagged_reads(&self) -> u64 {
        self.untagged.load(Ordering::Relaxed)
    }

    pub fn reads_outside_audit(&self) -> u64 {
        self.outside_audit.load(Ordering::Relaxed)
    }

    // Each update leaves the map consistent, so a panic elsewhere while
    // holding the lock cannot corrupt it.
    fn lock_totals(&self) -> std::sync::MutexGuard<'_, BTreeMap<ObjectType, ReadTotals>> {
        self.totals.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ComponentSamplingHandler for ReadSampler {
    fn sample_get(
        &self,
        ctx: &CoreContext,
        key: &str,
        value: Option<&BlobstoreGetData>,
        inner_id: Option<BlobstoreId>,
    ) -> Result<()> {
        let read = match ctx.sampling_key() {
            None => {
                self.outside_audit.fetch_add(1, Ordering::Relaxed);
                None
            }
            Some(sampling_key) => {
                let read = self.tags.record_read(sampling_key, key, value.is_some());
                if read.is_none() {
                    self.untagged.fetch_add(1, Ordering::Relaxed);
                }
                read
            }
        };
        if self.print_reads {
            let store = inner_id.map_or_else(|| "-".to_string(), |id| id.to_string());
            let counted_as = match (&read, ctx.sampling_key()) {
                (Some(read), _) => read.tag.object.object_type().to_string(),
                (None, Some(_)) => "untagged".to_string(),
                (None, None) => "outside".to_string(),
            };
            let size = value.map_or_else(|| "absent".to_string(), |value| value.len().to_string());
            println!("read {store} {counted_as} {key} {size}");
        }
        if let Some(read) = read
            && let Some(value) = value
            && read.first_found
        {
            let mut totals = self.lock_totals();
            let totals = totals.entry(read.tag.object.object_type()).or_default();
            totals.keys += 1;
            totals.bytes += value.len() as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use context::SamplingKey;
    use fbinit::FacebookInit;
    use memblob::KeyedMemblob;
    use mononoke_macros::mononoke;
    use mononoke_types::ChangesetId;
    use mononoke_types::audit::LoadContext;
    use mononoke_types::audit::ObjectId;
    use mononoke_types::hash::Blake2;
    use tests_utils::BasicTestRepo;

    use super::*;

    fn changeset(byte: u8) -> ObjectId {
        ObjectId::of::<BasicTestRepo, _>(&ChangesetId::new(Blake2::from_byte_array([byte; 32])))
    }

    fn data(len: usize) -> BlobstoreGetData {
        BlobstoreGetData::from_bytes(vec![0; len])
    }

    fn load_ctx(ctx: &CoreContext, tags: &Arc<LoadTags>) -> LoadContext {
        LoadContext::new(ctx.clone(), Arc::new(KeyedMemblob::default()), tags.clone())
    }

    #[mononoke::fbinit_test]
    async fn each_load_counts_a_key_once_whichever_stores_return_it(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a sampler over a registry, and two loads of changesets
        let ctx = CoreContext::test_mock(fb);
        let tags = Arc::new(LoadTags::default());
        let sampler = ReadSampler::new(tags.clone(), false);
        let first = load_ctx(&ctx, &tags).tag(changeset(1));
        let second = load_ctx(&ctx, &tags).tag(changeset(2));
        let store = |id| Some(BlobstoreId::new(id));

        // When the first load reads one key from two stores, and another key
        // the stores lack; and the second load reads the first key from a
        // store that lacks it, then from two that have it
        sampler.sample_get(&first, "a", Some(&data(10)), store(1))?;
        sampler.sample_get(&first, "a", Some(&data(10)), store(2))?;
        sampler.sample_get(&first, "absent", None, store(1))?;
        sampler.sample_get(&second, "a", None, store(1))?;
        sampler.sample_get(&second, "a", Some(&data(10)), store(2))?;
        sampler.sample_get(&second, "a", Some(&data(10)), store(3))?;

        // Then the key counts once for each load that found it, and the
        // absent key not at all
        assert_eq!(
            sampler.totals(),
            BTreeMap::from([(
                changeset(1).object_type(),
                ReadTotals { keys: 2, bytes: 20 }
            )])
        );
        assert_eq!(sampler.untagged_reads(), 0);
        assert_eq!(sampler.reads_outside_audit(), 0);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn audit_reads_without_a_live_tag_are_untagged(fb: FacebookInit) -> Result<()> {
        // Given a context whose load tag was dropped, and an audit context
        // that carries a sampling key but was never tagged
        let ctx = CoreContext::test_mock(fb);
        let tags = Arc::new(LoadTags::default());
        let sampler = ReadSampler::new(tags.clone(), false);
        let tagged = load_ctx(&ctx, &tags).tag(changeset(1));
        let dropped: CoreContext = (*tagged).clone();
        drop(tagged);
        let audit = ctx.clone_and_sample(SamplingKey::new());

        // When each reads a key, present or absent
        sampler.sample_get(&dropped, "a", Some(&data(10)), None)?;
        sampler.sample_get(&audit, "b", None, None)?;

        // Then both are untagged, and neither is in the totals
        assert_eq!(sampler.untagged_reads(), 2);
        assert_eq!(sampler.reads_outside_audit(), 0);
        assert_eq!(sampler.totals(), BTreeMap::new());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn reads_without_a_sampling_key_are_outside_the_audit(fb: FacebookInit) -> Result<()> {
        // Given a context with no sampling key, as when opening the repository
        let ctx = CoreContext::test_mock(fb);
        let sampler = ReadSampler::new(Arc::new(LoadTags::default()), false);

        // When it reads a key
        sampler.sample_get(&ctx, "redaction-keylist", Some(&data(10)), None)?;

        // Then the read is outside the audit: neither untagged nor counted
        assert_eq!(sampler.reads_outside_audit(), 1);
        assert_eq!(sampler.untagged_reads(), 0);
        assert_eq!(sampler.totals(), BTreeMap::new());
        Ok(())
    }
}
