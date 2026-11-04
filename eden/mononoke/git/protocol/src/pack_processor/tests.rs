/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::stream;
use git_types::PackfileItem;
use gix_hash::ObjectId;
use gix_pack::data::output::Entry;
use mononoke_macros::mononoke;
use packfile::pack::DeltaForm;
use packfile::pack::PackfileWriter;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreArc;
use sha1::Digest;
use sha1::Sha1;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio::time::timeout;
use weight_observer::WeightObserver;

use super::parse_pack;

mod scan_validation;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const PAYLOAD: &[u8] = b"hello";

#[facet::container]
struct TestRepo(RepoBlobstore);

#[derive(Default)]
struct RecordingObserver {
    added: AtomicUsize,
    removed: AtomicUsize,
    add_calls: AtomicUsize,
    remove_calls: AtomicUsize,
    released: Notify,
}

impl RecordingObserver {
    fn counts(&self) -> (usize, usize) {
        (
            self.added.load(Ordering::SeqCst),
            self.removed.load(Ordering::SeqCst),
        )
    }
}

impl WeightObserver for RecordingObserver {
    fn on_weight_added(&self, weight: usize) {
        self.added.fetch_add(weight, Ordering::SeqCst);
        self.add_calls.fetch_add(1, Ordering::SeqCst);
    }

    fn on_weight_removed(&self, weight: usize) {
        self.removed.fetch_add(weight, Ordering::SeqCst);
        self.remove_calls.fetch_add(1, Ordering::SeqCst);
        self.released.notify_one();
    }
}

fn loose_blob(payload: &[u8]) -> Bytes {
    let mut bytes = format!("blob {}\0", payload.len()).into_bytes();
    bytes.extend_from_slice(payload);
    bytes.into()
}

fn blob_id(payload: &[u8]) -> ObjectId {
    ObjectId::from_bytes_or_panic(Sha1::digest(loose_blob(payload)).as_slice())
}

async fn make_pack(items: Vec<PackfileItem>) -> Result<Vec<u8>> {
    let mut writer = PackfileWriter::new(
        Vec::new(),
        items.len().try_into()?,
        1,
        DeltaForm::RefAndOffset,
        None,
    );
    writer
        .write_unweighted(stream::iter(items.into_iter().map(Ok)))
        .await?;
    writer.finish().await?;
    Ok(writer.into_write())
}

async fn blob_pack() -> Result<Vec<u8>> {
    make_pack(vec![PackfileItem::new_base(loose_blob(PAYLOAD))?]).await
}

fn ref_delta(base: &[u8], target: &[u8]) -> Result<PackfileItem> {
    assert!(base.len() < 128 && !target.is_empty() && target.len() < 128);
    // A small delta containing only a literal insertion.
    let mut instructions = vec![base.len() as u8, target.len() as u8, target.len() as u8];
    instructions.extend_from_slice(target);
    let encoded: Entry = PackfileItem::new_base(loose_blob(&instructions))?.try_into()?;
    Ok(PackfileItem::new_delta(
        blob_id(target),
        blob_id(base),
        instructions.len() as u64,
        encoded.compressed_data,
    ))
}

#[mononoke::fbinit_test]
async fn parsed_objects_keep_weight_until_guard_drop(fb: FacebookInit) -> Result<()> {
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);
    let pack = blob_pack().await?;
    for tracked in [false, true] {
        let observer = Arc::new(RecordingObserver::default());
        let observer_arg = tracked.then(|| observer.clone() as Arc<dyn WeightObserver>);
        let (objects, guard) =
            parse_pack(&pack, &ctx, repo.repo_blobstore_arc(), 1, observer_arg).await?;
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[&blob_id(PAYLOAD)].raw(), &loose_blob(PAYLOAD));
        let weight = if tracked { PAYLOAD.len() } else { 0 };
        assert_eq!(guard.weight, weight);
        assert_eq!(observer.counts(), (weight, 0));
        drop(objects);
        assert_eq!(observer.counts(), (weight, 0));
        drop(guard);
        assert_eq!(observer.counts(), (weight, weight));
        assert_eq!(
            observer.remove_calls.load(Ordering::SeqCst),
            usize::from(tracked)
        );
    }
    Ok(())
}

struct GatedObserver {
    observer: Arc<RecordingObserver>,
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl WeightObserver for GatedObserver {
    fn on_weight_added(&self, weight: usize) {
        self.observer.on_weight_added(weight);
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
            self.release
                .lock()
                .unwrap()
                .recv_timeout(EVENT_TIMEOUT)
                .expect("blocking decode must be released before the finite gate timeout");
        }
    }

    fn on_weight_removed(&self, weight: usize) {
        self.observer.on_weight_removed(weight);
    }
}

struct ReleaseOnDrop(Option<mpsc::Sender<()>>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            let _ = release.send(());
        }
    }
}

