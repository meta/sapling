/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;

use bytes::Bytes;
use mononoke_macros::mononoke;
use mononoke_types::DateTime;
use quickcheck::QuickCheck;
use quickcheck::TestResult;
use quickcheck::quickcheck;

use super::revlog::Extra;
use super::revlog::RevlogChangeset;
use super::revlog::escape;
use super::revlog::serialize_extras;
use super::revlog::unescape;
use crate::HgBlob;
use crate::HgBlobNode;
use crate::HgManifestId;
use crate::HgNodeHash;
use crate::HgParents;
use crate::NULL_HASH;
use crate::NonRootMPath;
use crate::blobs::ChangesetMetadata;
use crate::blobs::HgBlobChangeset;
use crate::blobs::HgChangesetContent;
use crate::blobs::SNAPSHOT_METADATA_KEY;
use crate::blobs::snapshot_state;
use crate::snapshot::SnapshotHgState;

const CHANGESET: &[u8] = include_bytes!("cset.bin");
const CHANGESET_NOEXTRA: &[u8] = include_bytes!("cset_noextra.bin");

#[mononoke::test]
fn test_parse() {
    let csid: HgNodeHash = "0849d280663e46b3e247857f4a68fabd2ba503c3".parse().unwrap();
    let p1: HgNodeHash = "169cb9e47f8e86079ee9fd79972092f78fbf68b1".parse().unwrap();
    let cset = RevlogChangeset::parse(HgBlob::new(Bytes::from(CHANGESET)), Some(p1), None)
        .expect("parsed");

    let node = HgBlobNode::new(HgBlob::new(Bytes::from(CHANGESET)), Some(p1), None);
    assert_eq!(node.nodeid(), csid);

    assert_eq!(
        cset,
        RevlogChangeset {
            p1: Some(p1),
            p2: None,
            manifestid: HgManifestId::new(
                "497522ef3706a1665bf4140497c65b467454e962".parse().unwrap()
            ),
            user: "Mads Kiilerich <madski@unity3d.com>".into(),
            time: DateTime::from_timestamp(1383910550, -3600).expect("valid timestamp"),
            extra: Extra(
                vec![("branch".into(), "stable".into())]
                    .into_iter()
                    .collect()
            ),
            files: vec![NonRootMPath::new(b"mercurial/util.py").unwrap()],
            message: r"util: warn when adding paths ending with \

Paths ending with \ will fail the verification introduced in 684a977c2ae0 when
checking out on Windows ... and if it didn't fail it would probably not do what
the user expected."
                .into(),
        }
    );

    let csid: HgNodeHash = "526722d24ee5b3b860d4060e008219e083488356".parse().unwrap();
    let p1: HgNodeHash = "db5eb6a86179ce819db03da9ef2090b32f8e3fc4".parse().unwrap();
    let cset = RevlogChangeset::parse(HgBlob::new(Bytes::from(CHANGESET_NOEXTRA)), Some(p1), None)
        .expect("parsed");

    let node = HgBlobNode::new(Bytes::from(CHANGESET_NOEXTRA), Some(p1), None);
    assert_eq!(node.nodeid(), csid);

    assert_eq!(
        cset,
        RevlogChangeset {
            p1: Some(p1),
            p2: None,
            manifestid: HgManifestId::new(
                "6c0d10b92d045127f9a3846b59480451fe3bbac9".parse().unwrap()
            ),
            user: "jake@edge2.net".into(),
            time: DateTime::from_timestamp(1116031690, 25200).expect("valid timestamp"),
            extra: Extra(vec![].into_iter().collect()),
            files: vec![NonRootMPath::new(b"hgweb.py").unwrap()],
            message: r#"reorganize code into classes
clean up html code for w3c validation
"#
            .into(),
        }
    );
}

