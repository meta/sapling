/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(never_type)]

mod cachelib_utils;
mod factory;
mod memcache_utils;
mod mock_store;

use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::Hash;
use std::time::Duration;

use anyhow::Context as _;
use anyhow::Error;
use async_trait::async_trait;
use bytes::Bytes;
use cloned::cloned;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use itertools::Itertools;
use memcache::KeyGen;
use memcache::MEMCACHE_VALUE_MAX_SIZE;
use mononoke_macros::mononoke;
use stats::prelude::*;

pub use crate::cachelib_utils::CachelibHandler;
pub use crate::factory::CacheHandlerEncoding;
pub use crate::factory::CacheHandlerFactory;
pub use crate::memcache_utils::MemcacheHandler;
pub use crate::mock_store::MockStoreStats;

define_stats_struct! {
    CacheStats("mononoke.cache.{}", label: String),

    cachelib_hit: timeseries("cachelib.hit"; Rate, Sum),
    cachelib_miss: timeseries("cachelib.miss"; Rate, Sum),

    memcache_hit: timeseries("memcache.hit"; Rate, Sum),
    memcache_miss: timeseries("memcache.miss"; Rate, Sum),
    memcache_internal_err: timeseries("memcache.internal_err"; Rate, Sum),
    memcache_deserialize_err: timeseries("memcache.deserialize_err"; Rate, Sum),

    origin_hit: timeseries("origin.hit"; Rate, Sum),
    origin_miss: timeseries("origin.miss"; Rate, Sum),
}

#[macro_export]
macro_rules! impl_singleton_stats {
    ( $name:literal ) => {
        fn stats(&self) -> &$crate::CacheStats {
            static STATS: std::sync::LazyLock<$crate::CacheStats> =
                std::sync::LazyLock::new(|| $crate::CacheStats::new(String::from($name)));
            &*STATS
        }
    };
}

/// Error type to help with proper reporting of memcache errors
pub enum McErrorKind {
    /// error came from calling memcache API
    MemcacheInternal,
    /// value returned from memcache was None
    Missing,
    /// deserialization of memcache data to Rust structures failed
    Deserialization,
}

const MEMCACHE_CONCURRENCY: usize = 100;

pub type McResult<T> = Result<T, McErrorKind>;

struct CachelibKey(String);
struct MemcacheKey(String);

/// TTL for caching an item
#[derive(Copy, Clone)]
pub enum CacheTtl {
    /// The item is valid forever, and can be cached indefinitely
    NoTtl,
    /// Fetch from backing store once the duration given expires
    Ttl(Duration),
}

/// Whether or not to cache an item
#[derive(Copy, Clone)]
pub enum CacheDisposition {
    /// Cache this item with the given TTL
    Cache(CacheTtl),
    /// Do not cache this item; re-fetch from backing store if it's requested again
    Ignore,
}

/// Implement this for a data item that can be cached. You will also need
/// #[derive(bincode::Encode, bincode::Decode)] on the data item.
pub trait MemcacheEntity: Sized {
    /// Convert the item to bytes that can live in Memcache and be deserialized
    /// in another process
    fn serialize(&self) -> Bytes;

    /// Deserialize the item from bytes into an object, or fail to do so
    fn deserialize(bytes: Bytes) -> McResult<Self>;
}

/// Implement this trait to indicate that you can cache values retrieved through you
pub trait EntityStore<V> {
    /// Get the cachelib handler. This can be created with `.into()` on a `VolatileLruCachePool`
    fn cachelib(&self) -> &CachelibHandler<V>;

    /// Get the Memcache KeyGen, for creating Memcache keys. This has both code and site versions,
    /// as well as a prefix.
    fn keygen(&self) -> &KeyGen;

    /// Get the Memcache handler. This can be created with `into()` on a `MemcacheClient`.
    fn memcache(&self) -> &MemcacheHandler;

    /// Given a value `v`, decide whether or not to cache it.
    fn cache_determinator(&self, v: &V) -> CacheDisposition;

    /// Finds the cache stats for this handler
    ///
    /// Implement this method with `caching_ext::impl_singleton_stats!` macro, instead of by hand
    fn stats(&self) -> &CacheStats;
}

/// Implement this to make it possible to fetch keys via the cache
#[async_trait]
pub trait KeyedEntityStore<K, V>: EntityStore<V> {
    /// Given an item key, return the cachelib key to use.
    fn get_cache_key(&self, key: &K) -> String;