#[mononoke::fbinit_test]
async fn cancelled_parse_releases_successful_blocking_decode_weight(
    fb: FacebookInit,
) -> Result<()> {
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);
    let pack = blob_pack().await?;
    let observer = Arc::new(RecordingObserver::default());
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release = ReleaseOnDrop(Some(release_tx));
    let gated = Arc::new(GatedObserver {
        observer: observer.clone(),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
    });
    let mut parse = Box::pin(parse_pack(
        &pack,
        &ctx,
        repo.repo_blobstore_arc(),
        1,
        Some(gated),
    ));
    tokio::select! {
        _ = &mut parse => panic!("parse returned before the decoder gate was released"),
        entered = timeout(EVENT_TIMEOUT, entered_rx) => entered??,
    }
    assert_eq!(observer.counts(), (PAYLOAD.len(), 0));
    drop(parse);
    assert_eq!(observer.counts(), (PAYLOAD.len(), 0));
    drop(release);
    timeout(EVENT_TIMEOUT, observer.released.notified()).await?;
    assert_eq!(observer.counts(), (PAYLOAD.len(), PAYLOAD.len()));
    assert_eq!(observer.remove_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[mononoke::fbinit_test]
async fn invalid_decoded_object_releases_weight(fb: FacebookInit) -> Result<()> {
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);
    let observer = Arc::new(RecordingObserver::default());
    let mut pack = blob_pack().await?;
    // Keep valid zlib/checksum framing but parse the blob bytes as an invalid tree.
    pack[12] = (pack[12] & !0x70) | 0x20;
    let trailer = pack.len() - 20;
    let checksum = Sha1::digest(&pack[..trailer]);
    pack[trailer..].copy_from_slice(checksum.as_slice());
    assert!(
        parse_pack(
            &pack,
            &ctx,
            repo.repo_blobstore_arc(),
            1,
            Some(observer.clone())
        )
        .await
        .is_err()
    );
    assert_eq!(observer.counts(), (PAYLOAD.len(), PAYLOAD.len()));
    assert_eq!(observer.remove_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

struct PanicAfterCharge(Arc<RecordingObserver>);

impl WeightObserver for PanicAfterCharge {
    fn on_weight_added(&self, weight: usize) {
        self.0.on_weight_added(weight);
        panic!("injected unwind after recording the decoded object charge");
    }

    fn on_weight_removed(&self, weight: usize) {
        self.0.on_weight_removed(weight);
    }
}

#[mononoke::fbinit_test]
async fn panicking_decode_releases_weight(fb: FacebookInit) -> Result<()> {
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);
    let observer = Arc::new(RecordingObserver::default());
    let pack = blob_pack().await?;
    assert!(
        parse_pack(
            &pack,
            &ctx,
            repo.repo_blobstore_arc(),
            1,
            Some(Arc::new(PanicAfterCharge(observer.clone()))),
        )
        .await
        .is_err()
    );
    assert_eq!(observer.counts(), (PAYLOAD.len(), PAYLOAD.len()));
    assert_eq!(observer.remove_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[mononoke::fbinit_test]
async fn ref_delta_passes_do_not_recharge_processed_objects(fb: FacebookInit) -> Result<()> {
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);
    let observer = Arc::new(RecordingObserver::default());
    let target = b"hello world";
    let pack = make_pack(vec![
        PackfileItem::new_base(loose_blob(PAYLOAD))?,
        ref_delta(PAYLOAD, target)?,
    ])
    .await?;
    let (objects, guard) = parse_pack(
        &pack,
        &ctx,
        repo.repo_blobstore_arc(),
        1,
        Some(observer.clone()),
    )
    .await?;
    assert_eq!(objects.len(), 2);
    assert_eq!(objects[&blob_id(PAYLOAD)].raw(), &loose_blob(PAYLOAD));
    assert_eq!(objects[&blob_id(target)].raw(), &loose_blob(target));
    let weight = PAYLOAD.len() + target.len();
    assert_eq!(guard.weight, weight);
    assert_eq!(observer.counts(), (weight, 0));
    assert_eq!(observer.add_calls.load(Ordering::SeqCst), 2);
    drop((objects, guard));
    assert_eq!(observer.counts(), (weight, weight));
    assert_eq!(observer.remove_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[mononoke::fbinit_test]
async fn unresolved_delta_depth_limit_releases_prior_weight(fb: FacebookInit) -> Result<()> {
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);
    let observer = Arc::new(RecordingObserver::default());
    let pack = make_pack(vec![
        PackfileItem::new_base(loose_blob(PAYLOAD))?,
        ref_delta(b"missing base", b"new blob")?,
    ])
    .await?;
    let result = parse_pack(
        &pack,
        &ctx,
        repo.repo_blobstore_arc(),
        1,
        Some(observer.clone()),
    )
    .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("unresolved delta unexpectedly parsed"),
    };
    assert!(format!("{error:#}").contains("Maximum allowed depth reached"));
    assert_eq!(observer.counts(), (PAYLOAD.len(), PAYLOAD.len()));
    assert_eq!(observer.add_calls.load(Ordering::SeqCst), 1);
    assert_eq!(observer.remove_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[mononoke::fbinit_test]
async fn empty_and_invalid_checksum_packs_do_not_charge(fb: FacebookInit) -> Result<()> {
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);
    let observer = Arc::new(RecordingObserver::default());
    let result = parse_pack(
        &[],
        &ctx,
        repo.repo_blobstore_arc(),
        1,
        Some(observer.clone()),
    )
    .await?;
    assert!(result.0.is_empty());
    assert_eq!(result.1.weight, 0);
    drop(result);
    let mut pack = blob_pack().await?;
    *pack.last_mut().unwrap() ^= 1;
    assert!(
        parse_pack(
            &pack,
            &ctx,
            repo.repo_blobstore_arc(),
            1,
            Some(observer.clone())
        )
        .await
        .is_err()
    );
    assert_eq!(observer.counts(), (0, 0));
    assert_eq!(observer.add_calls.load(Ordering::SeqCst), 0);
    assert_eq!(observer.remove_calls.load(Ordering::SeqCst), 0);
    Ok(())
}
