/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt;
use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use blobstore::Blobstore;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::OverwriteStatus;
use blobstore::PutBehaviour;
use context::CoreContext;
use governor::Jitter;
use governor::Quota;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::state::InMemoryState;
use governor::state::direct::NotKeyed;
use mononoke_types::BlobstoreBytes;
use nonzero_ext::nonzero;

#[derive(Clone, Copy, Debug, Default)]
pub struct ThrottleOptions {
    pub read_qps: Option<NonZeroU32>,
    pub write_qps: Option<NonZeroU32>,
    pub read_bytes: Option<NonZeroUsize>,
    pub write_bytes: Option<NonZeroUsize>,
    pub read_burst_bytes: Option<NonZeroUsize>,
    pub write_burst_bytes: Option<NonZeroUsize>,
    pub bytes_min_count: Option<NonZeroUsize>,
}

impl ThrottleOptions {
    pub fn has_throttle(&self) -> bool {
        self.read_qps.is_some()
            || self.write_qps.is_some()
            || self.read_bytes.is_some()
            || self.write_bytes.is_some()
    }
}

fn bytes_to_count(bytes_min_count: usize, num_bytes: usize) -> NonZeroU32 {
    let count: u32 = (num_bytes / bytes_min_count).try_into().unwrap_or(u32::MAX);
    NonZeroU32::new(count).unwrap_or(nonzero!(1u32))
}

// The rate limiters use u32, so if we want to go > 4GiB/s need to scale the bytes to the rate limiter count.
// Default of 1_000 allows us to throttle in the range 1KB to 42.94GB/s (aka 40GiB/s). 40GiB/s should be enough for anybody :)
pub const DEFAULT_BYTES_MIN_COUNT: usize = 1_000;

// Any blobs over this the max don't attempt to throttle, instead they error.
// Default is set high as we'd rather throttle than error unless specified
pub const DEFAULT_BURST_BYTES_S: usize = 100_000_000;

/// A Blobstore that rate limits the number of read and write operations.
pub struct ThrottledBlob<T: fmt::Debug> {
    blobstore: T,
    read_qps_limiter: Option<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
    write_qps_limiter: Option<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
    read_bytes_limiter: Option<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
    write_bytes_limiter: Option<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
    bytes_min_count: usize,
    /// The options fields are used for Debug. They are not consulted at runtime.
    options: ThrottleOptions,
}

impl<T: fmt::Display + fmt::Debug> fmt::Display for ThrottledBlob<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ThrottledBlob<{}>", self.blobstore)
    }
}

static JITTER_MAX: Duration = Duration::from_millis(5);

fn jitter() -> Jitter {
    Jitter::up_to(JITTER_MAX)
}

impl<T: fmt::Debug + Send + Sync> ThrottledBlob<T> {
    pub fn new(blobstore: T, options: ThrottleOptions) -> Self {
        let qps_limiter =
            |qps: Option<NonZeroU32>| qps.map(|qps| RateLimiter::direct(Quota::per_second(qps)));
        let read_qps_limiter = qps_limiter(options.read_qps);
        let write_qps_limiter = qps_limiter(options.write_qps);

        let bytes_min_count = options
            .bytes_min_count
            .map_or(DEFAULT_BYTES_MIN_COUNT, |v| v.get());
        let bytes_limiter = |bytes_s: Option<NonZeroUsize>, burst_bytes_s: Option<NonZeroUsize>| {
            bytes_s.map(|bytes_s| {
                let count_s = bytes_to_count(bytes_min_count, bytes_s.get());
                RateLimiter::direct(Quota::per_second(count_s).allow_burst(
                    burst_bytes_s.map_or_else(
                        || bytes_to_count(bytes_min_count, DEFAULT_BURST_BYTES_S),
                        |burst_bytes_s| bytes_to_count(bytes_min_count, burst_bytes_s.get()),
                    ),
                ))
            })
        };
        let read_bytes_limiter = bytes_limiter(options.read_bytes, options.read_burst_bytes);
        let write_bytes_limiter = bytes_limiter(options.write_bytes, options.write_burst_bytes);

        Self {
            blobstore,
            read_qps_limiter,
            write_qps_limiter,
            read_bytes_limiter,
            write_bytes_limiter,
            bytes_min_count,
            options,
        }
    }

    // Convert from number of bytes to the count to request from until_n_ready
    fn count_n(&self, num_bytes: usize) -> NonZeroU32 {
        bytes_to_count(self.bytes_min_count, num_bytes)
    }
}