    /// Given a set of keys to fetch from backing store, return a map from keys to fetched values
    ///
    /// If a key has no value in the backing store, omit it from the result map. Only use an
    /// Error for a failure to fetch, not absence
    async fn get_from_db(&self, keys: HashSet<K>) -> Result<HashMap<K, V>, Error>;

    fn on_memcache_hits<'a>(
        &self,
        _values: impl IntoIterator<Item = (&'a K, &'a mut V)>,
    ) -> Result<(), Error>
    where
        K: 'a,
        V: 'a,
    {
        Ok(())
    }
}

/// Utility function to fetch all keys in a single chunk without parallelism
pub async fn get_or_fill<K, V>(
    store: &impl KeyedEntityStore<K, V>,
    keys: HashSet<K>,
) -> Result<HashMap<K, V>, Error>
where
    K: Hash + Eq + Clone,
    V: bincode::Encode + bincode::Decode<()> + MemcacheEntity + Clone,
{
    get_or_fill_chunked(store, keys, usize::MAX, 1).await
}

/// The core of caching with this module. Takes a store that implements
/// `KeyedEntityStore`, and a set of keys to fetch. Returns a map
/// of fetched values.
///
/// Your accessor functions for consumers should call this to get values
/// from cache or backing store, as this will do the job of keeping
/// cachelib filled from memcache, and memcache filled from your backing store
///
/// fetch_chunk and parallel_chunks are used to implement chunked
/// and parallel fetching. Keys to fetch from the backing store
/// will be split into `fetch_chunk` size groups, and at most `parallel_chunks`
/// groups will be in flight at once.
pub async fn get_or_fill_chunked<K, V>(
    store: &impl KeyedEntityStore<K, V>,
    keys: HashSet<K>,
    fetch_chunk: usize,
    parallel_chunks: usize,
) -> Result<HashMap<K, V>, Error>
where
    K: Hash + Eq + Clone,
    V: bincode::Encode + bincode::Decode<()> + MemcacheEntity + Clone,
{
    let mut ret = HashMap::<K, V>::with_capacity(keys.len());

    let stats = store.stats();

    let cachelib_keys: Vec<_> = keys
        .into_iter()
        .map(|key| {
            let cachelib_key = CachelibKey(store.get_cache_key(&key));
            (key, cachelib_key)
        })
        .collect();

    let (fetched_from_cachelib, to_fetch_from_memcache) = store
        .cachelib()
        .get_multiple_from_cachelib::<K>(cachelib_keys)
        .with_context(|| "Error reading from cachelib")?;

    stats
        .cachelib_hit
        .add_value(fetched_from_cachelib.len() as i64);
    stats
        .cachelib_miss
        .add_value(to_fetch_from_memcache.len() as i64);

    ret.extend(fetched_from_cachelib);

    let to_fetch_from_memcache: Vec<(K, CachelibKey, MemcacheKey)> = to_fetch_from_memcache
        .into_iter()
        .map(|(key, cachelib_key)| {
            let memcache_key = MemcacheKey(store.keygen().key(&cachelib_key.0));
            (key, cachelib_key, memcache_key)
        })
        .collect();

    let to_fetch_from_store = {
        let (mut fetched_from_memcache, to_fetch_from_store) =
            get_multiple_from_memcache(store.memcache(), to_fetch_from_memcache, stats).await;

        stats
            .memcache_hit
            .add_value(fetched_from_memcache.len() as i64);
        stats
            .memcache_miss
            .add_value(to_fetch_from_store.len() as i64);

        store.on_memcache_hits(fetched_from_memcache.iter_mut().map(|(k, (v, _))| (k, v)))?;

        fill_multiple_cachelib(
            store.cachelib(),
            fetched_from_memcache
                .values()
                .filter_map(|(v, k)| match store.cache_determinator(v) {
                    CacheDisposition::Cache(ttl) => Some((k, ttl, v)),
                    _ => None,
                }),
        );

        ret.extend(fetched_from_memcache.into_iter().map(|(k, (v, _))| (k, v)));

        to_fetch_from_store
    };

    if !to_fetch_from_store.is_empty() {
        let to_fetch_from_store: Vec<_> = to_fetch_from_store
            .into_iter()
            .chunks(fetch_chunk)
            .into_iter()
            .map(|chunk| {
                let mut keys = HashSet::new();
                let mut key_mapping = HashMap::new();
                for (key, cachelib_key, memcache_key) in chunk {
                    keys.insert(key.clone());
                    key_mapping.insert(key.clone(), (cachelib_key, memcache_key));
                }
                fill_one_chunk(store, keys, key_mapping)
            })
            .collect();
        stream::iter(to_fetch_from_store)
            .buffer_unordered(parallel_chunks)
            .try_fold(&mut ret, |ret, chunk| async move {
                ret.extend(chunk);
                Ok::<_, Error>(ret)
            })
            .await?;
    }

    Ok(ret)
}