#[mononoke::test]
fn test_generate() {
    fn test(csid: HgNodeHash, p1: Option<HgNodeHash>, blob: HgBlob, cs: &[u8]) {
        let cset = RevlogChangeset::parse(blob.clone(), p1, None).expect("parsed");

        let node = HgBlobNode::new(blob, p1, None);
        assert_eq!(node.nodeid(), csid);

        let mut new = Vec::new();

        cset.generate(&mut new).expect("generate failed");

        assert_eq!(new, cs);
    }

    let csid: HgNodeHash = "0849d280663e46b3e247857f4a68fabd2ba503c3".parse().unwrap();
    let p1: HgNodeHash = "169cb9e47f8e86079ee9fd79972092f78fbf68b1".parse().unwrap();
    test(
        csid,
        Some(p1),
        HgBlob::new(Bytes::from(CHANGESET)),
        CHANGESET,
    );

    let csid: HgNodeHash = "526722d24ee5b3b860d4060e008219e083488356".parse().unwrap();
    let p1: HgNodeHash = "db5eb6a86179ce819db03da9ef2090b32f8e3fc4".parse().unwrap();
    test(
        csid,
        Some(p1),
        HgBlob::new(Bytes::from(CHANGESET_NOEXTRA)),
        CHANGESET_NOEXTRA,
    );
}

quickcheck! {
    fn escape_roundtrip(s: Vec<u8>) -> bool {
        let esc = escape(&s);
        let unesc = unescape(&esc);
        if s != unesc {
            println!("s: {s:?}, esc: {esc:?}, unesc: {unesc:?}")
        }
        s == unesc
    }
}

fn extras_roundtrip_prop(kv: BTreeMap<Vec<u8>, Vec<u8>>) -> TestResult {
    if kv.keys().any(|k| k.contains(&b':')) {
        return TestResult::discard();
    }

    let extra = Extra(
        kv.into_iter()
            .map(|(k, v)| (Bytes::from(k), Bytes::from(v)))
            .collect(),
    );
    let mut enc = Vec::new();
    let () = serialize_extras(&extra, &mut enc).expect("enc failed");
    let new = Extra::from_slice(Some(&enc)).expect("parse failed");

    TestResult::from_bool(new == extra)
}

#[mononoke::test]
fn extras_roundtrip() {
    QuickCheck::new()
        .tests(50) // more takes too much time
        .quickcheck(extras_roundtrip_prop as fn(BTreeMap<Vec<u8>, Vec<u8>>) -> TestResult);
}

fn changeset_with_files(files: Vec<NonRootMPath>) -> RevlogChangeset {
    RevlogChangeset::new_from_parts(
        HgParents::None,
        HgManifestId::new(NULL_HASH),
        Bytes::from_static(b"user"),
        DateTime::from_timestamp(0, 0).unwrap(),
        BTreeMap::new(),
        files,
        Bytes::from_static(b"msg"),
    )
}

fn changeset_content_with_files(files: Vec<NonRootMPath>) -> HgChangesetContent {
    HgChangesetContent::new_from_parts(
        HgParents::None,
        HgManifestId::new(NULL_HASH),
        ChangesetMetadata {
            user: "user".to_string(),
            time: DateTime::from_timestamp(0, 0).unwrap(),
            extra: BTreeMap::new(),
            message: "msg".to_string(),
        },
        files,
    )
}

#[mononoke::test]
fn generate_writes_non_utf8_path_bytes_verbatim() {
    let path = NonRootMPath::new(b"caf\xe9").unwrap();
    let cs = changeset_with_files(vec![path.clone()]);

    let mut text = Vec::new();
    cs.generate(&mut text).expect("generate");

    // Mercurial's changelog text: "\n".join([hex(manifest), user, date] + files + ["", desc]),
    // with file names written as raw bytes.
    let hg_text: &[u8] = b"0000000000000000000000000000000000000000\nuser\n0 0\ncaf\xe9\n\nmsg";
    assert_eq!(text, hg_text);

    let hg_id = HgBlobNode::new(Bytes::from_static(hg_text), None, None).nodeid();
    assert_eq!(cs.get_node().unwrap().nodeid(), hg_id);

    let parsed = RevlogChangeset::parse(HgBlob::new(Bytes::from(text)), None, None).expect("parse");
    assert_eq!(parsed.files(), &[path.clone()][..]);

    let content = changeset_content_with_files(vec![path]);
    assert_eq!(content.compute_hash().unwrap().into_nodehash(), hg_id);
}

fn empty_metadata() -> ChangesetMetadata {
    ChangesetMetadata {
        user: "user".to_string(),
        time: DateTime::from_timestamp(0, 0).unwrap(),
        extra: BTreeMap::new(),
        message: "msg".to_string(),
    }
}

