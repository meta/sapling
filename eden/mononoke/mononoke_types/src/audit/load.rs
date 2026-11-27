/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Tags that tie each storage read to the object it was for.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::hash::Hash;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use blobstore::KeyedBlobstore;
use context::CoreContext;
use context::SamplingKey;

use crate::ChangesetId;
use crate::audit::ObjectId;

/// What one load was for.
#[derive(Clone, Debug)]
pub struct LoadTag {
    pub object: ObjectId,
    /// The commit whose audit made the load, a parent's load included.
    pub changeset_id: Option<ChangesetId>,
}

/// One storage read, attributed to the load in progress that made it.
#[derive(Clone, Debug)]
pub struct TaggedRead {
    pub tag: LoadTag,
    /// Whether the store returned the key and no earlier read by this load,
    /// from any store, had. A multiplexed load can read one key from several
    /// stores.
    pub first_found: bool,
}

/// A load in progress: its tag and the blobstore keys it has found so far.
#[derive(Debug)]
struct LiveTag {
    tag: LoadTag,
    found_keys: HashSet<String>,
}

/// The tags of the loads in progress, by the sampling key their context carries.
#[derive(Debug, Default)]
pub struct LoadTags {
    tags: Mutex<HashMap<SamplingKey, LiveTag>>,
}

impl LoadTags {
    /// The tag of the load whose context carries `key`, while that load runs.
    pub fn get(&self, key: &SamplingKey) -> Option<LoadTag> {
        self.lock().get(key).map(|live| live.tag.clone())
    }

    /// Attributes a read of `blobstore_key` to the load whose context carries
    /// `key`, while that load runs; `found` says whether the store returned
    /// the key. The keys a load found are kept until its tag is removed.
    pub fn record_read(
        &self,
        key: &SamplingKey,
        blobstore_key: &str,
        found: bool,
    ) -> Option<TaggedRead> {
        let mut tags = self.lock();
        let live = tags.get_mut(key)?;
        let first_found = found && live.found_keys.insert(blobstore_key.to_owned());
        Some(TaggedRead {
            tag: live.tag.clone(),
            first_found,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    // Each operation leaves the map consistent, so a panic elsewhere while
    // holding the lock cannot corrupt it.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SamplingKey, LiveTag>> {
        self.tags.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The context one commit's loads run under: the store they read and the
/// tags that attribute each read.
///
/// Every storage load, including a parent's, takes its context from
/// [`LoadContext::tag`]. Only a process's first read of a key reaches storage,
/// so an untagged load could leave that read unattributed for every commit.
#[derive(Clone)]
pub struct LoadContext {
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    tags: Arc<LoadTags>,
    changeset_id: Option<ChangesetId>,
}

impl LoadContext {
    pub fn new(ctx: CoreContext, blobstore: Arc<dyn KeyedBlobstore>, tags: Arc<LoadTags>) -> Self {
        Self {
            ctx,
            blobstore,
            tags,
            changeset_id: None,
        }
    }

    /// The same context, whose tags name the commit being audited.
    pub fn with_changeset_id(self, changeset_id: ChangesetId) -> Self {
        Self {
            changeset_id: Some(changeset_id),
            ..self
        }
    }

    pub fn blobstore(&self) -> &Arc<dyn KeyedBlobstore> {
        &self.blobstore
    }

    /// A context whose reads, including those inside library code, carry a
    /// tag for `object` until it is dropped.
    pub fn tag(&self, object: ObjectId) -> TaggedContext {
        let key = insert_with_free_key(
            &mut self.tags.lock(),
            LiveTag {
                tag: LoadTag {
                    object,
                    changeset_id: self.changeset_id,
                },
                found_keys: HashSet::new(),
            },
            SamplingKey::new,
        );
        TaggedContext {
            ctx: self.ctx.clone_and_sample(key),
            blobstore: self.blobstore.clone(),
            tags: self.tags.clone(),
            key,
        }
    }
}

/// A context carrying one load's tag. Dropping it removes the tag, whether the
/// load finished, failed or was abandoned: sampling keys wrap, so a tag left
/// behind would eventually be read as a later load's. No two live contexts
/// share a key, so dropping one removes only its own tag.
pub struct TaggedContext {
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    tags: Arc<LoadTags>,
    key: SamplingKey,
}

impl TaggedContext {
    pub fn blobstore(&self) -> &Arc<dyn KeyedBlobstore> {
        &self.blobstore
    }
}

impl Deref for TaggedContext {
    type Target = CoreContext;

    fn deref(&self) -> &CoreContext {
        &self.ctx
    }
}

impl Drop for TaggedContext {
    fn drop(&mut self) {
        self.tags.lock().remove(&self.key);
    }
}

/// Inserts `value` under the first key `next_key` returns that no entry holds.
/// Sampling keys wrap after 2^32, so a new key can be one a running load still
/// holds; taking the next one keeps that load's tag.
fn insert_with_free_key<K: Copy + Eq + Hash, V>(
    map: &mut HashMap<K, V>,
    value: V,
    mut next_key: impl FnMut() -> K,
) -> K {
    loop {
        let key = next_key();
        if let Entry::Vacant(entry) = map.entry(key) {
            entry.insert(value);
            return key;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use mononoke_macros::mononoke;

    use super::insert_with_free_key;

    #[mononoke::test]
    fn a_key_in_use_is_skipped() {
        // Given a tag under key 0, and keys that have wrapped back to 0
        let mut map = HashMap::from([(0u32, "running")]);
        let mut keys = [0, 1].into_iter();

        // When another tag is inserted
        let key = insert_with_free_key(&mut map, "new", || keys.next().expect("a free key"));

        // Then it takes the next free key and the running tag is kept
        assert_eq!(key, 1);
        assert_eq!(map, HashMap::from([(0, "running"), (1, "new")]));
    }
}