async fn fill_one_chunk<K, V>(
    store: &impl KeyedEntityStore<K, V>,
    keys: HashSet<K>,
    mut key_mapping: HashMap<K, (CachelibKey, MemcacheKey)>,
) -> Result<HashMap<K, V>, Error>
where
    K: Hash + Eq + Clone,
    V: bincode::Encode + bincode::Decode<()> + MemcacheEntity + Clone,
{
    let n_keys = keys.len();

    let stats = store.stats();
    let data = store
        .get_from_db(keys)
        .await
        .with_context(|| "Error reading from store")?;

    stats.origin_hit.add_value(data.len() as i64);
    stats.origin_miss.add_value((n_keys - data.len()) as i64);

    fill_caches_by_key(
        store,
        data.iter().map(|(key, v)| {
            let (cachelib_key, memcache_key) = key_mapping
                .remove(key)
                .expect("caching_ext: Missing entry in key_mapping, this should not happen");

            (cachelib_key, memcache_key, v)
        }),
    )
    .await;
    Ok(data)
}

pub fn fill_cachelib<'a, K, V>(
    store: &impl KeyedEntityStore<K, V>,
    data: impl IntoIterator<Item = (&'a K, &'a V)>,
) where
    K: Hash + Eq + Clone + 'a,
    V: bincode::Encode + bincode::Decode<()> + Clone + 'a,
{
    let mut cachelib_keys = Vec::new();
    for (k, v) in data {
        let cachelib_key = CachelibKey(store.get_cache_key(k));
        let ttl = match store.cache_determinator(v) {
            CacheDisposition::Cache(ttl) => ttl,
            CacheDisposition::Ignore => continue,
        };
        cachelib_keys.push((cachelib_key, ttl, v));
    }
    fill_multiple_cachelib(store.cachelib(), cachelib_keys);
}

/// Directly fill a cache from data you've prefetched outside the caching system
pub async fn fill_cache<'a, K, V>(
    store: &impl KeyedEntityStore<K, V>,
    data: impl IntoIterator<Item = (&'a K, &'a V)>,
) where
    K: Hash + Eq + Clone + 'a,
    V: bincode::Encode + bincode::Decode<()> + MemcacheEntity + Clone + 'a,
{
    fill_caches_by_key(
        store,
        data.into_iter().map(|(k, v)| {
            let cachelib_key = CachelibKey(store.get_cache_key(k));
            let memcache_key = MemcacheKey(store.keygen().key(&cachelib_key.0));
            (cachelib_key, memcache_key, v)
        }),
    )
    .await;
}

async fn fill_caches_by_key<'a, V>(
    store: &impl EntityStore<V>,
    data: impl IntoIterator<Item = (CachelibKey, MemcacheKey, &'a V)>,
) where
    V: bincode::Encode + bincode::Decode<()> + MemcacheEntity + Clone + 'a,
{
    let mut cachelib_keys = Vec::new();
    let mut memcache_keys = Vec::new();

    for (cachelib_key, memcache_key, v) in data.into_iter() {
        let ttl = match store.cache_determinator(v) {
            CacheDisposition::Cache(ttl) => ttl,
            CacheDisposition::Ignore => continue,
        };

        memcache_keys.push((memcache_key, ttl, v));
        cachelib_keys.push((cachelib_key, ttl, v));
    }

    fill_multiple_cachelib(store.cachelib(), cachelib_keys);

    fill_multiple_memcache(store.memcache(), memcache_keys).await;
}