fn paths(paths: &[&str]) -> Vec<NonRootMPath> {
    paths
        .iter()
        .map(|path| NonRootMPath::new(path).unwrap())
        .collect()
}

#[mononoke::test]
fn record_snapshot_state_roundtrip() {
    let mut metadata = empty_metadata();
    metadata
        .record_snapshot_state(paths(&["dir/untracked.txt"]), paths(&["missing.txt"]))
        .unwrap();

    let state = snapshot_state(&metadata.extra).unwrap();
    assert_eq!(
        state,
        Some(SnapshotHgState {
            untracked: paths(&["dir/untracked.txt"]),
            missing: paths(&["missing.txt"]),
        })
    );
}

#[mononoke::test]
fn record_snapshot_state_sorted() {
    let mut metadata = empty_metadata();
    metadata
        .record_snapshot_state(
            paths(&["b.txt", "a/z.txt", "a.b", "b.txt", "a/b.txt"]),
            paths(&["z", "y", "z"]),
        )
        .unwrap();

    assert_eq!(
        metadata.extra.get(SNAPSHOT_METADATA_KEY).unwrap().as_ref(),
        br#"{"v":1,"untracked":["a.b","a/b.txt","a/z.txt","b.txt"],"missing":["y","z"]}"#,
        "paths must be sorted by their bytes and deduplicated"
    );
}

#[mononoke::test]
fn record_snapshot_state_empty_marks_snapshot() {
    let mut metadata = empty_metadata();
    metadata
        .record_snapshot_state(Vec::new(), Vec::new())
        .unwrap();

    assert_eq!(
        metadata.extra.get(SNAPSHOT_METADATA_KEY).unwrap().as_ref(),
        br#"{"v":1,"untracked":[],"missing":[]}"#,
    );
    assert_eq!(
        snapshot_state(&metadata.extra).unwrap(),
        Some(SnapshotHgState::default())
    );
    assert_eq!(
        snapshot_state(&empty_metadata().extra).unwrap(),
        None,
        "changesets without the extra are not snapshots"
    );
}

#[mononoke::test]
fn record_snapshot_state_twice_bails() {
    let mut metadata = empty_metadata();
    metadata
        .record_snapshot_state(paths(&["a"]), Vec::new())
        .unwrap();

    assert!(
        metadata
            .record_snapshot_state(paths(&["b"]), Vec::new())
            .is_err()
    );
    assert_eq!(
        snapshot_state(&metadata.extra).unwrap().unwrap().untracked,
        paths(&["a"]),
        "the first state must be kept"
    );
}

#[mononoke::test]
fn snapshot_extra_survives_changeset_text() {
    let untracked = paths(&["dir/back\\slash", "quote\"d", "caf\u{e9}"]);
    let missing = paths(&["gone/file.txt"]);
    let mut metadata = empty_metadata();
    metadata
        .record_snapshot_state(untracked.clone(), missing.clone())
        .unwrap();
    let extra = metadata.extra.clone();
    let files = paths(&["dir/back\\slash", "gone/file.txt"]);

    let blob_cs = HgBlobChangeset::new(HgChangesetContent::new_from_parts(
        HgParents::None,
        HgManifestId::new(NULL_HASH),
        metadata,
        files.clone(),
    ))
    .unwrap();

    let revlog_cs = RevlogChangeset::new_from_parts(
        HgParents::None,
        HgManifestId::new(NULL_HASH),
        Bytes::from_static(b"user"),
        DateTime::from_timestamp(0, 0).unwrap(),
        extra,
        files,
        Bytes::from_static(b"msg"),
    );
    let mut text = Vec::new();
    revlog_cs.generate(&mut text).unwrap();
    assert_eq!(
        HgBlobNode::new(Bytes::from(text.clone()), None, None).nodeid(),
        blob_cs.get_changeset_id().into_nodehash(),
        "the text must be the one the changeset hash covers"
    );

    let parsed = RevlogChangeset::parse(HgBlob::new(Bytes::from(text)), None, None).unwrap();
    assert_eq!(parsed.extra(), blob_cs.extra());
    let mut expected_untracked = untracked;
    expected_untracked.sort_by(NonRootMPath::compare_bytes);
    assert_eq!(
        snapshot_state(parsed.extra()).unwrap(),
        Some(SnapshotHgState {
            untracked: expected_untracked,
            missing,
        })
    );
}
