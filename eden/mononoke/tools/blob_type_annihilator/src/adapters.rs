/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Direct access to one physical store: bounded key enumeration by prefix,
//! reads, definite existence checks and exact unlink.
//!
//! Stores are opened without multiplex, caching or packing wrappers, so keys
//! are physical keys exactly as stored.  Reads and writes go through the
//! readonly and throttling wrappers the global storage options select.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use blobstore::Blobstore;
use blobstore::BlobstoreEnumerableWithUnlink;
use blobstore::BlobstoreIsPresent;
use blobstore::BlobstoreKeyParam;
use blobstore::BlobstoreKeyRange;
use blobstore_factory::ReadOnlyStorage;
use blobstore_factory::ThrottleOptions;
use blobstore_factory::make_manifold_blobstore_enumerable_with_unlink;
use blobstore_factory::make_sql_blobstore;
use clap::Args;
use context::CoreContext;
use futures::Stream;
use futures::stream;
use metaconfig_types::BlobConfig;
use mononoke_app::MononokeApp;
use mononoke_types::BlobstoreBytes;
use readonlyblob::ReadOnlyBlobstore;
use sqlblob::Sqlblob;
use throttledblob::ThrottledBlob;
use tracing::warn;

use crate::progress::STORE_STATS;
use crate::progress::StoreOp;
use crate::retry::RetryPolicy;

#[derive(Args, Clone, Debug)]
pub struct AdapterOptions {
    /// Rows read per SQL query when enumerating SQLblob keys
    #[clap(long, default_value_t = NonZeroU32::new(1000).expect("1000 is non-zero"))]
    pub sql_page_size: NonZeroU32,

    /// Attempts at each key listing before it is reported as failed
    #[clap(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=30))]
    pub list_attempts: u32,

    /// Milliseconds before the first retry of a failed key listing.  Each
    /// further retry doubles the delay, up to 30 seconds
    #[clap(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(0..=30_000))]
    pub list_retry_delay_ms: u64,
}

/// Where the next page of an enumeration starts.
#[derive(Clone, Debug)]
enum Cursor {
    KeySourceStart,
    KeySource(BlobstoreKeyParam),
    /// Within `shard`, moving on to the next shard at its end.
    Sql {
        shard: usize,
        after: Option<String>,
    },
}

/// One page of physical keys, all starting with the requested prefix.
/// An empty page with a cursor is not the end of the enumeration.
struct Page {
    keys: Vec<String>,
    next: Option<Cursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnlinkOutcome {
    Unlinked,
    /// The key was confirmed absent, so there was nothing to unlink.
    AlreadyAbsent,
}

#[derive(Clone)]
enum Backend {
    /// A store with a `BlobstoreKeySource` range enumeration (Manifold).
    KeySource(Arc<dyn BlobstoreEnumerableWithUnlink>),
    /// SQLblob, enumerated shard by shard with bounded pages.
    Sql {
        store: Arc<Sqlblob>,
        page_size: NonZeroU32,
    },
}

#[derive(Clone)]
pub struct ComponentStore {
    /// Lists keys.
    backend: Backend,
    /// Reads, checks and writes keys.
    blobstore: Arc<dyn Blobstore>,
    list_retry: RetryPolicy,
}

/// The inclusive range covering every key starting with `prefix`: it also
/// includes the single key just past them, which callers filter out.
fn prefix_range(prefix: &str) -> Result<BlobstoreKeyRange> {
    let mut chars: Vec<char> = prefix.chars().collect();
    let last = chars.pop().context("cannot enumerate an empty prefix")?;
    let next = (u32::from(last) + 1..=u32::from(char::MAX))
        .find_map(char::from_u32)
        .with_context(|| format!("prefix {prefix:?} has no successor"))?;
    chars.push(next);
    Ok(BlobstoreKeyRange {
        begin_key: prefix.to_string(),
        end_key: chars.into_iter().collect(),
    })
}

/// Wraps `store` as the blobstore factory does for the global storage
/// options, so readonly storage refuses writes and throttles apply.
fn guarded(
    store: Arc<dyn Blobstore>,
    readonly: ReadOnlyStorage,
    throttle: ThrottleOptions,
) -> Arc<dyn Blobstore> {
    let store = if readonly.0 {
        Arc::new(ReadOnlyBlobstore::new(store)) as Arc<dyn Blobstore>
    } else {
        store
    };
    if throttle.has_throttle() {
        Arc::new(ThrottledBlob::new(store, throttle))
    } else {
        store
    }
}

impl ComponentStore {
    /// A store enumerated with `BlobstoreKeySource`, listing each page once.
    pub fn key_source(store: Arc<dyn BlobstoreEnumerableWithUnlink>) -> Self {
        Self {
            blobstore: Arc::new(store.clone()),
            backend: Backend::KeySource(store),
            list_retry: RetryPolicy::once(),
        }
    }