async fn get_multiple_from_memcache<K, V>(
    memcache: &MemcacheHandler,
    keys: Vec<(K, CachelibKey, MemcacheKey)>,
    stats: &CacheStats,
) -> (
    HashMap<K, (V, CachelibKey)>,
    Vec<(K, CachelibKey, MemcacheKey)>,
)
where
    K: Eq + Hash,
    V: MemcacheEntity,
{
    let mc_fetch_futs = keys
        .into_iter()
        .map(move |(key, cachelib_key, memcache_key)| {
            cloned!(memcache);
            async move {
                let res = memcache
                    .get(memcache_key.0.clone())
                    .await
                    .map_err(|_| McErrorKind::MemcacheInternal)
                    .and_then(|maybe_bytes| maybe_bytes.ok_or(McErrorKind::Missing))
                    .and_then(V::deserialize);

                (key, cachelib_key, memcache_key, res)
            }
        });

    let mut entries = stream::iter(mc_fetch_futs).buffered(MEMCACHE_CONCURRENCY);

    let mut fetched = HashMap::new();
    let mut left_to_fetch = Vec::new();

    while let Some((key, cachelib_key, memcache_key, res)) = entries.next().await {
        match res {
            Ok(entity) => {
                fetched.insert(key, (entity, cachelib_key));
            }
            Err(e) => {
                match e {
                    McErrorKind::MemcacheInternal => stats.memcache_internal_err.add_value(1),
                    McErrorKind::Deserialization => stats.memcache_deserialize_err.add_value(1),
                    McErrorKind::Missing => {} // no op, we record missing at a higher level anyway.
                };

                left_to_fetch.push((key, cachelib_key, memcache_key));
            }
        }
    }

    (fetched, left_to_fetch)
}

fn fill_multiple_cachelib<'a, V>(
    cachelib: &'a CachelibHandler<V>,
    data: impl IntoIterator<Item = (impl Borrow<CachelibKey> + 'a, CacheTtl, &'a V)>,
) where
    V: bincode::Encode + bincode::Decode<()> + Clone,
{
    for (cachelib_key, ttl, v) in data {
        let cachelib_key = cachelib_key.borrow();

        let ttl = match ttl {
            CacheTtl::NoTtl => None,
            CacheTtl::Ttl(ttl) => Some(ttl),
        };

        // NOTE: We ignore failures to cache individual entries here.
        let _ = cachelib.set_cached(&cachelib_key.0, v, ttl);
    }
}

