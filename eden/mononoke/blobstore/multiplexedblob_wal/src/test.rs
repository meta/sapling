/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt::Debug;
use std::future::Future;
use std::panic;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use blobstore::Blobstore;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore_sync_queue::BlobstoreWal;
use blobstore_sync_queue::BlobstoreWalEntry;
use blobstore_sync_queue::SqlBlobstoreWalBuilder;
use blobstore_test_utils::Tickable;
use borrowed::borrowed;
use bytes::Bytes;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::future::FutureExt;
use futures::task::Poll;
use lock_ext::LockExt;
use metaconfig_types::BlobstoreId;
use metaconfig_types::MultiplexId;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreBytes;
use mononoke_types::Timestamp;
use multiplexedblob::LoggingScrubHandler;
use multiplexedblob::ScrubAction;
use multiplexedblob::ScrubHandler;
use multiplexedblob::ScrubOptions;
use multiplexedblob::SrubWriteOnly;
use nonzero_ext::nonzero;
use scuba_ext::MononokeScubaSampleBuilder;
use sql_construct::SqlConstruct;

use crate::MultiplexTimeout;
use crate::Scuba;
use crate::WalMultiplexedBlobstore;
use crate::scrub::WalScrubBlobstore;

#[mononoke::fbinit_test]
async fn test_quorum_is_valid(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let scuba = Scuba::new(
        MononokeScubaSampleBuilder::with_discard(),
        MononokeScubaSampleBuilder::with_discard(),
        nonzero!(1u64),
    )?;
    let wal =
        Arc::new(SqlBlobstoreWalBuilder::with_sqlite_in_memory()?.build(ctx.sql_query_telemetry()));

    // Check the quorum cannot be zero
    {
        // no main-stores, no write-only
        assert!(setup_multiplex(0, 0, None).is_err());
    }

    // Check creating multiplex fails if there are no enough main blobstores
    {
        let stores = (0..2)
            .map(|id| {
                (
                    BlobstoreId::new(id),
                    Arc::new(Tickable::new()) as Arc<dyn Blobstore>,
                )
            })
            .collect();
        // write-only don't count into the quorum
        let write_only = (2..4)
            .map(|id| {
                (
                    BlobstoreId::new(id),
                    Arc::new(Tickable::new()) as Arc<dyn Blobstore>,
                )
            })
            .collect();
        let quorum = 3;
        let result = WalMultiplexedBlobstore::new(
            MultiplexId::new(0),
            wal,
            stores,
            write_only,
            quorum,
            None,
            scuba,
        );

        assert!(result.is_err());
    }

    // Check creating multiplex succeeds with the same amount of stores as the quorum
    {
        assert!(setup_multiplex(3, 3, None).is_ok());
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_put_wal_fails(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (tickable_queue, tickable_blobstores, multiplex) = setup_multiplex(3, 2, None)?;

    let v = make_value("v");
    let k = "k";

    let mut put_fut = multiplex.put(&ctx, k.to_owned(), v).boxed();
    assert_pending(&mut put_fut).await;

    // wal queue write fails
    tickable_queue.tick(Some("wal queue failed"));

    // multiplex put should fail
    assert!(put_fut.await.is_err());

    // check there is no blob in the storage
    {
        let mut get_fut = multiplex.get(&ctx, k).boxed();
        assert_pending(&mut get_fut).await;

        // blobstore gets succeed
        for (_id, store) in tickable_blobstores.iter() {
            store.tick(None);
        }
        validate_blob(get_fut.await, Ok(None));
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_put_fails(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (tickable_queue, tickable_blobstores, multiplex) = setup_multiplex(3, 2, None)?;

    // All puts fail, the multiplex put should fail: [x] [x] [x]
    {
        let v = make_value("v0");
        let k = "k0";

        let mut put_fut = multiplex.put(&ctx, k.to_owned(), v).boxed();
        assert_pending(&mut put_fut).await;

        // wal queue write succeeds
        tickable_queue.tick(None);
        assert_pending(&mut put_fut).await;

        // all blobstores should fail
        for (id, store) in tickable_blobstores.iter() {
            store.tick(Some(format!("all fail: bs{id} failed").as_str()));
        }

        assert!(put_fut.await.is_err());

        // No `put` succeeded, there is no blob in the storage
        {
            let mut get_fut = multiplex.get(&ctx, k).boxed();
            assert_pending(&mut get_fut).await;

            // blobstore gets succeed
            for (_id, store) in tickable_blobstores.iter() {
                store.tick(None);
            }
            validate_blob(get_fut.await, Ok(None));
        }
    }

    // Second blobstore put succeeded but no quorum was achieved, multiplex put fails: [x] [ ] [x]
    {
        let v = make_value("v1");
        let k = "k1";

        let mut put_fut = multiplex.put(&ctx, k.to_owned(), v.clone()).boxed();
        assert_pending(&mut put_fut).await;

        // wal queue write succeeds
        tickable_queue.tick(None);
        assert_pending(&mut put_fut).await;

        // first blobstore fails
        tickable_blobstores[0].1.tick(Some("all fail: bs0 failed"));
        assert_pending(&mut put_fut).await;
        // second blobstore succeeds
        tickable_blobstores[1].1.tick(None);
        assert_pending(&mut put_fut).await;
        // third blobstore fails
        tickable_blobstores[2].1.tick(Some("all fail: bs2 failed"));

        // the multiplex put fails
        assert!(put_fut.await.is_err());

        // The blob was written to the second blobstore even if the multiplex put failed.
        // That means `get` result is undefined: it can either return `None` or `Some`
        // depending on which blobstores' put returned first.
        //
        // This is fine because if multiplex `put` failed, multiplex blobstore doesn't provide
        // any guarantees whether the blob is present in the storage or not.
        // There is only guarantee: if `put` succeeded, the blob will always be present,
        // i.e. `get` will always return `Some` (if it didn't fail for some other reason).

        // 1st and 3rd blobstore returned `None`, before 2nd blobstore returned anything
        // there is a read quorum on `None`, so multiplex `get` should also return `None`
        {
            let mut get_fut = multiplex.get(&ctx, k).boxed();
            assert_pending(&mut get_fut).await;

            // first and third blobstores don't have the blob
            tickable_blobstores[0].1.tick(None);
            tickable_blobstores[2].1.tick(None);

            // the result is ready
            validate_blob(get_fut.await, Ok(None));
            tickable_blobstores[1].1.drain(1);
        }

        // 2nd blobstore returned before the read quorum on `None` was achieved, so
        // multiplex `get` should also return `Some`
        {
            let mut get_fut = multiplex.get(&ctx, k).boxed();
            assert_pending(&mut get_fut).await;

            // first blobstore doesn't have the blob
            tickable_blobstores[0].1.tick(None);
            // second blobstore has
            tickable_blobstores[1].1.tick(None);

            // the result is ready
            validate_blob(get_fut.await, Ok(Some(&v)));
            tickable_blobstores[2].1.drain(1);
        }
    }

    Ok(())
}

async fn queue_keys(ctx: &CoreContext, multiplex: &WalMultiplexedBlobstore) -> Result<Vec<String>> {
    let mut entries: Vec<_> = multiplex
        .wal_queue
        .read(ctx, &multiplex.multiplex_id, &Timestamp::now(), 100)
        .await?
        .into_iter()
        .map(|e| e.blobstore_key)
        .collect();
    entries.sort_unstable();
    Ok(entries)
}

#[mononoke::fbinit_test]
async fn test_put_succeeds(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);

    let quorum = 2;
    let (tickable_queue, tickable_blobstores, multiplex) = setup_multiplex(3, quorum, None)?;

    // Quorum puts succeed, the multiplex put succeeds: [ ] [x] [ ]
    // Should wait for the third put to complete.
    {
        let v = make_value("v2");
        let k = "k2";

        let mut put_fut = multiplex.put(&ctx, k.to_owned(), v.clone()).boxed();
        assert_pending(&mut put_fut).await;

        // wal queue write succeeds
        tickable_queue.tick(None);
        assert_pending(&mut put_fut).await;
        assert_eq!(&queue_keys(&ctx, &multiplex).await?, &["k2"]);

        // first blobstore succeeds
        tickable_blobstores[0].1.tick(None);
        assert_pending(&mut put_fut).await;
        // second blobstore fails
        tickable_blobstores[1].1.tick(Some("all fail: bs1 failed"));
        assert_pending(&mut put_fut).await;
        // third blobstore succeeds
        tickable_blobstores[2].1.tick(None);

        assert!(put_fut.await.is_ok());

        // check we can read the blob from the 1st store
        {
            let mut get_fut = multiplex.get(&ctx, k).boxed();
            assert_pending(&mut get_fut).await;

            // first blobstore returns the blob
            tickable_blobstores[0].1.tick(None);
            validate_blob(get_fut.await, Ok(Some(&v)));

            // drain the tickables of the pending requests, as they won't be claimed
            for (_id, store) in &tickable_blobstores[1..3] {
                store.drain(1);
            }
        }

        // check we can read the blob from the 3rd store
        {
            let mut get_fut = multiplex.get(&ctx, k).boxed();
            assert_pending(&mut get_fut).await;

            // first blobstore fails
            tickable_blobstores[0].1.tick(Some("bs0 failed"));
            assert_pending(&mut get_fut).await;

            // second blobstore doesn't have the blob
            tickable_blobstores[1].1.tick(None);
            assert_pending(&mut get_fut).await;

            // third blobstore succeeds
            tickable_blobstores[2].1.tick(None);
            validate_blob(get_fut.await, Ok(Some(&v)));
        }
    }

    // All puts succeed, the multiplex put succeeds: [ ] [ ] [ ]
    // Should not wait for the third put to complete.
    {
        let v = make_value("v3");
        let k = "k3";

        let mut put_fut = multiplex.put(&ctx, k.to_owned(), v.clone()).boxed();
        assert_pending(&mut put_fut).await;

        // wal queue write succeeds
        tickable_queue.tick(None);
        assert_pending(&mut put_fut).await;

        // first quorum blobstore puts succeed, multiplex doesn't wait for the rest
        // of the puts
        for (_id, store) in &tickable_blobstores[0..quorum] {
            store.tick(None);
        }

        assert!(put_fut.await.is_ok());

        // check we can read the blob
        {
            let mut get_fut = multiplex.get(&ctx, k).boxed();
            assert_pending(&mut get_fut).await;

            // blobstore gets succeed
            for (_id, store) in tickable_blobstores.iter() {
                store.tick(None);
            }
            validate_blob(get_fut.await, Ok(Some(&v)));
        }
        // check the optimisation to delete the item from the queue
        {
            tokio::task::yield_now().await;
            // 2 item in the queue (k2 is from the previous test)
            assert_eq!(&queue_keys(&ctx, &multiplex).await?, &["k2", "k3"]);
            // Tick the rest of the blobstores
            for (_id, store) in &tickable_blobstores[quorum..] {
                store.tick(None);
            }
            tokio::task::yield_now().await;
            // Tick deletion
            tickable_queue.tick(None);
            tokio::task::yield_now().await;
            // k2 correctly doesn't leave the key because we didn't succeed all writes
            assert_eq!(&queue_keys(&ctx, &multiplex).await?, &["k2"]);
        }
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_get_on_missing(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (_tickable_queue, tickable_blobstores, multiplex) = setup_multiplex(3, 2, None)?;

    // No blobstores have the key
    let k = "k1";

    // all gets succeed, but multiplexed returns `None`
    {
        let mut get_fut = multiplex.get(&ctx, k).boxed();
        assert_pending(&mut get_fut).await;

        tickable_blobstores[0].1.tick(None);
        assert_pending(&mut get_fut).await;
        tickable_blobstores[1].1.tick(None);

        // the read-quorum on `None` achieved, multiplexed get returns `None`
        validate_blob(get_fut.await, Ok(None));
        tickable_blobstores[2].1.drain(1);
    }

    // two gets succeed, but multiplexed returns `None`
    {
        let mut get_fut = multiplex.get(&ctx, k).boxed();
        assert_pending(&mut get_fut).await;

        tickable_blobstores[0].1.tick(None);
        tickable_blobstores[1].1.tick(Some("bs1 failed"));
        // muliplexed get waits on the third
        assert_pending(&mut get_fut).await;
        tickable_blobstores[2].1.tick(None);

        // the read-quorum on `None` achieved, multiplexed get returns `None`
        validate_blob(get_fut.await, Ok(None));
    }

    // two gets fail, multiplexed get fails, because no read quorum
    {
        let mut get_fut = multiplex.get(&ctx, k).boxed();
        assert_pending(&mut get_fut).await;

        tickable_blobstores[0].1.tick(Some("bs0 failed"));
        tickable_blobstores[1].1.tick(Some("bs1 failed"));
        // muliplexed get waits on the third, which returns `None`
        assert_pending(&mut get_fut).await;
        tickable_blobstores[2].1.tick(None);

        // no read-quorum, multiplexed get fails
        validate_blob(get_fut.await, Err(anyhow::anyhow!("error")));
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_get_on_existing(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (tickable_queue, tickable_blobstores, multiplex) = setup_multiplex(3, 2, None)?;

    // Two blobstores have the key, one failed to write: [ ] [x] [ ]

    let v = make_value("v1");
    let k = "k1";

    let mut put_fut = multiplex.put(&ctx, k.to_owned(), v.clone()).boxed();
    assert_pending(&mut put_fut).await;

    // wal queue write succeeds
    tickable_queue.tick(None);
    assert_pending(&mut put_fut).await;

    tickable_blobstores[0].1.tick(None);
    tickable_blobstores[1].1.tick(Some("bs1 failed"));
    tickable_blobstores[2].1.tick(None);

    // multiplexed put succeeds: write quorum achieved
    assert!(put_fut.await.is_ok());

    // all gets succeed, but multiplexed returns on the first successful `Some`
    {
        let mut get_fut = multiplex.get(&ctx, k).boxed();
        assert_pending(&mut get_fut).await;

        // first blobstore returns the blob
        tickable_blobstores[0].1.tick(None);
        validate_blob(get_fut.await, Ok(Some(&v)));

        // drain the tickables of the pending requests, as they won't be claimed
        for (_id, store) in &tickable_blobstores[1..3] {
            store.drain(1);
        }
    }

    // first get fails, but multiplexed returns on the third get
    {
        let mut get_fut = multiplex.get(&ctx, k).boxed();
        assert_pending(&mut get_fut).await;

        // first blobstore get fails
        tickable_blobstores[0].1.tick(Some("bs1 failed!"));
        assert_pending(&mut get_fut).await;

        // second blobstore get returns None
        tickable_blobstores[1].1.tick(None);
        assert_pending(&mut get_fut).await;

        // third blobstore get returns Some
        tickable_blobstores[2].1.tick(None);
        validate_blob(get_fut.await, Ok(Some(&v)));
    }

    // 2 first gets fail, but multiplexed returns `Some` on the third get
    {
        let mut get_fut = multiplex.get(&ctx, k).boxed();
        assert_pending(&mut get_fut).await;

        // first blobstore get fails
        tickable_blobstores[0].1.tick(Some("bs1 failed!"));
        assert_pending(&mut get_fut).await;

        // second blobstore get fails
        tickable_blobstores[1].1.tick(Some("bs2 failed!"));
        assert_pending(&mut get_fut).await;

        // third blobstore get returns Some
        tickable_blobstores[2].1.tick(None);
        validate_blob(get_fut.await, Ok(Some(&v)));
    }

    // all blobstores that have the blob fail, multiplexed get fail:
    // no read quorum on `None` was achieved
    {
        let mut get_fut = multiplex.get(&ctx, k).boxed();
        assert_pending(&mut get_fut).await;

        // first and third blobstore gets fail
        tickable_blobstores[0].1.tick(Some("bs1 failed!"));
        tickable_blobstores[2].1.tick(Some("bs3 failed!"));
        assert_pending(&mut get_fut).await;

        // second blobstore get returns None
        tickable_blobstores[1].1.tick(None);
        validate_blob(get_fut.await, Err(anyhow::anyhow!("error")));
    }

    // all blobstores gets fail, multiplexed get fail
    {
        let mut get_fut = multiplex.get(&ctx, k).boxed();
        assert_pending(&mut get_fut).await;

        for (id, store) in tickable_blobstores {
            store.tick(Some(format!("bs{id} failed!").as_str()));
        }

        validate_blob(get_fut.await, Err(anyhow::anyhow!("error")));
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_is_present_missing(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (_tickable_queue, tickable_blobstores, multiplex) = setup_multiplex(3, 2, None)?;

    // No blobstores have the key
    let k = "k1";

    // all `is_present` succeed, multiplexed returns `Absent`
    {
        let mut fut = multiplex.is_present(&ctx, k).boxed();
        assert_pending(&mut fut).await;

        tickable_blobstores[0].1.tick(None);
        assert_pending(&mut fut).await;
        tickable_blobstores[1].1.tick(None);

        // the read-quorum on `None` achieved, multiplexed returns `Absent`
        assert_is_present_ok(fut.await, BlobstoreIsPresent::Absent);
        tickable_blobstores[2].1.drain(1);
    }

    // two `is_present`s succeed, multiplexed returns `Absent`
    {
        let mut fut = multiplex.is_present(&ctx, k).boxed();
        assert_pending(&mut fut).await;

        tickable_blobstores[0].1.tick(None);
        tickable_blobstores[1].1.tick(Some("bs1 failed"));
        // muliplexed is_present waits on the third
        assert_pending(&mut fut).await;
        tickable_blobstores[2].1.tick(None);

        // the read-quorum achieved, multiplexed returns `Absent`
        assert_is_present_ok(fut.await, BlobstoreIsPresent::Absent);
    }

    // two `is_present`s fail, multiplexed returns `ProbablyNotPresent`
    {
        let mut fut = multiplex.is_present(&ctx, k).boxed();
        assert_pending(&mut fut).await;

        tickable_blobstores[0].1.tick(Some("bs0 failed"));
        tickable_blobstores[1].1.tick(None);
        // muliplexed is_present waits on the third
        assert_pending(&mut fut).await;
        tickable_blobstores[2].1.tick(Some("bs2 failed"));

        assert_is_present_ok(
            fut.await,
            BlobstoreIsPresent::ProbablyNotPresent(anyhow!("some failed!")),
        );
    }

    // all `is_present`s fail, multiplexed fails
    {
        let mut fut = multiplex.is_present(&ctx, k).boxed();
        assert_pending(&mut fut).await;

        for (id, store) in tickable_blobstores {
            store.tick(Some(format!("bs{id} failed!").as_str()));
        }
        assert!(fut.await.is_err());
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_unlink(fb: FacebookInit) -> Result<()> {
    async fn setup_multiplex_for_unlink(
        ctx: &CoreContext,
    ) -> Result<(
        Vec<(BlobstoreId, Arc<Tickable<(BlobstoreBytes, u64)>>)>,
        WalMultiplexedBlobstore,
        &'static str,
    )> {
        let (tickable_queue, tickable_blobstores, multiplex) = setup_multiplex(3, 2, None)?;

        // Two blobstores have the key, one failed to write: [ ] [x] [ ]
        let v = make_value("v1");
        let k = "k1";

        let mut put_fut = multiplex.put(ctx, k.to_owned(), v.clone()).boxed();
        assert_pending(&mut put_fut).await;

        // wal queue write succeeds
        tickable_queue.tick(None);
        assert_pending(&mut put_fut).await;

        tickable_blobstores[0].1.tick(None);
        tickable_blobstores[1].1.tick(Some("bs1 failed"));
        tickable_blobstores[2].1.tick(None);

        // multiplexed put succeeds: write quorum achieved
        assert!(put_fut.await.is_ok());
        Ok((tickable_blobstores, multiplex, k))
    }

    let ctx = CoreContext::test_mock(fb);

    // all unlinks succeed, multiplexed unlink succeeds
    {
        let (tickable_blobstores, multiplex, k) = setup_multiplex_for_unlink(&ctx).await?;
        let mut unlink_fut = multiplex.unlink(&ctx, k).boxed();
        assert_pending(&mut unlink_fut).await;

        // first blobstore unlinks successfully
        tickable_blobstores[0].1.tick(None);
        assert_pending(&mut unlink_fut).await;
        // second blobstore unlinks successfully
        tickable_blobstores[1].1.tick(None);
        assert_pending(&mut unlink_fut).await;
        // third blobstore unlinks successfully
        tickable_blobstores[2].1.tick(None);
        assert!(unlink_fut.await.is_ok());
    }

    // all unlinks succeed, except blobstore 1 for which the write failed, multiplexed unlink succeeds
    {
        let (tickable_blobstores, multiplex, k) = setup_multiplex_for_unlink(&ctx).await?;
        let mut unlink_fut = multiplex.unlink(&ctx, k).boxed();
        assert_pending(&mut unlink_fut).await;

        // first blobstore unlinks successfully
        tickable_blobstores[0].1.tick(None);
        assert_pending(&mut unlink_fut).await;
        // second blobstore fails to unlink
        tickable_blobstores[1].1.tick(Some("bs1 failed"));
        assert_pending(&mut unlink_fut).await;
        // but the blob is absent, so it's a success anyway
        tickable_blobstores[1].1.tick(None);
        assert_pending(&mut unlink_fut).await;
        // third blobstore unlinks successfully
        tickable_blobstores[2].1.tick(None);
        assert!(unlink_fut.await.is_ok());
    }

    // all unlinks succeed, except blobstore 1 for which the write failed. The is_present call fails,
    // multiplexed unlink fails as we don't have a way to know that the blob is not present in blobstore 1
    {
        let (tickable_blobstores, multiplex, k) = setup_multiplex_for_unlink(&ctx).await?;
        let mut unlink_fut = multiplex.unlink(&ctx, k).boxed();
        assert_pending(&mut unlink_fut).await;

        // first blobstore unlinks successfully
        tickable_blobstores[0].1.tick(None);
        assert_pending(&mut unlink_fut).await;
        // second blobstore fails to unlink
        tickable_blobstores[1].1.tick(Some("bs1 unlink failed"));
        assert_pending(&mut unlink_fut).await;
        // and the is_present call fails
        tickable_blobstores[1].1.tick(Some("bs1 is_present failed"));
        assert_pending(&mut unlink_fut).await;
        // third blobstore unlinks successfully
        tickable_blobstores[2].1.tick(None);
        assert!(unlink_fut.await.is_err());
    }

    // all unlinks succeed, except blobstore 0 for which the write succeeded, multiplexed unlink
    // fails as the blob in blobstore 0 remains
    {
        let (tickable_blobstores, multiplex, k) = setup_multiplex_for_unlink(&ctx).await?;
        let mut unlink_fut = multiplex.unlink(&ctx, k).boxed();
        assert_pending(&mut unlink_fut).await;

        // first blobstore unlinks successfully
        tickable_blobstores[0].1.tick(Some("bs0 failed"));
        assert_pending(&mut unlink_fut).await;
        // and the blob is present, so the unlink failed
        tickable_blobstores[0].1.tick(None);
        assert_pending(&mut unlink_fut).await;
        // second blobstore fails to unlink
        tickable_blobstores[1].1.tick(None);
        assert_pending(&mut unlink_fut).await;
        // third blobstore unlinks successfully
        tickable_blobstores[2].1.tick(None);
        assert!(unlink_fut.await.is_err());
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_is_present_existing(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (tickable_queue, tickable_blobstores, multiplex) = setup_multiplex(3, 2, None)?;

    // Two blobstores have the key, one failed to write: [ ] [x] [ ]
    {
        let v = make_value("v1");
        let k = "k1";

        let mut put_fut = multiplex.put(&ctx, k.to_owned(), v.clone()).boxed();
        assert_pending(&mut put_fut).await;

        // wal queue write succeeds
        tickable_queue.tick(None);
        assert_pending(&mut put_fut).await;

        tickable_blobstores[0].1.tick(None);
        tickable_blobstores[1].1.tick(Some("bs1 failed"));
        tickable_blobstores[2].1.tick(None);

        // multiplexed put succeeds: write quorum achieved
        assert!(put_fut.await.is_ok());

        // first `is_present` succeed with `Present`, multiplexed returns `Present`
        {
            let mut fut = multiplex.is_present(&ctx, k).boxed();
            assert_pending(&mut fut).await;

            tickable_blobstores[0].1.tick(None);
            assert_is_present_ok(fut.await, BlobstoreIsPresent::Present);

            for (_id, store) in &tickable_blobstores[1..] {
                store.drain(1);
            }
        }

        // first `is_present` fails, second succeed with `Absent`, third returns `Present`
        // multiplexed returns `Present`
        {
            let mut fut = multiplex.is_present(&ctx, k).boxed();
            assert_pending(&mut fut).await;

            tickable_blobstores[0].1.tick(Some("bs0 failed"));
            tickable_blobstores[1].1.tick(None);
            assert_pending(&mut fut).await;

            tickable_blobstores[2].1.tick(None);
            assert_is_present_ok(fut.await, BlobstoreIsPresent::Present);
        }
    }

    // Two blobstores failed to write, one succeeded: [x] [ ] [x]
    {
        let v = make_value("v2");
        let k = "k2";

        let mut put_fut = multiplex.put(&ctx, k.to_owned(), v.clone()).boxed();
        assert_pending(&mut put_fut).await;

        // wal queue write succeeds
        tickable_queue.tick(None);
        assert_pending(&mut put_fut).await;

        tickable_blobstores[0].1.tick(Some("bs0 failed"));
        tickable_blobstores[1].1.tick(None);
        tickable_blobstores[2].1.tick(Some("bs2 failed"));

        // multiplexed put failed: no write quorum
        assert!(put_fut.await.is_err());

        // the first `is_present` to succeed returns `Present`, multiplexed returns `Present`
        {
            let mut fut = multiplex.is_present(&ctx, k).boxed();
            assert_pending(&mut fut).await;

            tickable_blobstores[1].1.tick(None);
            assert_is_present_ok(fut.await, BlobstoreIsPresent::Present);

            tickable_blobstores[0].1.drain(1);
            tickable_blobstores[2].1.drain(1);
        }

        // if the first two `is_present` calls return `Absent`, multiplexed returns `Absent`
        {
            let mut fut = multiplex.is_present(&ctx, k).boxed();
            assert_pending(&mut fut).await;

            tickable_blobstores[0].1.tick(None);
            tickable_blobstores[2].1.tick(None);

            assert_is_present_ok(fut.await, BlobstoreIsPresent::Absent);
            tickable_blobstores[1].1.drain(1);
        }

        // if one `is_present` returns `Absent`, another 2 fail, multiplexed is unsure
        {
            let mut fut = multiplex.is_present(&ctx, k).boxed();
            assert_pending(&mut fut).await;

            tickable_blobstores[0].1.tick(None);
            for (id, store) in &tickable_blobstores[1..] {
                store.tick(Some(format!("bs{id} failed").as_str()));
            }

            assert_is_present_ok(
                fut.await,
                BlobstoreIsPresent::ProbablyNotPresent(anyhow!("some failed!")),
            );
        }
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_timeout_on_request(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);

    // Ensure that even if the quorum writes succeeded, the rest of the writes are not
    // being dropped.
    {
        let timeout = MultiplexTimeout {
            // we want writes to succeed
            write: Duration::from_secs(10),
            // and reads to fail because of timeout
            read: Duration::from_millis(5),
        };
        let (tickable_queue, tickable_blobstores, multiplex) =
            setup_multiplex(3, 2, Some(timeout))?;

        let v = make_value("v1");
        let k = "k1";

        let mut put_fut = multiplex.put(&ctx, k.to_owned(), v.clone()).boxed();
        assert_pending(&mut put_fut).await;

        // wal queue write succeeds
        tickable_queue.tick(None);
        assert_pending(&mut put_fut).await;

        // quorum puts succeed -> the multiplexed put succeeds
        tickable_blobstores[0].1.tick(None);
        tickable_blobstores[1].1.tick(None);
        assert!(put_fut.await.is_ok());

        // Now we'll try to respond to the last put, that haven't completed.
        // It should not panic, as the future is still in flight and still
        // waits for the response.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let result = panic::catch_unwind(|| tickable_blobstores[2].1.tick(None));
        assert!(result.is_ok());

        // We'll try to read with a delayed response from the blobstores.
        // Get should fail.
        let mut fut = multiplex.get(&ctx, k).boxed();
        assert_pending(&mut fut).await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(fut.await.is_err());
    }

    // Ensure that after the quorum writes succeeded, the rest of the writes are
    // dropped because of the timeout.
    {
        // set write timeout to a very low value
        let timeout = MultiplexTimeout::new(
            None,                           /* read */
            Some(Duration::from_millis(5)), /* write */
        );
        let (tickable_queue, tickable_blobstores, multiplex) =
            setup_multiplex(3, 2, Some(timeout))?;

        let v = make_value("v2");
        let k = "k2";

        let mut put_fut = multiplex.put(&ctx, k.to_owned(), v.clone()).boxed();
        assert_pending(&mut put_fut).await;

        // wal queue write succeeds
        tickable_queue.tick(None);
        assert_pending(&mut put_fut).await;

        // quorum puts succeed -> the multiplexed put succeeds
        tickable_blobstores[0].1.tick(None);
        tickable_blobstores[1].1.tick(None);
        assert!(put_fut.await.is_ok());

        // Now we'll delay the response again and respond to the last put,
        // that haven't completed.
        // It *should* panic, as the future was dropped due to the timeout.
        tokio::time::sleep(Duration::from_millis(25)).await;
        let result = panic::catch_unwind(|| tickable_blobstores[2].1.tick(None));
        assert!(result.is_err());
    }

    Ok(())
}

async fn assert_pending<T: Debug>(fut: &mut (impl Future<Output = T> + Unpin)) {
    match futures::poll!(fut) {
        Poll::Pending => {}
        state => {
            panic!("future must be pending, received: {state:?}");
        }
    }
}

fn setup_multiplex(
    num: u64,
    quorum: usize,
    timeout: Option<MultiplexTimeout>,
) -> Result<(
    Arc<Tickable<BlobstoreWalEntry>>,
    Vec<(BlobstoreId, Arc<Tickable<(BlobstoreBytes, u64)>>)>,
    WalMultiplexedBlobstore,
)> {
    let (tickable_queue, wal_queue) = setup_queue();
    let (tickable_blobstores, blobstores) = setup_blobstores(num);
    let scuba = Scuba::new(
        MononokeScubaSampleBuilder::with_discard(),
        MononokeScubaSampleBuilder::with_discard(),
        nonzero!(1u64),
    )?;
    let multiplex = WalMultiplexedBlobstore::new(
        MultiplexId::new(1),
        wal_queue,
        blobstores,
        vec![],
        quorum,
        timeout,
        scuba,
    )?;

    Ok((tickable_queue, tickable_blobstores, multiplex))
}

type TickableBytes = Tickable<(BlobstoreBytes, u64)>;

fn setup_blobstores(
    num: u64,
) -> (
    Vec<(BlobstoreId, Arc<TickableBytes>)>,
    Vec<(BlobstoreId, Arc<dyn Blobstore>)>,
) {
    let tickable_blobstores: Vec<_> = (0..num)
        .map(|id| (BlobstoreId::new(id), Arc::new(TickableBytes::new())))
        .collect();
    let blobstores = tickable_blobstores
        .clone()
        .into_iter()
        .map(|(id, store)| (id, store as Arc<dyn Blobstore>))
        .collect();
    (tickable_blobstores, blobstores)
}

fn setup_queue() -> (Arc<Tickable<BlobstoreWalEntry>>, Arc<dyn BlobstoreWal>) {
    let tickable_queue = Arc::new(Tickable::new());
    let wal_queue = tickable_queue.clone() as Arc<dyn BlobstoreWal>;
    (tickable_queue, wal_queue)
}

fn make_value(value: &str) -> BlobstoreBytes {
    BlobstoreBytes::from_bytes(Bytes::copy_from_slice(value.as_bytes()))
}

fn validate_blob(
    get_data: Result<Option<BlobstoreGetData>>,
    expected: Result<Option<&BlobstoreBytes>>,
) {
    assert_eq!(get_data.is_ok(), expected.is_ok());
    if let Ok(expected) = expected {
        let get_data = get_data.unwrap().map(|data| data.into_bytes());
        assert_eq!(get_data.as_ref(), expected);
    }
}

fn assert_is_present_ok(result: Result<BlobstoreIsPresent>, expected: BlobstoreIsPresent) {
    assert!(result.is_ok());
    match (result.unwrap(), expected) {
        (BlobstoreIsPresent::Absent, BlobstoreIsPresent::Absent)
        | (BlobstoreIsPresent::Present, BlobstoreIsPresent::Present)
        | (BlobstoreIsPresent::ProbablyNotPresent(_), BlobstoreIsPresent::ProbablyNotPresent(_)) => {
        }
        (res, exp) => {
            panic!("`is_present` call must return {exp:?}, received: {res:?}");
        }
    }
}

async fn scrub_none(
    fb: FacebookInit,
    scrub_action_on_missing_write_only: SrubWriteOnly,
) -> Result<()> {
    let bid0 = BlobstoreId::new(0);
    let bs0 = Arc::new(Tickable::new());
    let bid1 = BlobstoreId::new(1);
    let bs1 = Arc::new(Tickable::new());
    let bid2 = BlobstoreId::new(2);
    let bs2 = Arc::new(Tickable::new());

    let (_, queue) = setup_queue();
    let ctx = CoreContext::test_mock(fb);
    borrowed!(ctx);
    let bs = WalScrubBlobstore::new(
        MultiplexId::new(1),
        queue,
        vec![(bid0, bs0.clone()), (bid1, bs1.clone())],
        vec![(bid2, bs2.clone())],
        1,
        None,
        Scuba::new_from_raw(fb, None, None, nonzero!(1u64))?,
        ScrubOptions {
            scrub_action_on_missing_write_only,
            ..ScrubOptions::default()
        },
        Arc::new(LoggingScrubHandler::new(false)) as Arc<dyn ScrubHandler>,
    )?;

    let mut fut = bs.get(ctx, "key");
    assert!(futures::poll!(&mut fut).is_pending());

    // No entry for "key" - blobstores return None...
    bs0.tick(None);
    bs1.tick(None);
    // Expect a read from writemostly stores regardless
    bs2.tick(None);

    fut.await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn scrub_blobstore_fetch_none(fb: FacebookInit) -> Result<()> {
    scrub_none(fb, SrubWriteOnly::Scrub).await?;
    scrub_none(fb, SrubWriteOnly::SkipMissing).await?;
    scrub_none(fb, SrubWriteOnly::PopulateIfAbsent).await
}
async fn scrub_scenarios(
    fb: FacebookInit,
    scrub_action_on_missing_write_only: SrubWriteOnly,
) -> Result<()> {
    use SrubWriteOnly::*;
    println!("{scrub_action_on_missing_write_only:?}");
    let ctx = CoreContext::test_mock(fb);
    borrowed!(ctx);
    let (tick_queue, queue) = setup_queue();
    let scrub_handler = Arc::new(LoggingScrubHandler::new(false)) as Arc<dyn ScrubHandler>;
    let bid0 = BlobstoreId::new(0);
    let bs0 = Arc::new(Tickable::new());
    let bid1 = BlobstoreId::new(1);
    let bs1 = Arc::new(Tickable::new());
    let bid2 = BlobstoreId::new(2);
    let bs2 = Arc::new(Tickable::new());
    let scuba = Scuba::new_from_raw(fb, None, None, nonzero!(1u64))?;
    let bs = WalScrubBlobstore::new(
        MultiplexId::new(1),
        queue.clone(),
        vec![(bid0, bs0.clone()), (bid1, bs1.clone())],
        vec![(bid2, bs2.clone())],
        1,
        None,
        scuba.clone(),
        ScrubOptions {
            scrub_action: ScrubAction::ReportOnly,
            scrub_action_on_missing_write_only,
            ..ScrubOptions::default()
        },
        scrub_handler.clone(),
    )?;

    // non-existing key when one main blobstore failing
    {
        let k0 = "k0";

        let mut get_fut = bs.get(ctx, k0).boxed();
        assert_pending(&mut get_fut).await;

        bs0.tick(None);
        assert_pending(&mut get_fut).await;

        bs1.tick(Some("bs1 failed"));

        bs2.tick(None);
        assert!(get_fut.await.is_err(), "SomeNone + Err expected Err");
    }

    // non-existing key when one write only blobstore failing
    {
        let k0 = "k0";

        let mut get_fut = bs.get(ctx, k0).boxed();
        assert_pending(&mut get_fut).await;

        bs0.tick(None);
        assert_pending(&mut get_fut).await;

        bs1.tick(None);

        match scrub_action_on_missing_write_only {
            PopulateIfAbsent | ScrubIfAbsent => {
                // bs2 is ignored because it's write_only and the result of the normal
                // blobstores is failing
                assert_eq!(get_fut.await.unwrap(), None, "SomeNone + Err expected None");
            }
            _ => {
                bs2.tick(Some("bs2 failed"));
                // Write mostly blobstore failed but we still have quorum to say
                // the get succeeded
                assert_eq!(get_fut.await.unwrap(), None, "SomeNone + Err expected None");
            }
        }
    }

    // fail all but one store on put to make sure only one has the data
    // only replica containing key fails on read.
    {
        let v1 = make_value("v1");
        let k1 = "k1";

        let mut put_fut = bs.put(ctx, k1.to_owned(), v1.clone()).boxed();
        assert_pending(&mut put_fut).await;
        tick_queue.tick(None);
        assert_pending(&mut put_fut).await;
        bs0.tick(None);
        bs1.tick(Some("bs1 failed"));
        bs2.tick(Some("bs2 failed"));
        put_fut.await.unwrap();

        assert_eq!(
            queue
                .read(ctx, &MultiplexId::new(1), &Timestamp::now(), 2)
                .await
                .unwrap()
                .len(),
            1
        );

        let mut get_fut = bs.get(ctx, k1).boxed();
        assert_pending(&mut get_fut).await;

        bs0.tick(Some("bs0 failed"));
        assert_pending(&mut get_fut).await;

        bs1.tick(None);
        bs2.tick(None);
        assert!(get_fut.await.is_err(), "None/Err while replicating");
    }

    // all replicas fail
    {
        let k2 = "k2";

        let mut get_fut = bs.get(ctx, k2).boxed();
        assert_pending(&mut get_fut).await;
        bs0.tick(Some("bs0 failed"));
        bs1.tick(Some("bs1 failed"));
        bs2.tick(Some("bs1 failed"));
        assert!(get_fut.await.is_err(), "Err/Err");
    }

    // Now replace bs1 & bs2 with an empty blobstore, and see the scrub work
    let bid1 = BlobstoreId::new(1);
    let bs1 = Arc::new(Tickable::new());
    let bid2 = BlobstoreId::new(2);
    let bs2 = Arc::new(Tickable::new());
    let bs = WalScrubBlobstore::new(
        MultiplexId::new(1),
        queue.clone(),
        vec![(bid0, bs0.clone()), (bid1, bs1.clone())],
        vec![(bid2, bs2.clone())],
        1,
        None,
        scuba.clone(),
        ScrubOptions {
            scrub_action: ScrubAction::Repair,
            scrub_action_on_missing_write_only,
            ..ScrubOptions::default()
        },
        scrub_handler.clone(),
    )?;

    // Non-existing key in all blobstores, new blobstore failing
    {
        let k0 = "k0";

        let mut get_fut = bs.get(ctx, k0).boxed();
        assert_pending(&mut get_fut).await;

        bs0.tick(None);
        assert_pending(&mut get_fut).await;

        bs1.tick(Some("bs1 failed"));

        bs2.tick(None);
        assert!(get_fut.await.is_err(), "None/Err after replacement");
    }

    // only replica containing key replaced after failure - DATA LOST
    {
        let k1 = "k1";

        let mut get_fut = bs.get(ctx, k1).boxed();
        assert_pending(&mut get_fut).await;
        bs0.tick(Some("bs0 failed"));
        bs1.tick(None);
        bs2.tick(None);
        assert!(get_fut.await.is_err(), "Empty replacement against error");
    }

    // One working replica after failure.
    {
        let v1 = make_value("v1");
        let k1 = "k1";

        match queue
            .read(ctx, &MultiplexId::new(1), &Timestamp::now(), 2)
            .await
            .unwrap()
            .as_slice()
        {
            [entry] => {
                let to_del = [entry.clone()];
                let mut fut = queue.delete(ctx, &to_del).boxed();
                assert!(futures::poll!(&mut fut).is_pending());
                tick_queue.tick(None);
                fut.await.unwrap()
            }
            _ => panic!("only one entry expected"),
        }

        // bs1 and bs2 empty at this point
        assert_eq!(bs0.get_bytes(k1), Some(v1.clone()));
        assert!(bs1.storage.with(|s| s.is_empty()));
        assert!(bs2.storage.with(|s| s.is_empty()));

        let mut get_fut = bs.get(ctx, k1).boxed();
        assert_pending(&mut get_fut).await;
        // tick the gets
        bs0.tick(None);
        assert_pending(&mut get_fut).await;
        bs1.tick(None);
        if scrub_action_on_missing_write_only != SrubWriteOnly::PopulateIfAbsent {
            // this read doesn't happen in this mode
            bs2.tick(None);
        }
        assert_pending(&mut get_fut).await;
        // Tick the repairs
        bs1.tick(None);
        bs2.tick(None);

        // Succeeds
        assert_eq!(get_fut.await.unwrap().map(|v| v.into()), Some(v1.clone()));
        // Now all populated.
        assert_eq!(bs0.get_bytes(k1), Some(v1.clone()));
        assert_eq!(bs1.get_bytes(k1), Some(v1.clone()));
        match scrub_action_on_missing_write_only {
            SrubWriteOnly::Scrub
            | SrubWriteOnly::PopulateIfAbsent
            | SrubWriteOnly::ScrubIfAbsent => {
                assert_eq!(bs2.get_bytes(k1), Some(v1.clone()))
            }
            SrubWriteOnly::SkipMissing => {
                assert_eq!(bs2.get_bytes(k1), None)
            }
        }
    }

    // Main blobstore gets ignored on failure
    {
        let v2 = make_value("v2");
        let k2 = "k2";

        bs0.add_bytes(k2.to_string(), v2.clone());
        // bs1 and bs2 don't have k2
        assert_eq!(bs0.get_bytes(k2), Some(v2.clone()));
        assert_eq!(bs1.get_bytes(k2), None);
        assert_eq!(bs2.get_bytes(k2), None);

        let mut get_fut = bs.get(ctx, k2).boxed();
        assert_pending(&mut get_fut).await;
        // gets
        bs0.tick(None);
        bs1.tick(Some("bs1 get failed"));
        if scrub_action_on_missing_write_only != SrubWriteOnly::PopulateIfAbsent {
            // this read doesn't happen in this mode
            bs2.tick(None);
        }
        if scrub_action_on_missing_write_only != SrubWriteOnly::SkipMissing {
            assert_pending(&mut get_fut).await;
            // repair bs2
            bs2.tick(None);
        }
        assert_eq!(get_fut.await.unwrap().map(|v| v.into()), Some(v2.clone()));
        // bs1 still doesn't have the value. This is expected because we don't
        // want a single failing blobstore blocking the scrub.
        assert_eq!(bs1.get_bytes(k2), None);
        if scrub_action_on_missing_write_only != SrubWriteOnly::SkipMissing {
            // bs2 got repaired successfully
            assert_eq!(bs2.get_bytes(k2), Some(v2.clone()));
        } else {
            assert_eq!(bs2.get_bytes(k2), None);
        }
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn scrubbed(fb: FacebookInit) {
    scrub_scenarios(fb, SrubWriteOnly::Scrub).await.unwrap();
    scrub_scenarios(fb, SrubWriteOnly::SkipMissing)
        .await
        .unwrap();
    scrub_scenarios(fb, SrubWriteOnly::PopulateIfAbsent)
        .await
        .unwrap();
}

/// A multiplexed `unlink` must remove the key from the write-only blobstores as well as from the
/// main ones: any copy left behind can be healed back into the other blobstores.
#[mononoke::fbinit_test]
async fn test_unlink_removes_from_write_only_blobstores(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (tickable_queue, wal_queue) = setup_queue();
    let (tickable_main, main_blobstores) = setup_blobstores(2);
    let write_only_tickable = Arc::new(TickableBytes::new());
    let write_only: Vec<(BlobstoreId, Arc<dyn Blobstore>)> = vec![(
        BlobstoreId::new(2),
        write_only_tickable.clone() as Arc<dyn Blobstore>,
    )];
    let scuba = Scuba::new(
        MononokeScubaSampleBuilder::with_discard(),
        MononokeScubaSampleBuilder::with_discard(),
        nonzero!(1u64),
    )?;
    let multiplex = WalMultiplexedBlobstore::new(
        MultiplexId::new(1),
        wal_queue,
        main_blobstores,
        write_only,
        1,
        None,
        scuba,
    )?;

    let v = make_value("v1");
    let k = "k1";

    // put: WAL log, then both main stores succeed; the write-only put is spawned after the quorum.
    let mut put_fut = multiplex.put(&ctx, k.to_owned(), v.clone()).boxed();
    assert_pending(&mut put_fut).await;
    tickable_queue.tick(None);
    assert_pending(&mut put_fut).await;
    tickable_main[0].1.tick(None);
    tickable_main[1].1.tick(None);
    assert!(put_fut.await.is_ok());
    // Let the spawned write-only put register on its tickable, then complete it.
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    write_only_tickable.tick(None);
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The WAL-delete optimisation runs once every put succeeded.
    tickable_queue.tick(None);
    tokio::task::yield_now().await;
    assert_eq!(
        write_only_tickable.get_bytes(k),
        Some(v.clone()),
        "write-only store received the blob"
    );

    // unlink through the multiplex: the main stores and the write-only store are all asked.
    let mut unlink_fut = multiplex.unlink(&ctx, k).boxed();
    assert_pending(&mut unlink_fut).await;
    tickable_main[0].1.tick(None);
    tickable_main[1].1.tick(None);
    assert_pending(&mut unlink_fut).await;
    write_only_tickable.tick(None);
    assert!(unlink_fut.await.is_ok());

    assert_eq!(tickable_main[0].1.get_bytes(k), None);
    assert_eq!(tickable_main[1].1.get_bytes(k), None);
    assert_eq!(write_only_tickable.get_bytes(k), None);
    Ok(())
}