    /// A SQLblob store enumerated in pages of `page_size`, listing each page
    /// once.
    pub fn sql(store: Arc<Sqlblob>, page_size: NonZeroU32) -> Self {
        Self {
            blobstore: store.clone(),
            backend: Backend::Sql { store, page_size },
            list_retry: RetryPolicy::once(),
        }
    }

    #[cfg(test)]
    pub fn with_list_retry(self, list_retry: RetryPolicy) -> Self {
        Self { list_retry, ..self }
    }

    /// This store with the storage options applied, as `open` applies them.
    #[cfg(test)]
    pub fn with_storage_options(
        self,
        readonly: ReadOnlyStorage,
        throttle: ThrottleOptions,
    ) -> Self {
        Self {
            blobstore: guarded(self.blobstore, readonly, throttle),
            ..self
        }
    }

    /// Opens the physical store `config`, which must be a leaf config.
    pub async fn open(
        app: &MononokeApp,
        config: &BlobConfig,
        options: &AdapterOptions,
    ) -> Result<Self> {
        let store = match config {
            BlobConfig::Manifold { .. } | BlobConfig::ManifoldWithTtl { .. } => {
                let store = make_manifold_blobstore_enumerable_with_unlink(
                    app.fb,
                    config.clone(),
                    app.blobstore_options(),
                )
                .await?;
                ComponentStore::key_source(store)
            }
            BlobConfig::Sqlite { .. } | BlobConfig::Mysql { .. } => {
                let store = make_sql_blobstore(
                    app.fb,
                    config.clone(),
                    *app.readonly_storage(),
                    app.blobstore_options(),
                    app.config_store(),
                )
                .await?
                .into_inner();
                ComponentStore::sql(Arc::new(store), options.sql_page_size)
            }
            other => bail!("no adapter for physical store {other:?}"),
        };
        Ok(Self {
            blobstore: guarded(
                store.blobstore,
                *app.readonly_storage(),
                app.blobstore_options().throttle_options,
            ),
            list_retry: RetryPolicy::new(
                options.list_attempts,
                Duration::from_millis(options.list_retry_delay_ms),
            )?,
            ..store
        })
    }

    fn listing_start(&self) -> Cursor {
        match &self.backend {
            Backend::KeySource(_) => Cursor::KeySourceStart,
            Backend::Sql { .. } => Cursor::Sql {
                shard: 0,
                after: None,
            },
        }
    }

