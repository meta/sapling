/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt;

use blobstore::Blobstore;
use blobstore::BlobstoreBytes;
use blobstore::BlobstoreGetData;
use blobstore::KeyedBlobstore;
use blobstore::OverwriteStatus;
use blobstore::PutBehaviour;
use md5::Digest;
use md5::Md5;
use memblob::Memblob;
use p4_types::Md5Digest;

use super::*;

/// A revision: depot path, revision, action, Perforce type and content.
pub(super) type Spec = (&'static str, u32, Action, &'static str, &'static [u8]);

fn digest(content: &[u8]) -> Md5Digest {
    Md5Digest::new(Md5::digest(content).into())
}

/// `source` with changelist `number` of `files`, each with its content's real MD5 and size.
pub(super) fn with_revisions(source: FakeSource, number: u64, files: &[Spec]) -> FakeSource {
    let mut source = source.with_changelist(number, &[]);
    for &(depot_path, rev, action, file_type, content) in files {
        let file = FileRevision {
            revision: rev,
            size: Some(content.len() as u64),
            digest: Some(digest(content)),
            ..revision(depot_path, action, file_type, None)
        };
        source.contents.insert(key(&file), content);
        source
            .changelists
            .get_mut(&number)
            .unwrap()
            .files
            .push(file);
    }
    source
}

/// One changelist and one file at a time, so a record one revision writes is there for the
/// next.
fn options(content_reuse: ContentReuse) -> ImportOptions {
    ImportOptions {
        content_reuse,
        changelist_lookahead: 1,
        file_concurrency: 1,
        ..Default::default()
    }
}

fn prints(source: &FakeSource) -> usize {
    let calls = source.calls.lock().unwrap();
    calls.iter().filter(|call| **call == "print").count()
}

fn record_key(content: &[u8]) -> String {
    format!("p4.content_by_md5.v1.{}.{}", digest(content), content.len())
}

fn marker_key(content: &[u8]) -> String {
    format!(
        "p4.content_by_md5.v1.collision.{}.{}",
        digest(content),
        content.len()
    )
}

async fn read(ctx: &CoreContext, repo: &TestRepo, key: &str) -> Result<Option<String>> {
    let data = repo.repo_blobstore().get(ctx, key).await?;
    Ok(data.map(|data| String::from_utf8_lossy(data.as_raw_bytes()).into_owned()))
}

async fn write(ctx: &CoreContext, repo: &TestRepo, key: String, value: String) -> Result<()> {
    repo.repo_blobstore()
        .put(ctx, key, BlobstoreBytes::from_bytes(value))
        .await
}

/// Stores `content` as an earlier import would have.
async fn store(ctx: &CoreContext, repo: &TestRepo, content: &'static [u8]) -> Result<ContentId> {
    let size = content.len() as u64;
    Ok(upload_content(ctx, repo, size, chunks(&[content]))
        .await?
        .content_id)
}

/// The one file `changeset` changes, with its content.
async fn only_file(
    ctx: &CoreContext,
    repo: &TestRepo,
    changeset: &BonsaiChangeset,
) -> Result<(String, Option<Bytes>)> {
    let files = files(ctx, repo, changeset).await?;
    let [(path, content, _)] = &files[..] else {
        panic!("one file expected: {files:?}");
    };
    Ok((path.clone(), content.clone()))
}

fn one(path: &str) -> (String, Option<Bytes>) {
    (path.to_owned(), Some(Bytes::from_static(b"one\n")))
}

/// 9 branches `b.txt` from a file the depot does not have.
fn branch_of_one() -> FakeSource {
    with_revisions(
        FakeSource::default(),
        9,
        &[("//test1/b.txt", 1, Action::Branch, "text", b"one\n")],
    )
    .with_branch("//test1/b.txt", "//test1/nowhere.txt")
}

#[mononoke::fbinit_test]
async fn test_a_branch_of_stored_content_is_not_printed(fb: FacebookInit) -> Result<()> {
    let (ctx, dir) = scuba_logging_context(fb)?;
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = with_revisions(
        FakeSource::default(),
        7,
        &[("//test1/a.txt", 1, Action::Add, "text", b"one\n")],
    );
    let source = with_revisions(
        source,
        9,
        &[("//test1/b.txt", 1, Action::Branch, "text", b"one\n")],
    )
    .with_branch("//test1/b.txt", "//test1/a.txt");

    let batch = import_changelists(
        &ctx,
        &repo,
        &source,
        DEPOT,
        &master,
        &[7, 9],
        &options(ContentReuse::On),
    )
    .await?;

    assert_eq!(1, prints(&source), "only the add is printed");
    let [add, branch] = &batch.changelists[..] else {
        panic!("two changelists expected: {batch:?}");
    };
    let changeset = branch.cs_id.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        vec![(
            "b.txt".to_owned(),
            Some(Bytes::from_static(b"one\n")),
            Some((path("a.txt"), add.cs_id))
        )],
        files(&ctx, &repo, &changeset).await?
    );
    let reuse = &branch.timings.content_reuse;
    assert_eq!(
        (0, 1, 4, 0),
        (
            branch.timings.uploaded_files,
            reuse.reused_files,
            reuse.reused_bytes,
            reuse.reusable_files
        )
    );
    let rows = scuba_rows(&dir, "P4 changelist imported")?;
    let row = &rows[1];
    assert_eq!(row["p4_change"], 9);
    assert_eq!(row["uploaded_files"], 0);
    assert_eq!(row["reused_files"], 1);
    assert_eq!(row["reused_bytes"], 4);
    assert_eq!(row["reusable_files"], 0);
    assert_eq!(row["reusable_bytes"], 0);
    assert!(row.get("content_reuse_lookup_us").is_some(), "{row:?}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_an_add_edit_or_import_of_stored_content_is_printed(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let source = with_revisions(
        FakeSource::default(),
        7,
        &[("//test1/a.txt", 1, Action::Add, "text", b"one\n")],
    );
    let source = with_revisions(
        source,
        9,
        &[
            ("//test1/a.txt", 2, Action::Edit, "text", b"one\n"),
            ("//test1/c.txt", 1, Action::Add, "text", b"one\n"),
            ("//test1/d.txt", 1, Action::Import, "text", b"one\n"),
        ],
    );

    let built = build_changesets(
        &ctx,
        &repo,
        &source,
        DEPOT,
        None,
        &[7, 9],
        &options(ContentReuse::On),
    )
    .await?;

    assert!(
        read(&ctx, &repo, &record_key(b"one\n")).await?.is_some(),
        "7's add recorded the content"
    );
    assert_eq!(4, prints(&source), "every add, edit and import is printed");
    assert_eq!(3, built[1].timings.uploaded_files);
    assert_eq!(
        ContentReuseReport::default(),
        built[1].timings.content_reuse,
        "nothing reused, nothing looked up"
    );
    Ok(())
}