async fn fill_multiple_memcache<'a, V>(
    memcache: &'a MemcacheHandler,
    data: impl IntoIterator<Item = (MemcacheKey, CacheTtl, &'a V)>,
) where
    V: MemcacheEntity + 'a,
{
    if memcache.is_noop() {
        return;
    }

    let futs = data
        .into_iter()
        .filter_map(|(memcache_key, ttl, v)| {
            let bytes = v.serialize();

            if bytes.len() >= MEMCACHE_VALUE_MAX_SIZE {
                return None;
            }

            cloned!(memcache);

            Some(async move {
                match ttl {
                    CacheTtl::NoTtl => {
                        let _ = memcache.set(memcache_key.0, bytes).await;
                    }
                    CacheTtl::Ttl(ttl) => {
                        let _ = memcache.set_with_ttl(memcache_key.0, bytes, ttl).await;
                    }
                }
            })
        })
        .collect::<Vec<_>>();

    let fut = stream::iter(futs).for_each_concurrent(MEMCACHE_CONCURRENCY, |fut| fut);

    if memcache.is_async() {
        mononoke::spawn_task(fut);
    } else {
        fut.await;
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use maplit::hashmap;
    use maplit::hashset;

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    #[derive(bincode::Encode, bincode::Decode)]
    struct TestEntity(Vec<u8>);

    impl MemcacheEntity for TestEntity {
        fn serialize(&self) -> Bytes {
            Bytes::from(self.0.clone())
        }

        fn deserialize(bytes: Bytes) -> McResult<Self> {
            Ok(Self(bytes.to_vec()))
        }
    }

    struct TestStore {
        keygen: KeyGen,
        cachelib: CachelibHandler<TestEntity>,
        memcache: MemcacheHandler,
        calls: AtomicUsize,
        keys: AtomicUsize,
        data: HashMap<String, TestEntity>,
    }

    impl TestStore {
        pub fn new() -> Self {
            Self {
                keygen: KeyGen::new("", 0, 0),
                cachelib: CachelibHandler::create_mock(),
                memcache: MemcacheHandler::create_mock(),
                calls: AtomicUsize::new(0),
                keys: AtomicUsize::new(0),
                data: HashMap::new(),
            }
        }
    }

    impl EntityStore<TestEntity> for TestStore {
        fn cachelib(&self) -> &CachelibHandler<TestEntity> {
            &self.cachelib
        }

        fn keygen(&self) -> &KeyGen {
            &self.keygen
        }

        fn memcache(&self) -> &MemcacheHandler {
            &self.memcache
        }

        fn cache_determinator(&self, _: &TestEntity) -> CacheDisposition {
            CacheDisposition::Cache(CacheTtl::NoTtl)
        }

        impl_singleton_stats!("test");
    }

    #[async_trait]
    impl KeyedEntityStore<String, TestEntity> for TestStore {
        fn get_cache_key(&self, key: &String) -> String {
            format!("key:{key}")
        }

        async fn get_from_db(
            &self,
            keys: HashSet<String>,
        ) -> Result<HashMap<String, TestEntity>, Error> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.keys.fetch_add(keys.len(), Ordering::Relaxed);

            Ok(keys
                .into_iter()
                .filter_map(|k| {
                    let v = self.data.get(&k).cloned();
                    v.map(|v| (k, v))
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn simple() -> Result<(), Error> {
        let store = TestStore::new();

        let res = get_or_fill(&store, hashset! {}).await?;
        assert_eq!(res.len(), 0);
        assert_eq!(store.cachelib.gets_count(), 0);
        assert_eq!(store.memcache.gets_count(), 0);

        let res = get_or_fill(&store, hashset! {"key".into()}).await?;
        assert_eq!(res.len(), 0);
        assert_eq!(store.cachelib.gets_count(), 1);
        assert_eq!(store.memcache.gets_count(), 1);
        assert_eq!(store.keys.load(Ordering::Relaxed), 1);

        Ok(())
    }

    #[tokio::test]
    async fn fetch_from_db_cachelib_memcache() -> Result<(), Error> {
        let mut store = TestStore::new();

        let e = TestEntity(vec![0]);
        store.data.insert("key".into(), e.clone());

        // Fetch from db
        let res = get_or_fill(&store, hashset! {"key".into()}).await?;
        assert_eq!(res, hashmap! { "key".into() => e.clone() });
        assert_eq!(store.cachelib.gets_count(), 1);
        assert_eq!(store.memcache.gets_count(), 1);
        assert_eq!(store.keys.load(Ordering::Relaxed), 1);

        // Now fetch from cachelib
        let res = get_or_fill(&store, hashset! {"key".into()}).await?;
        assert_eq!(res, hashmap! { "key".into() => e.clone() });
        assert_eq!(store.cachelib.gets_count(), 2);
        assert_eq!(store.memcache.gets_count(), 1);
        assert_eq!(store.keys.load(Ordering::Relaxed), 1);

        // Reset cachelib, fetch from memcache
        store.cachelib = CachelibHandler::create_mock();
        let res = get_or_fill(&store, hashset! {"key".into()}).await?;
        assert_eq!(res, hashmap! { "key".into() => e.clone() });
        assert_eq!(store.cachelib.gets_count(), 1);
        assert_eq!(store.memcache.gets_count(), 2);
        assert_eq!(store.keys.load(Ordering::Relaxed), 1);

        Ok(())
    }

    #[tokio::test]
    async fn fetch_from_db() -> Result<(), Error> {
        let mut store = TestStore::new();

        let e0 = TestEntity(vec![0]);
        let e1 = TestEntity(vec![1]);
        let e2 = TestEntity(vec![2]);

        store.data.insert("key0".into(), e0.clone());
        store.data.insert("key1".into(), e1.clone());
        store.data.insert("key2".into(), e2.clone());

        let res = get_or_fill(
            &store,
            hashset! { "key0".into(), "key1".into(), "key2".into() },
        )
        .await?;

        assert_eq!(
            res,
            hashmap! { "key0".into() => e0, "key1".into() => e1, "key2".into() => e2 }
        );
        assert_eq!(store.calls.load(Ordering::Relaxed), 1);
        assert_eq!(store.cachelib.gets_count(), 3);
        assert_eq!(store.memcache.gets_count(), 3);
        assert_eq!(store.keys.load(Ordering::Relaxed), 3);

        Ok(())
    }

    #[tokio::test]
    async fn fetch_from_db_chunked() -> Result<(), Error> {
        let mut store = TestStore::new();

        let e0 = TestEntity(vec![0]);
        let e1 = TestEntity(vec![1]);
        let e2 = TestEntity(vec![2]);

        store.data.insert("key0".into(), e0.clone());
        store.data.insert("key1".into(), e1.clone());
        store.data.insert("key2".into(), e2.clone());

        let res = get_or_fill_chunked(
            &store,
            hashset! { "key0".into(), "key1".into(), "key2".into() },
            1,
            3,
        )
        .await?;

        assert_eq!(
            res,
            hashmap! { "key0".into() => e0, "key1".into() => e1, "key2".into() => e2 }
        );
        assert_eq!(store.calls.load(Ordering::Relaxed), 3);
        assert_eq!(store.cachelib.gets_count(), 3);
        assert_eq!(store.memcache.gets_count(), 3);
        assert_eq!(store.keys.load(Ordering::Relaxed), 3);

        Ok(())
    }

    #[tokio::test]
    async fn fetch_from_all() -> Result<(), Error> {
        let mut store = TestStore::new();

        let e0 = TestEntity(vec![0]);
        let e1 = TestEntity(vec![1]);
        let e2 = TestEntity(vec![2]);

        store.data.insert("key0".into(), e0.clone());
        store.data.insert("key1".into(), e1.clone());
        store.data.insert("key2".into(), e2.clone());

        let res = get_or_fill(&store, hashset! { "key1".into() }).await?;
        assert_eq!(res, hashmap! { "key1".into() => e1.clone() });
        assert_eq!(store.cachelib.gets_count(), 1);
        assert_eq!(store.memcache.gets_count(), 1);
        assert_eq!(store.calls.load(Ordering::Relaxed), 1);

        // Reset cachelib
        store.cachelib = CachelibHandler::create_mock();
        let res = get_or_fill(&store, hashset! { "key0".into() }).await?;
        assert_eq!(res, hashmap! { "key0".into() => e0.clone() });
        assert_eq!(store.cachelib.gets_count(), 1);
        assert_eq!(store.memcache.gets_count(), 2);
        assert_eq!(store.calls.load(Ordering::Relaxed), 2);

        let res = get_or_fill(
            &store,
            hashset! { "key0".into(), "key1".into(), "key2".into() },
        )
        .await?;

        assert_eq!(
            res,
            hashmap! { "key0".into() => e0.clone(), "key1".into() => e1.clone(), "key2".into() => e2.clone() }
        );
        assert_eq!(store.cachelib.gets_count(), 1 + 3); // 3 new fetches from cachelib, 2 misses
        assert_eq!(store.memcache.gets_count(), 2 + 2); // 2 new fetches from memcache, 1 miss
        assert_eq!(store.calls.load(Ordering::Relaxed), 2 + 1); // 1 fetch from db

        // Only from cachelib
        let res = get_or_fill(
            &store,
            hashset! { "key0".into(), "key1".into(), "key2".into() },
        )
        .await?;

        assert_eq!(
            res,
            hashmap! { "key0".into() => e0.clone(), "key1".into() => e1.clone(), "key2".into() => e2.clone() }
        );
        assert_eq!(store.cachelib.gets_count(), 7);
        assert_eq!(store.memcache.gets_count(), 4);
        assert_eq!(store.calls.load(Ordering::Relaxed), 3);

        // // Reset cachelib, only from memcache
        store.cachelib = CachelibHandler::create_mock();
        let res = get_or_fill(
            &store,
            hashset! { "key0".into(), "key1".into(), "key2".into() },
        )
        .await?;

        assert_eq!(
            res,
            hashmap! { "key0".into() => e0.clone(), "key1".into() => e1.clone(), "key2".into() => e2.clone() }
        );
        assert_eq!(store.cachelib.gets_count(), 3); // 3 misses
        assert_eq!(store.memcache.gets_count(), 4 + 3); // 3 hits
        assert_eq!(store.calls.load(Ordering::Relaxed), 3);

        Ok(())
    }

    #[tokio::test]
    async fn get_from_db_elision() -> Result<(), Error> {
        let store = TestStore::new();

        get_or_fill(&store, hashset! {}).await?;
        assert_eq!(store.calls.load(Ordering::Relaxed), 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_fill_cache() -> Result<(), Error> {
        let store = TestStore::new();
        let e0 = TestEntity(vec![0]);
        fill_cache(&store, hashmap! { "key0".into() => e0.clone() }.iter()).await;

        let res = get_or_fill(&store, hashset! { "key0".into() }).await?;
        assert_eq!(res, hashmap! { "key0".into() => e0.clone() });
        assert_eq!(store.cachelib.gets_count(), 1);
        assert_eq!(store.memcache.gets_count(), 0);
        assert_eq!(store.calls.load(Ordering::Relaxed), 0);

        Ok(())
    }
}