    /// Every page of keys starting with `prefix`, in listing order.  A
    /// failed listing yields its error and ends the stream, so a consumer
    /// must stop at the first error rather than treat the keys it has seen
    /// as complete.
    pub fn pages<'a>(
        &'a self,
        ctx: &'a CoreContext,
        prefix: &'a str,
    ) -> impl Stream<Item = Result<Vec<String>>> + 'a {
        Box::pin(stream::unfold(
            Some(self.listing_start()),
            move |cursor| async move {
                let page = self.enumerate_page(ctx, prefix, cursor?).await;
                let next = page.as_ref().ok().and_then(|page| page.next.clone());
                Some((page.map(|page| page.keys), next))
            },
        ))
    }

    /// Fetches one page of keys starting with `prefix`, retrying failed
    /// listings.  `cursor` must come from the previous page of the same
    /// listing.
    async fn enumerate_page(
        &self,
        ctx: &CoreContext,
        prefix: &str,
        cursor: Cursor,
    ) -> Result<Page> {
        self.list_retry
            .run(
                |_| self.enumerate_page_once(ctx, prefix, cursor.clone()),
                |attempt, error| {
                    warn!("listing {prefix} failed on attempt {attempt}, retrying: {error:#}");
                    STORE_STATS.record(StoreOp::ListingRetried);
                },
            )
            .await
            .with_context(|| format!("listing {prefix}"))
    }

    async fn enumerate_page_once(
        &self,
        ctx: &CoreContext,
        prefix: &str,
        cursor: Cursor,
    ) -> Result<Page> {
        let page = match (&self.backend, cursor) {
            (
                Backend::KeySource(store),
                cursor @ (Cursor::KeySourceStart | Cursor::KeySource(_)),
            ) => {
                let param = match cursor {
                    Cursor::KeySource(param) => param,
                    _ => BlobstoreKeyParam::Start(prefix_range(prefix)?),
                };
                let data = store.enumerate(ctx, &param).await?;
                Page {
                    keys: data
                        .keys
                        .into_iter()
                        .filter(|key| key.starts_with(prefix))
                        .collect(),
                    next: data.next_token.map(Cursor::KeySource),
                }
            }
            (Backend::Sql { store, page_size }, Cursor::Sql { shard, after }) => {
                let page = store
                    .get_keys_with_prefix_from_shard(
                        ctx,
                        shard,
                        prefix,
                        after.as_deref(),
                        *page_size,
                    )
                    .await?;
                let next = match page.resume_after {
                    Some(after) => Some(Cursor::Sql {
                        shard,
                        after: Some(after),
                    }),
                    None if shard + 1 < store.shard_count() => Some(Cursor::Sql {
                        shard: shard + 1,
                        after: None,
                    }),
                    None => None,
                };
                Page {
                    keys: page.keys,
                    next,
                }
            }
            (_, cursor) => bail!("cursor {cursor:?} does not belong to this store"),
        };
        STORE_STATS.record(StoreOp::PageListed);
        STORE_STATS.add(StoreOp::KeySeen, page.keys.len());
        Ok(page)
    }

    pub fn blobstore(&self) -> Arc<dyn Blobstore> {
        self.blobstore.clone()
    }

    pub async fn get(&self, ctx: &CoreContext, key: &str) -> Result<Option<BlobstoreBytes>> {
        STORE_STATS.record(StoreOp::ValueRead);
        Ok(self
            .blobstore
            .get(ctx, key)
            .await
            .with_context(|| format!("reading {key}"))?
            .map(|data| data.into_bytes()))
    }

    /// Whether `key` exists; fails rather than guess when the store cannot
    /// say.
    pub async fn is_present(&self, ctx: &CoreContext, key: &str) -> Result<bool> {
        match self
            .blobstore
            .is_present(ctx, key)
            .await
            .with_context(|| format!("checking {key}"))?
        {
            BlobstoreIsPresent::Present => Ok(true),
            BlobstoreIsPresent::Absent => Ok(false),
            BlobstoreIsPresent::ProbablyNotPresent(e) => {
                Err(e.context(format!("existence of {key} is unknown")))
            }
        }
    }

    /// Unlinks exactly `key`.  A failed unlink of a key that is confirmed
    /// absent is reported as already absent.
    pub async fn unlink(&self, ctx: &CoreContext, key: &str) -> Result<UnlinkOutcome> {
        match self.blobstore.unlink(ctx, key).await {
            Ok(()) => {
                STORE_STATS.record(StoreOp::KeyUnlinked);
                Ok(UnlinkOutcome::Unlinked)
            }
            Err(unlink_error) => match self.is_present(ctx, key).await {
                Ok(false) => Ok(UnlinkOutcome::AlreadyAbsent),
                Ok(true) => Err(unlink_error.context(format!("unlinking {key}"))),
                Err(check_error) => Err(unlink_error
                    .context(format!("unlinking {key}"))
                    .context(check_error)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;

    use async_trait::async_trait;
    use blobstore::BlobstoreEnumerationData;
    use blobstore::BlobstoreGetData;
    use blobstore::BlobstoreKeySource;
    use blobstore::OverwriteStatus;
    use blobstore::PutBehaviour;
    use bytes::Bytes;
    use fbinit::FacebookInit;
    use futures::stream::TryStreamExt;
    use memblob::Memblob;
    use mononoke_macros::mononoke;

    use super::*;

    fn value(bytes: &'static [u8]) -> BlobstoreBytes {
        BlobstoreBytes::from_bytes(Bytes::from_static(bytes))
    }

    /// A store whose first `failures` listings fail.
    #[derive(Debug, Default)]
    struct Flaky {
        inner: Memblob,
        failures: AtomicU32,
    }

    fn take_failure(counter: &AtomicU32) -> bool {
        counter
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
    }

    impl fmt::Display for Flaky {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "Flaky")
        }
    }

    #[async_trait]
    impl Blobstore for Flaky {
        async fn get<'a>(
            &'a self,
            ctx: &'a CoreContext,
            key: &'a str,
        ) -> Result<Option<BlobstoreGetData>> {
            self.inner.get(ctx, key).await
        }

        async fn put_explicit<'a>(
            &'a self,
            ctx: &'a CoreContext,
            key: String,
            value: BlobstoreBytes,
            put_behaviour: PutBehaviour,
        ) -> Result<OverwriteStatus> {
            self.inner
                .put_explicit(ctx, key, value, put_behaviour)
                .await
        }

        async fn put_with_status<'a>(
            &'a self,
            ctx: &'a CoreContext,
            key: String,
            value: BlobstoreBytes,
        ) -> Result<OverwriteStatus> {
            self.inner.put_with_status(ctx, key, value).await
        }

        async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
            Blobstore::unlink(&self.inner, ctx, key).await
        }
    }

    #[async_trait]
    impl BlobstoreKeySource for Flaky {
        async fn enumerate<'a>(
            &'a self,
            ctx: &'a CoreContext,
            range: &'a BlobstoreKeyParam,
        ) -> Result<BlobstoreEnumerationData> {
            if take_failure(&self.failures) {
                bail!("injected listing failure");
            }
            self.inner.enumerate(ctx, range).await
        }
    }

    async fn all_keys(
        store: &ComponentStore,
        ctx: &CoreContext,
        prefix: &str,
    ) -> Result<Vec<String>> {
        let mut keys: Vec<String> = store.pages(ctx, prefix).try_concat().await?;
        keys.sort();
        Ok(keys)
    }

    #[mononoke::test]
    fn prefix_ranges_are_inclusive_and_tight() -> Result<()> {
        let range = prefix_range("repo0001.historymf.file.blake2.0a")?;
        assert_eq!(range.begin_key, "repo0001.historymf.file.blake2.0a");
        assert_eq!(range.end_key, "repo0001.historymf.file.blake2.0b");
        assert!(
            prefix_range("").is_err(),
            "an empty bound would be unbounded"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn enumeration_filters_to_the_prefix(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let memblob = Arc::new(Memblob::default());
        for key in ["p.a0", "p.a1.pack", "p.a", "p.b0", "p.a\u{7f}"] {
            memblob.put(&ctx, key.to_string(), value(b"v")).await?;
        }
        let store = ComponentStore::key_source(memblob);
        assert_eq!(
            all_keys(&store, &ctx, "p.a").await?,
            vec!["p.a", "p.a0", "p.a1.pack", "p.a\u{7f}"]
        );
        assert_eq!(all_keys(&store, &ctx, "p.a0").await?, vec!["p.a0"]);
        assert!(all_keys(&store, &ctx, "q").await?.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn sql_enumeration_crosses_shards_with_small_pages(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (_, config_store) = sqlblob::get_test_config_store();
        let sql = Arc::new(
            Sqlblob::with_sqlite_in_memory(PutBehaviour::IfAbsent, &config_store, true, 0)?
                .into_inner(),
        );
        let wanted: Vec<String> = (0..9).map(|i| format!("repo0001.x.{i}")).collect();
        for key in wanted.iter().chain(&["repo0002.x.0".to_string()]) {
            sql.put(&ctx, key.clone(), value(b"v")).await?;
        }
        let store = ComponentStore::sql(sql, NonZeroU32::new(2).expect("2 is non-zero"));
        let pages: Vec<Vec<String>> = store.pages(&ctx, "repo0001.x.").try_collect().await?;
        assert!(pages.iter().all(|page| page.len() <= 2));
        assert!(pages.len() > 2, "listing takes several pages");
        let mut keys: Vec<String> = pages.into_iter().flatten().collect();
        keys.sort();
        assert_eq!(keys, wanted);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn listing_is_retried_until_it_succeeds(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let flaky = Arc::new(Flaky {
            failures: AtomicU32::new(2),
            ..Default::default()
        });
        flaky
            .inner
            .put(&ctx, "p.a0".to_string(), value(b"v"))
            .await?;
        let store = ComponentStore::key_source(flaky.clone())
            .with_list_retry(RetryPolicy::new(3, Duration::ZERO)?);
        assert_eq!(
            all_keys(&store, &ctx, "p.a").await?,
            vec!["p.a0"],
            "two failures fit within three attempts"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn listing_fails_once_its_attempts_are_used(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let flaky = Arc::new(Flaky {
            failures: AtomicU32::new(3),
            ..Default::default()
        });
        let store = ComponentStore::key_source(flaky.clone())
            .with_list_retry(RetryPolicy::new(3, Duration::ZERO)?);
        assert!(
            all_keys(&store, &ctx, "p.a").await.is_err(),
            "three failures exhaust three attempts"
        );
        assert_eq!(flaky.failures.load(Ordering::SeqCst), 0);
        let once = ComponentStore::key_source(Arc::new(Flaky {
            failures: AtomicU32::new(1),
            ..Default::default()
        }));
        assert!(
            all_keys(&once, &ctx, "p.a").await.is_err(),
            "a store opened without retries lists once"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn unlink_distinguishes_absent_keys(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let memblob = Arc::new(Memblob::default());
        memblob.put(&ctx, "k".to_string(), value(b"v")).await?;
        let store = ComponentStore::key_source(memblob);
        assert_eq!(store.unlink(&ctx, "k").await?, UnlinkOutcome::Unlinked);
        assert_eq!(store.unlink(&ctx, "k").await?, UnlinkOutcome::AlreadyAbsent);
        assert!(!store.is_present(&ctx, "k").await?);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn readonly_storage_refuses_unlinks(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let memblob = Arc::new(Memblob::default());
        memblob.put(&ctx, "k".to_string(), value(b"v")).await?;
        let readonly = guarded(
            memblob.clone(),
            ReadOnlyStorage(true),
            ThrottleOptions::default(),
        );
        assert!(readonly.unlink(&ctx, "k").await.is_err());
        assert!(readonly.get(&ctx, "k").await?.is_some(), "reads still work");
        let writable = guarded(memblob, ReadOnlyStorage(false), ThrottleOptions::default());
        writable.unlink(&ctx, "k").await?;
        assert!(writable.get(&ctx, "k").await?.is_none());
        Ok(())
    }
}