/// Revisions whose stored bytes need not be the bytes their digest covers.
const NEVER_RECORDED: [(&str, &str, &[u8]); 5] = [
    ("//test1/link", "symlink", b"a.txt\n"),
    ("//test1/k.txt", "text+k", b"$Id$ k\n"),
    ("//test1/ko.txt", "text+ko", b"$Id$ ko\n"),
    ("//test1/old.txt", "ktext", b"$Id$ old\n"),
    ("//test1/oldx", "kxtext", b"$Id$ oldx\n"),
];

#[mononoke::fbinit_test]
async fn test_symlinks_and_keyword_types_are_not_recorded(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let mut files: Vec<Spec> = NEVER_RECORDED
        .iter()
        .map(|&(path, file_type, content)| (path, 1, Action::Add, file_type, content))
        .collect();
    files.push(("//test1/plain.txt", 1, Action::Add, "text", b"plain\n"));
    let source = with_revisions(FakeSource::default(), 7, &files);

    build_changesets(
        &ctx,
        &repo,
        &source,
        DEPOT,
        None,
        &[7],
        &options(ContentReuse::On),
    )
    .await?;

    for (path, _, content) in NEVER_RECORDED {
        assert_eq!(
            None,
            read(&ctx, &repo, &record_key(content)).await?,
            "{path}"
        );
    }
    assert!(
        read(&ctx, &repo, &record_key(b"plain\n")).await?.is_some(),
        "a plain text add is recorded"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_symlinks_and_keyword_types_are_not_reused(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let all = NEVER_RECORDED
        .into_iter()
        .chain([("//test1/plain.txt", "text", &b"plain\n"[..])]);
    let mut branches = Vec::new();
    for (path, file_type, content) in all.clone() {
        // A valid record, as if the content had been stored as a plain file.
        let id = store(&ctx, &repo, content).await?;
        write(&ctx, &repo, record_key(content), id.to_string()).await?;
        branches.push((path, 1, Action::Branch, file_type, content));
    }
    let source = all.fold(
        with_revisions(FakeSource::default(), 9, &branches),
        |source, (path, _, _)| source.with_branch(path, "//test1/nowhere"),
    );

    let built = build_changesets(
        &ctx,
        &repo,
        &source,
        DEPOT,
        None,
        &[9],
        &options(ContentReuse::On),
    )
    .await?;

    assert_eq!(
        5,
        prints(&source),
        "all but the plain text branch are printed"
    );
    assert_eq!(1, built[0].timings.content_reuse.reused_files);
    let link = files(&ctx, &repo, &built[0].changeset)
        .await?
        .into_iter()
        .find(|(path, _, _)| path == "link");
    assert_eq!(
        Some(Bytes::from_static(b"a.txt")),
        link.and_then(|(_, content, _)| content),
        "the symlink is stored as its target, not as the recorded content"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_a_record_of_other_content_is_a_collision(fb: FacebookInit) -> Result<()> {
    for (mode, action) in [
        (ContentReuse::Verify, Action::Add),
        (ContentReuse::Verify, Action::Branch),
        (ContentReuse::On, Action::Add),
    ] {
        let case = format!("{mode:?} {action:?}");
        let (ctx, dir) = scuba_logging_context(fb)?;
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        // Same size as the printed content: only the content ids tell them apart.
        let other = store(&ctx, &repo, b"uno\n").await?;
        write(&ctx, &repo, record_key(b"one\n"), other.to_string()).await?;
        let source = with_revisions(
            FakeSource::default(),
            7,
            &[("//test1/a.txt", 1, action, "text", b"one\n")],
        )
        .with_branch("//test1/a.txt", "//test1/nowhere.txt");

        let built =
            build_changesets(&ctx, &repo, &source, DEPOT, None, &[7], &options(mode)).await?;

        assert_eq!(1, prints(&source), "{case}");
        let changeset = &built[0].changeset;
        assert_eq!(
            one("a.txt"),
            only_file(&ctx, &repo, changeset).await?,
            "{case}"
        );
        assert_eq!(
            ContentReuseReport {
                lookup: built[0].timings.content_reuse.lookup,
                ..Default::default()
            },
            built[0].timings.content_reuse,
            "{case}: nothing reused or reusable"
        );
        let printed = changeset
            .file_changes()
            .find_map(|(_, change)| change.content_id())
            .unwrap();
        assert_eq!(
            Some(printed.to_string()),
            read(&ctx, &repo, &marker_key(b"one\n")).await?,
            "{case}"
        );
        assert_eq!(
            Some(other.to_string()),
            read(&ctx, &repo, &record_key(b"one\n")).await?,
            "{case}: the record is never rewritten"
        );
        let rows = scuba_rows(&dir, "P4 content digest collision")?;
        assert_eq!(1, rows.len(), "{case}: {rows:?}");
        let row = &rows[0];
        assert_eq!(row["depot_path"], "//test1/a.txt", "{case}");
        assert_eq!(row["revision"], 1, "{case}");
        assert_eq!(row["digest"], digest(b"one\n").to_string(), "{case}");
        assert_eq!(row["size"], 4, "{case}");
        assert_eq!(row["recorded_content_id"], other.to_string(), "{case}");
        assert_eq!(row["printed_content_id"], printed.to_string(), "{case}");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_a_branch_prints_once_its_digest_has_collided(fb: FacebookInit) -> Result<()> {
    for (marked, expected_prints) in [(false, 0), (true, 1)] {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let id = store(&ctx, &repo, b"one\n").await?;
        write(&ctx, &repo, record_key(b"one\n"), id.to_string()).await?;
        if marked {
            write(&ctx, &repo, marker_key(b"one\n"), id.to_string()).await?;
        }
        let source = branch_of_one();

        let built = build_changesets(
            &ctx,
            &repo,
            &source,
            DEPOT,
            None,
            &[9],
            &options(ContentReuse::On),
        )
        .await?;

        assert_eq!(expected_prints, prints(&source), "marked: {marked}");
        assert_eq!(
            one("b.txt"),
            only_file(&ctx, &repo, &built[0].changeset).await?,
            "marked: {marked}"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_a_record_of_missing_content_or_another_size_is_a_miss(
    fb: FacebookInit,
) -> Result<()> {
    for case in ["not stored", "another size", "unparsable"] {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let record = match case {
            "not stored" => ONES_CTID.to_string(),
            "another size" => store(&ctx, &repo, b"one more\n").await?.to_string(),
            _ => "not a content id".to_owned(),
        };
        write(&ctx, &repo, record_key(b"one\n"), record).await?;
        let source = branch_of_one();

        let built = build_changesets(
            &ctx,
            &repo,
            &source,
            DEPOT,
            None,
            &[9],
            &options(ContentReuse::On),
        )
        .await?;

        assert_eq!(1, prints(&source), "{case}");
        assert_eq!(0, built[0].timings.content_reuse.reused_files, "{case}");
        assert_eq!(
            one("b.txt"),
            only_file(&ctx, &repo, &built[0].changeset).await?,
            "{case}"
        );
    }
    Ok(())
}

/// 7 adds `a.txt`; 9 copies its content in each way that can be reused, adds it again,
/// and branches new content.
fn copies_of_one() -> FakeSource {
    let source = with_revisions(
        FakeSource::default(),
        7,
        &[("//test1/a.txt", 1, Action::Add, "text", b"one\n")],
    );
    with_revisions(
        source,
        9,
        &[
            ("//test1/b.txt", 1, Action::Branch, "text", b"one\n"),
            ("//test1/c.txt", 1, Action::Add, "text", b"one\n"),
            ("//test1/d.txt", 1, Action::Branch, "text", b"new\n"),
            ("//test1/i.txt", 1, Action::Integrate, "text", b"one\n"),
            ("//test1/m.txt", 1, Action::MoveAdd, "text", b"one\n"),
        ],
    )
    .with_branch("//test1/b.txt", "//test1/a.txt")
    .with_branch("//test1/d.txt", "//test1/a.txt")
    .with_branch("//test1/m.txt", "//test1/a.txt")
}

#[mononoke::fbinit_test]
async fn test_verify_prints_everything_and_counts_what_on_reuses(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mut reports = Vec::new();
    for mode in [ContentReuse::Verify, ContentReuse::On] {
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let source = copies_of_one();
        let built =
            build_changesets(&ctx, &repo, &source, DEPOT, None, &[7, 9], &options(mode)).await?;
        let timings = &built[1].timings;
        let reuse = &timings.content_reuse;
        reports.push((
            prints(&source),
            timings.uploaded_files,
            reuse.reused_files,
            reuse.reused_bytes,
            reuse.reusable_files,
            reuse.reusable_bytes,
        ));
    }

    assert_eq!(
        vec![
            // Verify: everything printed; the branch, integrate and move/add were reusable.
            (6, 5, 0, 0, 3, 12),
            // On: those three are not printed.
            (3, 2, 3, 12, 0, 0),
        ],
        reports
    );
    Ok(())
}

/// Adds, edits, copies and symlinks over three changelists.
fn history_with_copies() -> FakeSource {
    let source = with_revisions(
        FakeSource::default(),
        7,
        &[
            ("//test1/a.txt", 1, Action::Add, "text", b"one\n"),
            ("//test1/b.txt", 1, Action::Add, "text", b"bye\n"),
            ("//test1/link", 1, Action::Add, "symlink", b"a.txt\n"),
        ],
    );
    let source = with_revisions(
        source,
        9,
        &[
            ("//test1/a.txt", 2, Action::Edit, "text", b"two\n"),
            ("//test1/c.txt", 1, Action::Branch, "text", b"one\n"),
        ],
    );
    with_revisions(
        source,
        11,
        &[
            ("//test1/d.txt", 1, Action::MoveAdd, "text", b"one\n"),
            ("//test1/e.txt", 1, Action::Integrate, "text", b"two\n"),
            ("//test1/link2", 1, Action::Branch, "symlink", b"a.txt\n"),
        ],
    )
    .with_branch("//test1/c.txt", "//test1/a.txt")
    .with_branch("//test1/d.txt", "//test1/c.txt")
    .with_branch("//test1/link2", "//test1/link")
}

#[mononoke::fbinit_test]
async fn test_the_changesets_are_the_same_in_every_mode(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let mut results = Vec::new();
    for mode in [ContentReuse::Off, ContentReuse::Verify, ContentReuse::On] {
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let source = history_with_copies();
        let built = build_changesets(
            &ctx,
            &repo,
            &source,
            DEPOT,
            None,
            &[7, 9, 11],
            &options(mode),
        )
        .await?;
        let ids: Vec<_> = built
            .iter()
            .map(|built| built.changeset.get_changeset_id())
            .collect();
        results.push((prints(&source), ids));
    }

    let [(off_prints, off), (verify_prints, verify), (on_prints, on)] = &results[..] else {
        panic!("three results expected: {results:?}");
    };
    assert_eq!(3, off.len());
    assert_eq!(off, verify);
    assert_eq!(off, on);
    // On reuses c.txt, d.txt and e.txt; the symlink branch is printed.
    assert_eq!((8, 8, 5), (*off_prints, *verify_prints, *on_prints));
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_content_stored_by_a_failed_batch_is_reused(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = branch_of_one();
    let options = options(ContentReuse::On);

    // The fake has no changelist 10: its describe fails the batch after 9 is stored.
    let error = import_changelists(&ctx, &repo, &source, DEPOT, &master, &[9, 10], &options)
        .await
        .unwrap_err();
    assert_eq!(Some(Stage::Build), failed_stage(&error), "{error:?}");
    assert_eq!(1, prints(&source));

    let batch = import_changelists(&ctx, &repo, &source, DEPOT, &master, &[9], &options).await?;

    assert_eq!(
        1,
        prints(&source),
        "the retry does not print the branch again"
    );
    let imported = &batch.changelists[0];
    assert_eq!(1, imported.timings.content_reuse.reused_files);
    let changeset = imported.cs_id.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(one("b.txt"), only_file(&ctx, &repo, &changeset).await?);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_a_revision_without_its_own_digest_is_not_recorded(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let mut source = with_revisions(
        FakeSource::default(),
        7,
        &[
            ("//test1/a.txt", 1, Action::Add, "text", b"one\n"),
            ("//test1/b.txt", 1, Action::Add, "text", b"two\n"),
            ("//test1/c.txt", 1, Action::Add, "text", b"three\n"),
        ],
    );
    let files = &mut source.changelists.get_mut(&7).unwrap().files;
    // The fake source does not check digests, as a real source would.
    files[0].digest = Some(digest(b"uno\n"));
    files[1].digest = None;

    build_changesets(
        &ctx,
        &repo,
        &source,
        DEPOT,
        None,
        &[7],
        &options(ContentReuse::Verify),
    )
    .await?;

    for content in [&b"one\n"[..], b"uno\n", b"two\n"] {
        assert_eq!(None, read(&ctx, &repo, &record_key(content)).await?);
    }
    assert!(
        read(&ctx, &repo, &record_key(b"three\n")).await?.is_some(),
        "a revision with its own digest is recorded"
    );
    Ok(())
}

/// Two contents with one MD5 and size: the first published MD5 collision (Wang and Yu,
/// 2005).
const COLLIDING_A: [u8; 128] = [
    0xd1, 0x31, 0xdd, 0x02, 0xc5, 0xe6, 0xee, 0xc4, 0x69, 0x3d, 0x9a, 0x06, 0x98, 0xaf, 0xf9, 0x5c,
    0x2f, 0xca, 0xb5, 0x87, 0x12, 0x46, 0x7e, 0xab, 0x40, 0x04, 0x58, 0x3e, 0xb8, 0xfb, 0x7f, 0x89,
    0x55, 0xad, 0x34, 0x06, 0x09, 0xf4, 0xb3, 0x02, 0x83, 0xe4, 0x88, 0x83, 0x25, 0x71, 0x41, 0x5a,
    0x08, 0x51, 0x25, 0xe8, 0xf7, 0xcd, 0xc9, 0x9f, 0xd9, 0x1d, 0xbd, 0xf2, 0x80, 0x37, 0x3c, 0x5b,
    0xd8, 0x82, 0x3e, 0x31, 0x56, 0x34, 0x8f, 0x5b, 0xae, 0x6d, 0xac, 0xd4, 0x36, 0xc9, 0x19, 0xc6,
    0xdd, 0x53, 0xe2, 0xb4, 0x87, 0xda, 0x03, 0xfd, 0x02, 0x39, 0x63, 0x06, 0xd2, 0x48, 0xcd, 0xa0,
    0xe9, 0x9f, 0x33, 0x42, 0x0f, 0x57, 0x7e, 0xe8, 0xce, 0x54, 0xb6, 0x70, 0x80, 0xa8, 0x0d, 0x1e,
    0xc6, 0x98, 0x21, 0xbc, 0xb6, 0xa8, 0x83, 0x93, 0x96, 0xf9, 0x65, 0x2b, 0x6f, 0xf7, 0x2a, 0x70,
];
const COLLIDING_B: [u8; 128] = [
    0xd1, 0x31, 0xdd, 0x02, 0xc5, 0xe6, 0xee, 0xc4, 0x69, 0x3d, 0x9a, 0x06, 0x98, 0xaf, 0xf9, 0x5c,
    0x2f, 0xca, 0xb5, 0x07, 0x12, 0x46, 0x7e, 0xab, 0x40, 0x04, 0x58, 0x3e, 0xb8, 0xfb, 0x7f, 0x89,
    0x55, 0xad, 0x34, 0x06, 0x09, 0xf4, 0xb3, 0x02, 0x83, 0xe4, 0x88, 0x83, 0x25, 0xf1, 0x41, 0x5a,
    0x08, 0x51, 0x25, 0xe8, 0xf7, 0xcd, 0xc9, 0x9f, 0xd9, 0x1d, 0xbd, 0x72, 0x80, 0x37, 0x3c, 0x5b,
    0xd8, 0x82, 0x3e, 0x31, 0x56, 0x34, 0x8f, 0x5b, 0xae, 0x6d, 0xac, 0xd4, 0x36, 0xc9, 0x19, 0xc6,
    0xdd, 0x53, 0xe2, 0x34, 0x87, 0xda, 0x03, 0xfd, 0x02, 0x39, 0x63, 0x06, 0xd2, 0x48, 0xcd, 0xa0,
    0xe9, 0x9f, 0x33, 0x42, 0x0f, 0x57, 0x7e, 0xe8, 0xce, 0x54, 0xb6, 0x70, 0x80, 0x28, 0x0d, 0x1e,
    0xc6, 0x98, 0x21, 0xbc, 0xb6, 0xa8, 0x83, 0x93, 0x96, 0xf9, 0x65, 0xab, 0x6f, 0xf7, 0x2a, 0x70,
];

/// A blobstore whose reads of records return a moment after they are served, so that two
/// record checks running at once would both read before either writes.
#[derive(Debug)]
struct SlowRecordReads(Memblob);

impl fmt::Display for SlowRecordReads {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SlowRecordReads({})", self.0)
    }
}

#[async_trait]
impl Blobstore for SlowRecordReads {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        let data = self.0.get(ctx, key).await;
        if key.contains("p4.content_by_md5") {
            tokio::task::yield_now().await;
        }
        data
    }

    async fn put_explicit<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
        put_behaviour: PutBehaviour,
    ) -> Result<OverwriteStatus> {
        self.0.put_explicit(ctx, key, value, put_behaviour).await
    }

    async fn put_with_status<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<OverwriteStatus> {
        self.0.put_with_status(ctx, key, value).await
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        Blobstore::unlink(&self.0, ctx, key).await
    }
}

#[mononoke::fbinit_test]
async fn test_two_contents_with_one_md5_printed_at_once_are_a_collision(
    fb: FacebookInit,
) -> Result<()> {
    assert_eq!(digest(&COLLIDING_A), digest(&COLLIDING_B));
    let cases = [
        (ContentReuse::Verify, PutBehaviour::IfAbsent),
        (ContentReuse::On, PutBehaviour::IfAbsent),
        (ContentReuse::On, PutBehaviour::Overwrite),
    ];
    for (mode, put_behaviour) in cases {
        let (ctx, dir) = scuba_logging_context(fb)?;
        let blobstore = SlowRecordReads(Memblob::new(put_behaviour));
        let repo: TestRepo = TestRepoFactory::new(fb)?
            .with_blobstore(Arc::new(blobstore))
            .build()
            .await?;
        let source = with_revisions(
            FakeSource::default(),
            7,
            &[
                ("//test1/a.bin", 1, Action::Add, "binary", &COLLIDING_A),
                ("//test1/b.bin", 1, Action::Add, "binary", &COLLIDING_B),
            ],
        );
        let source = with_revisions(
            source,
            9,
            &[("//test1/c.bin", 1, Action::Branch, "binary", &COLLIDING_A)],
        )
        .with_branch("//test1/c.bin", "//test1/a.bin");
        let options = ImportOptions {
            file_concurrency: 2,
            ..options(mode)
        };

        let built = build_changesets(&ctx, &repo, &source, DEPOT, None, &[7, 9], &options).await?;

        assert_eq!(3, prints(&source), "{mode:?}: the branch is printed too");
        let contents = |files: Vec<(String, Option<Bytes>, _)>| -> Vec<(String, Option<Bytes>)> {
            files
                .into_iter()
                .map(|(path, content, _)| (path, content))
                .collect()
        };
        assert_eq!(
            vec![
                ("a.bin".to_owned(), Some(Bytes::from_static(&COLLIDING_A))),
                ("b.bin".to_owned(), Some(Bytes::from_static(&COLLIDING_B))),
            ],
            contents(files(&ctx, &repo, &built[0].changeset).await?),
            "{mode:?}"
        );
        assert_eq!(
            vec![("c.bin".to_owned(), Some(Bytes::from_static(&COLLIDING_A)))],
            contents(files(&ctx, &repo, &built[1].changeset).await?),
            "{mode:?}: the branch has its own content, not the other one"
        );
        assert!(
            read(&ctx, &repo, &marker_key(&COLLIDING_A))
                .await?
                .is_some(),
            "{mode:?}"
        );
        let rows = scuba_rows(&dir, "P4 content digest collision")?;
        assert!(!rows.is_empty(), "{mode:?}");
        assert_eq!(rows[0]["p4_action"], "add", "{mode:?}");
        assert_eq!(
            rows[0]["content_reuse"],
            format!("{mode:?}").to_lowercase(),
            "{mode:?}"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_off_keeps_no_records_and_logs_no_reuse(fb: FacebookInit) -> Result<()> {
    let (ctx, dir) = scuba_logging_context(fb)?;
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = with_revisions(
        FakeSource::default(),
        7,
        &[("//test1/a.txt", 1, Action::Add, "text", b"one\n")],
    );

    import_changelists(
        &ctx,
        &repo,
        &source,
        DEPOT,
        &master,
        &[7],
        &options(ContentReuse::Off),
    )
    .await?;

    assert_eq!(None, read(&ctx, &repo, &record_key(b"one\n")).await?);
    let rows = scuba_rows(&dir, "P4 changelist imported")?;
    assert_eq!(rows[0]["uploaded_files"], 1);
    assert!(rows[0].get("reused_files").is_none(), "{:?}", rows[0]);
    Ok(())
}

/// A blobstore that fails every read of a collision marker.
#[derive(Debug, Default)]
struct MarkersUnreadable(Memblob);

impl fmt::Display for MarkersUnreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MarkersUnreadable({})", self.0)
    }
}

#[async_trait]
impl Blobstore for MarkersUnreadable {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        ensure!(!key.contains(".collision."), "{key} is unreadable");
        self.0.get(ctx, key).await
    }

    async fn put_explicit<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
        put_behaviour: PutBehaviour,
    ) -> Result<OverwriteStatus> {
        self.0.put_explicit(ctx, key, value, put_behaviour).await
    }

    async fn put_with_status<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<OverwriteStatus> {
        self.0.put_with_status(ctx, key, value).await
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        Blobstore::unlink(&self.0, ctx, key).await
    }
}

#[mononoke::fbinit_test]
async fn test_a_record_that_cannot_be_looked_up_prints(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = TestRepoFactory::new(fb)?
        .with_blobstore(Arc::new(MarkersUnreadable::default()))
        .build()
        .await?;
    // A valid record: were its marker readable, the branch would be reused.
    let id = store(&ctx, &repo, b"one\n").await?;
    write(&ctx, &repo, record_key(b"one\n"), id.to_string()).await?;
    let source = branch_of_one();

    let built = build_changesets(
        &ctx,
        &repo,
        &source,
        DEPOT,
        None,
        &[9],
        &options(ContentReuse::On),
    )
    .await?;

    assert_eq!(1, prints(&source), "the branch is printed");
    assert_eq!(0, built[0].timings.content_reuse.reused_files);
    assert_eq!(
        one("b.txt"),
        only_file(&ctx, &repo, &built[0].changeset).await?
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_a_reused_revision_is_printed_if_an_earlier_changelist_collides(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    // One half of the pair is stored and recorded, as an earlier import would have left it.
    let recorded = store(&ctx, &repo, &COLLIDING_A).await?;
    write(&ctx, &repo, record_key(&COLLIDING_A), recorded.to_string()).await?;
    // 7 adds the other half, and 9 branches it.
    let source = with_revisions(
        FakeSource::default(),
        7,
        &[("//test1/b.bin", 1, Action::Add, "binary", &COLLIDING_B)],
    );
    let mut source = with_revisions(
        source,
        9,
        &[("//test1/c.bin", 1, Action::Branch, "binary", &COLLIDING_B)],
    )
    .with_branch("//test1/c.bin", "//test1/b.bin");
    // 9 is looked up, and finds the recorded half, before 7 has printed.
    source.slow_describes.insert(7, Duration::from_millis(200));
    let options = ImportOptions {
        changelist_lookahead: 2,
        ..options(ContentReuse::On)
    };

    let built = build_changesets(&ctx, &repo, &source, DEPOT, None, &[7, 9], &options).await?;

    let branch = &built[1];
    assert_eq!(
        ("c.bin".to_owned(), Some(Bytes::from_static(&COLLIDING_B))),
        only_file(&ctx, &repo, &branch.changeset).await?,
        "the branch has the content of its own print"
    );
    assert_eq!(2, prints(&source), "the add, then the branch");
    assert_eq!(
        (1, 0, 0),
        (
            branch.timings.uploaded_files,
            branch.timings.content_reuse.reused_files,
            branch.timings.content_reuse.reused_bytes
        )
    );
    Ok(())
}