#[async_trait]
impl<T: Blobstore> Blobstore for ThrottledBlob<T> {
    // Throttling for get() bytes/s is approximate as we only know the size after the blob has been read
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        if let Some(limiter) = self.read_qps_limiter.as_ref() {
            limiter.until_ready_with_jitter(jitter()).await;
        }
        if let Some(limiter) = self.read_bytes_limiter.as_ref() {
            // Only know we'll use some bytes. Access one count so we throttle if already over the limit
            limiter.until_ready_with_jitter(jitter()).await;
        }

        let get_data = self.blobstore.get(ctx, key).await?;

        if let Some(limiter) = self.read_bytes_limiter.as_ref() {
            // Now we know the size, request rest of the quota
            if let Some(data) = get_data.as_ref() {
                let count_n = self.count_n(data.as_bytes().len());
                let adjusted_n = NonZeroU32::new(count_n.get().saturating_sub(1));
                if let Some(adjusted_n) = adjusted_n {
                    limiter
                        .until_n_ready_with_jitter(adjusted_n, jitter())
                        .await?;
                }
            }
        }
        Ok(get_data)
    }

    async fn is_present<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent> {
        if let Some(limiter) = self.read_qps_limiter.as_ref() {
            limiter.until_ready_with_jitter(jitter()).await;
        }
        // TODO(ahornby) would need to enhance Blobstore::is_present() to know how many bytes it transferred.
        // Some stores fetch just a flag, some fetch all the data then throw it away.
        if let Some(limiter) = self.read_bytes_limiter.as_ref() {
            limiter.until_ready_with_jitter(jitter()).await;
        }
        self.blobstore.is_present(ctx, key).await
    }

    // No data passes through the wrapper, so only the write rate applies.  The
    // trait's default get and put would not overwrite under `IfAbsent`.
    async fn copy<'a>(
        &'a self,
        ctx: &'a CoreContext,
        old_key: &'a str,
        new_key: String,
    ) -> Result<()> {
        if let Some(limiter) = self.write_qps_limiter.as_ref() {
            limiter.until_ready_with_jitter(jitter()).await;
        }
        self.blobstore.copy(ctx, old_key, new_key).await
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        if let Some(limiter) = self.write_qps_limiter.as_ref() {
            limiter.until_ready_with_jitter(jitter()).await;
        }
        self.blobstore.unlink(ctx, key).await
    }

    async fn put_explicit<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
        put_behaviour: PutBehaviour,
    ) -> Result<OverwriteStatus> {
        if let Some(limiter) = self.write_qps_limiter.as_ref() {
            limiter.until_ready_with_jitter(jitter()).await;
        }
        if let Some(limiter) = self.write_bytes_limiter.as_ref() {
            limiter
                .until_n_ready_with_jitter(self.count_n(value.len()), jitter())
                .await?;
        }
        self.blobstore
            .put_explicit(ctx, key, value, put_behaviour)
            .await
    }

    async fn put_with_status<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<OverwriteStatus> {
        if let Some(limiter) = self.write_qps_limiter.as_ref() {
            limiter.until_ready_with_jitter(jitter()).await;
        }
        if let Some(limiter) = self.write_bytes_limiter.as_ref() {
            limiter
                .until_n_ready_with_jitter(self.count_n(value.len()), jitter())
                .await?;
        }
        self.blobstore.put_with_status(ctx, key, value).await
    }
}

impl<T: fmt::Debug> fmt::Debug for ThrottledBlob<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThrottledBlob")
            .field("blobstore", &self.blobstore)
            .field("options", &self.options)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use fbinit::FacebookInit;
    use memblob::Memblob;
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::fbinit_test]
    async fn copy_overwrites_through_the_wrapper(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let inner = Memblob::new(PutBehaviour::IfAbsent);
        let options = ThrottleOptions {
            write_qps: NonZeroU32::new(1_000_000),
            ..Default::default()
        };
        let throttled = ThrottledBlob::new(inner.clone(), options);
        throttled
            .put(
                &ctx,
                "old".to_string(),
                BlobstoreBytes::from_bytes("new value"),
            )
            .await?;
        throttled
            .put(
                &ctx,
                "new".to_string(),
                BlobstoreBytes::from_bytes("stale value"),
            )
            .await?;
        throttled
            .put(
                &ctx,
                "new".to_string(),
                BlobstoreBytes::from_bytes("other value"),
            )
            .await?;
        let kept = inner.get(&ctx, "new").await?.context("new is present")?;
        assert_eq!(
            kept.into_bytes(),
            BlobstoreBytes::from_bytes("stale value"),
            "a put does not overwrite under IfAbsent"
        );

        throttled.copy(&ctx, "old", "new".to_string()).await?;
        let copied = inner.get(&ctx, "new").await?.context("new is present")?;
        assert_eq!(
            copied.into_bytes(),
            BlobstoreBytes::from_bytes("new value"),
            "a copy replaces the existing value, as the inner store's copy does"
        );
        Ok(())
    }
}
