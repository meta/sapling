/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Result;
use blobstore::KeyedBlobstore;
use bytes::Bytes;
use context::CoreContext;
use fbinit::FacebookInit;
use filestore::FilestoreConfig;
use filestore::StoreRequest;
use futures::stream;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreBytes;
use mononoke_types::hash::RichGitSha1;
use repo_blobstore::RepoBlobstoreArc;
use sha1::Digest;
use sha1::Sha1;

use super::PAYLOAD;
use super::RecordingObserver;
use super::TestRepo;
use super::blob_id;
use super::blob_pack;
use super::loose_blob;
use super::make_pack;
use super::parse_pack;
use super::ref_delta;

#[mononoke::fbinit_test]
async fn external_ref_delta_keeps_seeded_base_without_charging_it(fb: FacebookInit) -> Result<()> {
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);
    let id = blob_id(PAYLOAD);
    let sha: [u8; 20] = id.as_bytes().try_into()?;
    let request = StoreRequest::with_git_sha1(
        PAYLOAD.len() as u64,
        RichGitSha1::from_byte_array(sha, "blob", PAYLOAD.len() as u64),
    );
    filestore::store(
        &repo.repo_blobstore_arc(),
        FilestoreConfig {
            chunk_size: None,
            concurrency: 1,
        },
        &ctx,
        &request,
        stream::once(async { Ok(Bytes::from_static(PAYLOAD)) }),
    )
    .await?;
    let targets = [b"hello world".as_slice(), b"other target".as_slice()];
    let pack = make_pack(
        targets
            .iter()
            .map(|target| ref_delta(PAYLOAD, target))
            .collect::<Result<Vec<_>>>()?,
    )
    .await?;
    let observer = Arc::new(RecordingObserver::default());
    let (objects, guard) = parse_pack(
        &pack,
        &ctx,
        repo.repo_blobstore_arc(),
        2,
        Some(observer.clone()),
    )
    .await?;
    assert_eq!(objects.len(), 3);
    assert_eq!(objects[&id].raw(), &loose_blob(PAYLOAD));
    for target in targets {
        assert_eq!(objects[&blob_id(target)].raw(), &loose_blob(target));
    }
    let weight = targets.iter().map(|target| target.len()).sum();
    assert_eq!(guard.weight, weight);
    assert_eq!(observer.counts(), (weight, 0));
    drop((objects, guard));
    assert_eq!(observer.counts(), (weight, weight));
    Ok(())
}

#[mononoke::fbinit_test]
async fn corrupt_external_base_is_unavailable_without_weight_leak(fb: FacebookInit) -> Result<()> {
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);
    let id = blob_id(PAYLOAD);
    repo.repo_blobstore_arc()
        .put(
            &ctx,
            format!("git_object.{}", id.to_hex()),
            BlobstoreBytes::from_bytes(Bytes::from_static(b"not a loose object")),
        )
        .await?;
    let pack = make_pack(vec![ref_delta(PAYLOAD, b"target")?]).await?;
    let observer = Arc::new(RecordingObserver::default());
    let error = match parse_pack(
        &pack,
        &ctx,
        repo.repo_blobstore_arc(),
        2,
        Some(observer.clone()),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("corrupt external base unexpectedly resolved"),
    };
    assert!(format!("{error:#}").contains("Maximum allowed depth reached"));
    assert_eq!(observer.counts(), (0, 0));
    Ok(())
}

fn replace_checksum(pack: &mut [u8]) {
    let trailer = pack.len() - 20;
    let checksum = Sha1::digest(&pack[..trailer]);
    pack[trailer..].copy_from_slice(checksum.as_slice());
}

#[mononoke::fbinit_test]
async fn validated_entry_scan_rejects_bad_zlib_size_and_type(fb: FacebookInit) -> Result<()> {
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);
    let original = blob_pack().await?;
    for invalid in ["zlib", "size", "type", "truncated_header", "truncated_zlib"] {
        let mut pack = original.clone();
        match invalid {
            "zlib" => pack[13] = 0,
            "size" => pack[12] = (pack[12] & !0x0f) | 6,
            "type" => pack[12] = (pack[12] & !0x70) | 0x50,
            "truncated_header" => pack.truncate(8),
            "truncated_zlib" => {
                pack.remove(pack.len() - 21);
            }
            _ => unreachable!(),
        }
        if invalid != "truncated_header" {
            replace_checksum(&mut pack);
        }
        let observer = Arc::new(RecordingObserver::default());
        assert!(
            parse_pack(
                &pack,
                &ctx,
                repo.repo_blobstore_arc(),
                2,
                Some(observer.clone())
            )
            .await
            .is_err(),
            "accepted {invalid}"
        );
        assert_eq!(observer.counts(), (0, 0), "charged {invalid}");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn mixed_full_ref_chain_and_offset_deltas_decode_exactly(fb: FacebookInit) -> Result<()> {
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);
    let middle = b"middle blob";
    let tip = b"chain tip";
    let sibling = b"ref sibling";
    let offset_base = b"offset base";
    let offset_target = b"offset target";
    let mut pack = make_pack(vec![
        ref_delta(middle, tip)?,
        ref_delta(PAYLOAD, middle)?,
        git_types::PackfileItem::new_base(loose_blob(PAYLOAD))?,
        ref_delta(PAYLOAD, sibling)?,
    ])
    .await?;
    let mut writer = packfile::pack::PackfileWriter::new(
        Vec::new(),
        2,
        1,
        packfile::pack::DeltaForm::OnlyOffset,
        None,
    );
    writer
        .write_unweighted(stream::iter(vec![
            git_types::PackfileItem::new_base(loose_blob(offset_base)),
            ref_delta(offset_base, offset_target),
        ]))
        .await?;
    writer.finish().await?;
    let offsets = writer.into_write();
    // Moving both OFS entries together preserves their relative base distance.
    pack.truncate(pack.len() - 20);
    pack[8..12].copy_from_slice(&6u32.to_be_bytes());
    pack.extend_from_slice(&offsets[12..offsets.len() - 20]);
    let checksum = Sha1::digest(&pack);
    pack.extend_from_slice(checksum.as_slice());
    let stored = tempfile::NamedTempFile::new()?;
    tokio::fs::write(stored.path(), &pack).await?;
    let file = gix_pack::data::File::at(stored.path(), gix_hash::Kind::Sha1)?;
    let mut counts = [0usize; 3];
    for entry in file.streaming_iter()? {
        match entry?.header {
            gix_pack::data::entry::Header::RefDelta { .. } => counts[1] += 1,
            gix_pack::data::entry::Header::OfsDelta { .. } => counts[2] += 1,
            _ => counts[0] += 1,
        }
    }
    assert_eq!(counts, [2, 3, 1]);
    let observer = Arc::new(RecordingObserver::default());
    let (objects, guard) = parse_pack(
        &pack,
        &ctx,
        repo.repo_blobstore_arc(),
        2,
        Some(observer.clone()),
    )
    .await?;
    let expected = [
        PAYLOAD,
        middle.as_slice(),
        tip.as_slice(),
        sibling.as_slice(),
        offset_base.as_slice(),
        offset_target.as_slice(),
    ];
    assert_eq!(objects.len(), expected.len());
    for payload in expected {
        assert_eq!(objects[&blob_id(payload)].raw(), &loose_blob(payload));
    }
    let weight = expected.iter().map(|payload| payload.len()).sum();
    assert_eq!(guard.weight, weight);
    assert_eq!(observer.counts(), (weight, 0));
    drop((objects, guard));
    assert_eq!(observer.counts(), (weight, weight));
    Ok(())
}

#[mononoke::fbinit_test]
async fn many_entries_with_late_external_delta_preserve_all_objects(
    fb: FacebookInit,
) -> Result<()> {
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);
    let base_id = blob_id(PAYLOAD);
    let request = StoreRequest::with_git_sha1(
        PAYLOAD.len() as u64,
        RichGitSha1::from_byte_array(base_id.as_bytes().try_into()?, "blob", PAYLOAD.len() as u64),
    );
    filestore::store(
        &repo.repo_blobstore_arc(),
        FilestoreConfig {
            chunk_size: None,
            concurrency: 1,
        },
        &ctx,
        &request,
        stream::once(async { Ok(Bytes::from_static(PAYLOAD)) }),
    )
    .await?;
    let count = 70_000u64;
    let target = b"late cap target";
    let mut items = (0..count)
        .map(|i| git_types::PackfileItem::new_base(loose_blob(&i.to_le_bytes())))
        .collect::<Result<Vec<_>>>()?;
    items.push(ref_delta(PAYLOAD, target)?);
    let pack = make_pack(items).await?;
    let observer = Arc::new(RecordingObserver::default());
    let (objects, guard) = parse_pack(
        &pack,
        &ctx,
        repo.repo_blobstore_arc(),
        2,
        Some(observer.clone()),
    )
    .await?;
    assert_eq!(objects.len(), count as usize + 2);
    for i in 0..count {
        let payload = i.to_le_bytes();
        assert_eq!(objects[&blob_id(&payload)].raw(), &loose_blob(&payload));
    }
    assert_eq!(objects[&base_id].raw(), &loose_blob(PAYLOAD));
    assert_eq!(objects[&blob_id(target)].raw(), &loose_blob(target));
    let weight = count as usize * 8 + target.len();
    assert_eq!(guard.weight, weight);
    assert_eq!(observer.counts(), (weight, 0));
    drop((objects, guard));
    assert_eq!(observer.counts(), (weight, weight));
    Ok(())
}

#[mononoke::fbinit_test]
async fn encoded_empty_pack_has_no_objects_or_weight(fb: FacebookInit) -> Result<()> {
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ctx = CoreContext::test_mock(fb);
    let mut pack = b"PACK\x00\x00\x00\x02\x00\x00\x00\x00".to_vec();
    let checksum = Sha1::digest(&pack);
    pack.extend_from_slice(checksum.as_slice());
    let observer = Arc::new(RecordingObserver::default());
    let (objects, guard) = parse_pack(
        &pack,
        &ctx,
        repo.repo_blobstore_arc(),
        2,
        Some(observer.clone()),
    )
    .await?;
    assert!(objects.is_empty());
    assert_eq!(guard.weight, 0);
    drop((objects, guard));
    assert_eq!(observer.counts(), (0, 0));
    Ok(())
}
