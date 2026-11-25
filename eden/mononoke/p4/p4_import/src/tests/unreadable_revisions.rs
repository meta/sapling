/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use blobstore::BlobstoreBytes;
use blobstore::KeyedBlobstore;
use md5::Digest;
use md5::Md5;
use p4_types::Md5Digest;

use super::*;

/// Approves each `(change, depot path, revision)`.
pub(super) fn approving(entries: &[(u64, &str, u32)]) -> ImportOptions {
    let approved = entries
        .iter()
        .map(|&(change, depot_path, revision)| UnreadableRevision {
            change,
            depot_path: depot_path.to_owned(),
            revision,
        })
        .collect();
    ImportOptions {
        unreadable_revisions: Arc::new(approved),
        ..Default::default()
    }
}

/// `source` whose prints of these `path#rev` fail with `ArchiveUnreadable` halfway.
pub(super) fn unreadable_halfway(source: FakeSource, keys: &[&str]) -> FakeSource {
    FakeSource {
        unreadable: keys.iter().map(|key| key.to_string()).collect(),
        ..source
    }
}

/// A source whose `print` of these `path#rev` fails with `ArchiveUnreadable` before any
/// content, as a print that fails at once does.
struct UnreadableAtPrint {
    inner: FakeSource,
    keys: HashSet<String>,
}

#[async_trait]
impl ChangeSource for UnreadableAtPrint {
    async fn server_info(&self) -> Result<ServerInfo> {
        self.inner.server_info().await
    }

    async fn latest_change(&self) -> Result<u64> {
        self.inner.latest_change().await
    }

    async fn depots(&self) -> Result<Vec<Depot>> {
        self.inner.depots().await
    }

    async fn submitted_changes(
        &self,
        depot_root: &str,
        after: Option<u64>,
        upto: u64,
        limit: usize,
    ) -> Result<Vec<u64>> {
        self.inner
            .submitted_changes(depot_root, after, upto, limit)
            .await
    }

    async fn describe(&self, change: u64) -> Result<Changelist> {
        self.inner.describe(change).await
    }

    async fn print(&self, file: &FileRevision) -> Result<BoxStream<'static, Result<Bytes>>> {
        if self.keys.contains(&key(file)) {
            self.inner.record("print");
            return Err(ArchiveUnreadable {
                depot_path: file.depot_path.clone(),
                revision: file.revision,
                missing: true,
            }
            .into());
        }
        self.inner.print(file).await
    }

    async fn integrations(
        &self,
        files: &[&FileRevision],
    ) -> Result<HashMap<String, Vec<Integration>>> {
        self.inner.integrations(files).await
    }
}

pub(super) fn prints(source: &FakeSource) -> usize {
    let calls = source.calls.lock().unwrap();
    calls.iter().filter(|call| **call == "print").count()
}

/// 7 adds `a.txt` and `keep.txt`, 9 edits `a.txt` and adds `b.txt`, 11 edits `a.txt` again.
pub(super) fn edits_of_a() -> FakeSource {
    FakeSource::default()
        .with_changelist(
            7,
            &[
                ("//test1/a.txt", 1, Action::Add, Some(b"one\n")),
                ("//test1/keep.txt", 1, Action::Add, Some(b"keep\n")),
            ],
        )
        .with_changelist(
            9,
            &[
                ("//test1/a.txt", 2, Action::Edit, Some(b"two\n")),
                ("//test1/b.txt", 1, Action::Add, Some(b"b\n")),
            ],
        )
        .with_changelist(11, &[("//test1/a.txt", 3, Action::Edit, Some(b"three\n"))])
}

pub(super) fn extra<'a>(changeset: &'a BonsaiChangeset, key: &str) -> Option<&'a [u8]> {
    changeset
        .hg_extra()
        .find_map(|(extra_key, value)| (extra_key == key).then_some(value))
}

#[mononoke::fbinit_test]
async fn test_an_approved_unreadable_revision_is_a_deletion_until_its_next_edit(
    fb: FacebookInit,
) -> Result<()> {
    let (ctx, dir) = scuba_logging_context(fb)?;
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = unreadable_halfway(edits_of_a(), &["//test1/a.txt#2"]);
    let options = approving(&[(9, "//test1/a.txt", 2)]);

    let batch =
        import_changelists(&ctx, &repo, &source, DEPOT, &master, &[7, 9, 11], &options).await?;

    let [_, unreadable, edit] = &batch.changelists[..] else {
        panic!("three changelists expected: {batch:?}");
    };
    assert_eq!(
        Some(edit.cs_id),
        bookmark_target(&ctx, &repo, &master).await?
    );
    assert_eq!(Some(unreadable.cs_id), mapped(&ctx, &repo, 9).await?);
    let reported: Vec<_> = batch
        .changelists
        .iter()
        .map(|imported| imported.unreadable_revisions)
        .collect();
    assert_eq!(vec![0, 1, 0], reported);

    let changeset = unreadable.cs_id.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        vec![
            ("a.txt".to_owned(), None, None),
            ("b.txt".to_owned(), Some(Bytes::from_static(b"b\n")), None),
        ],
        files(&ctx, &repo, &changeset).await?,
        "a.txt is deleted, not given the half that printed, and b.txt is imported"
    );
    assert_eq!(
        Some(&b"2\ta.txt\n"[..]),
        extra(&changeset, p4_transform::UNREADABLE_REVS_EXTRA)
    );
    let revs = mononoke_types::P4FileRevs::from_bcs(&changeset).context("no p4-file-revs")?;
    assert_eq!(Some(2), revs.revision(&path("a.txt"))?);

    let changeset = edit.cs_id.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        vec![(
            "a.txt".to_owned(),
            Some(Bytes::from_static(b"three\n")),
            None
        )],
        files(&ctx, &repo, &changeset).await?,
        "the next edit brings a.txt back"
    );
    assert_eq!(None, extra(&changeset, p4_transform::UNREADABLE_REVS_EXTRA));

    let rows = scuba_rows(&dir, "P4 changelist imported")?;
    let columns: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                row["p4_change"].clone(),
                row.get("unreadable_revisions").cloned(),
            )
        })
        .collect();
    assert_eq!(
        vec![
            (Value::from(7), None),
            (Value::from(9), Some(Value::from(1))),
            (Value::from(11), None),
        ],
        columns
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_unapproved_unreadable_revisions_fail_the_changelist_naming_each(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let inner = FakeSource::default()
        .with_changelist(
            7,
            &[
                ("//test1/a.txt", 1, Action::Add, Some(b"one\n")),
                ("//test1/c.txt", 1, Action::Add, Some(b"c\n")),
            ],
        )
        .with_changelist(
            9,
            &[
                ("//test1/c.txt", 2, Action::Edit, Some(b"cc\n")),
                ("//test1/a.txt", 2, Action::Edit, Some(b"two\n")),
                ("//test1/b.txt", 1, Action::Add, Some(b"b\n")),
            ],
        );
    let source = UnreadableAtPrint {
        inner: unreadable_halfway(inner, &["//test1/c.txt#2"]),
        keys: HashSet::from(["//test1/a.txt#2".to_owned()]),
    };
    let options = ImportOptions {
        file_concurrency: 1,
        ..approving(&[(9, "//test1/b.txt", 1)])
    };

    let error = import_changelists(&ctx, &repo, &source, DEPOT, &master, &[7, 9], &options)
        .await
        .unwrap_err();

    let unreadable: &UnreadableRevisions = error
        .downcast_ref()
        .with_context(|| format!("not unreadable: {error:#}"))?;
    let named: Vec<_> = unreadable
        .revisions
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        (
            9,
            vec!["//test1/a.txt#2".to_owned(), "//test1/c.txt#2".to_owned()],
            false
        ),
        (unreadable.change, named, unreadable.incomplete)
    );
    assert!(
        format!("{error:#}").contains(
            "building changelist 9: the server cannot read its archive of 2 unapproved \
             revision(s) of changelist 9: //test1/a.txt#2, //test1/c.txt#2"
        ),
        "{error:#}"
    );
    assert_eq!(Some(Stage::Build), failed_stage(&error));
    assert_eq!(5, prints(&source.inner), "b.txt is printed after both fail");
    assert_eq!(None, bookmark_target(&ctx, &repo, &master).await?);
    assert_eq!(None, mapped(&ctx, &repo, 7).await?);
    assert_eq!(None, mapped(&ctx, &repo, 9).await?);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_an_approval_applies_to_its_changelist_path_and_revision_only(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let source = unreadable_halfway(edits_of_a(), &["//test1/a.txt#2"]);

    for approval in [
        (11, "//test1/a.txt", 2),
        (9, "//test1/a.txt", 1),
        (9, "//test1/a.txt", 3),
        (9, "//test1/b.txt", 2),
        (9, "//test1/A.txt", 2),
        (9, "//test1/a.TXT", 2),
        (9, "//other/a.txt", 2),
    ] {
        let error = build_changesets(
            &ctx,
            &repo,
            &source,
            DEPOT,
            None,
            &[7, 9],
            &approving(&[approval]),
        )
        .await
        .unwrap_err();

        assert!(
            error.downcast_ref::<UnreadableRevisions>().is_some(),
            "{approval:?}: {error:#}"
        );
    }
    let built = build_changesets(
        &ctx,
        &repo,
        &source,
        DEPOT,
        None,
        &[7, 9],
        &approving(&[(9, "//test1/a.txt", 2)]),
    )
    .await?;
    assert_eq!(
        1,
        built[1].skipped_revisions.len(),
        "the exact approval applies"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_an_approved_revision_that_prints_keeps_its_content(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let built = build_changesets(
        &ctx,
        &repo,
        &edits_of_a(),
        DEPOT,
        None,
        &[7, 9],
        &approving(&[(9, "//test1/a.txt", 2)]),
    )
    .await?;

    let changeset = &built[1].changeset;
    assert_eq!(
        vec![
            ("a.txt".to_owned(), Some(Bytes::from_static(b"two\n")), None),
            ("b.txt".to_owned(), Some(Bytes::from_static(b"b\n")), None),
        ],
        files(&ctx, &repo, changeset).await?
    );
    assert_eq!(None, extra(changeset, p4_transform::UNREADABLE_REVS_EXTRA));
    assert_eq!(0, built[1].skipped_revisions.len());
    Ok(())
}

/// Deleting a path the parent does not have, twice: the first revision has no content, then
/// Perforce deletes the file.
#[mononoke::fbinit_test]
async fn test_an_unreadable_first_revision_and_its_deletion_import(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = unreadable_halfway(
        FakeSource::default()
            .with_changelist(7, &[("//test1/keep.txt", 1, Action::Add, Some(b"keep\n"))])
            .with_changelist(9, &[("//test1/new.txt", 1, Action::Add, Some(b"new\n"))])
            .with_changelist(11, &[("//test1/new.txt", 2, Action::Delete, None)]),
        &["//test1/new.txt#1"],
    );
    let options = approving(&[(9, "//test1/new.txt", 1)]);

    let batch =
        import_changelists(&ctx, &repo, &source, DEPOT, &master, &[7, 9, 11], &options).await?;

    let changesets = stream::iter(&batch.changelists)
        .then(|imported| imported.cs_id.load(&ctx, repo.repo_blobstore()))
        .try_collect::<Vec<_>>()
        .await?;
    for changeset in &changesets[1..] {
        assert_eq!(
            vec![("new.txt".to_owned(), None, None)],
            files(&ctx, &repo, changeset).await?
        );
    }
    assert_eq!(
        Some(batch.changelists[2].cs_id),
        bookmark_target(&ctx, &repo, &master).await?
    );
    Ok(())
}

/// The file in the content manifest of `cs_id` at `path`, if there is one.
async fn file_at(
    ctx: &CoreContext,
    repo: &TestRepo,
    cs_id: ChangesetId,
    path: &str,
) -> Result<Option<(ContentId, FileType)>> {
    let root = base_root_for(
        ctx,
        repo,
        Some(cs_id),
        Built::default(),
        [self::path(path)].iter(),
    )
    .await?;
    file_in_parent(ctx, repo, root, Built::default(), &self::path(path)).await
}

/// Perforce lists the files of a directory a file replaces as deleted. With that file
/// unreadable, both the file and the directory's files are deletions, and the next revision
/// of the file adds it.
#[mononoke::fbinit_test]
async fn test_an_unreadable_file_replacing_a_deleted_directory_imports(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = unreadable_halfway(
        FakeSource::default()
            .with_changelist(7, &[("//test1/dir/f.txt", 1, Action::Add, Some(b"f\n"))])
            .with_changelist(
                9,
                &[
                    ("//test1/dir", 1, Action::Add, Some(b"now a file\n")),
                    ("//test1/dir/f.txt", 2, Action::Delete, None),
                ],
            )
            .with_changelist(11, &[("//test1/dir", 2, Action::Edit, Some(b"back\n"))]),
        &["//test1/dir#1"],
    );
    let options = approving(&[(9, "//test1/dir", 1)]);

    let batch =
        import_changelists(&ctx, &repo, &source, DEPOT, &master, &[7, 9, 11], &options).await?;

    let replaced = batch.changelists[1].cs_id;
    let changeset = replaced.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        vec![
            ("dir".to_owned(), None, None),
            ("dir/f.txt".to_owned(), None, None),
        ],
        files(&ctx, &repo, &changeset).await?
    );
    assert_eq!(0, batch.changelists[1].shadowed_deletions);
    assert_eq!(None, file_at(&ctx, &repo, replaced, "dir").await?);
    assert_eq!(None, file_at(&ctx, &repo, replaced, "dir/f.txt").await?);
    let back = file_at(&ctx, &repo, batch.changelists[2].cs_id, "dir").await?;
    let (content_id, _) = back.context("dir is not a file after 11")?;
    let content = filestore::fetch_concat(repo.repo_blobstore(), &ctx, content_id).await?;
    assert_eq!(Bytes::from_static(b"back\n"), content);
    Ok(())
}

fn digest(content: &[u8]) -> Md5Digest {
    Md5Digest::new(Md5::digest(content).into())
}

#[mononoke::fbinit_test]
async fn test_an_approved_revision_is_printed_not_reused(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let content: &[u8] = b"one\n";
    let stored = upload_content(&ctx, &repo, 4, chunks(&[content])).await?;
    // A valid record of the branch's MD5 and size, as an earlier import writes it.
    repo.repo_blobstore()
        .put(
            &ctx,
            format!("p4.content_by_md5.v1.{}.4", digest(content)),
            BlobstoreBytes::from_bytes(stored.content_id.to_string()),
        )
        .await?;
    let mut source = unreadable_halfway(
        FakeSource::default()
            .with_changelist(9, &[("//test1/b.txt", 1, Action::Branch, Some(content))])
            .with_branch("//test1/b.txt", "//test1/nowhere.txt"),
        &["//test1/b.txt#1"],
    );
    if let Some(changelist) = source.changelists.get_mut(&9) {
        changelist.files[0].digest = Some(digest(content));
    }
    let options = ImportOptions {
        content_reuse: ContentReuse::On,
        ..approving(&[(9, "//test1/b.txt", 1)])
    };

    let built = build_changesets(&ctx, &repo, &source, DEPOT, None, &[9], &options).await?;

    assert_eq!(
        1,
        prints(&source),
        "printed, though a record names its content"
    );
    assert_eq!(
        vec![("b.txt".to_owned(), None, None)],
        files(&ctx, &repo, &built[0].changeset).await?
    );
    assert_eq!(
        (1, ContentReuseReport::default()),
        (
            built[0].skipped_revisions.len(),
            built[0].timings.content_reuse.clone()
        ),
        "nothing looked up or reused"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_approvals_change_no_changeset_of_a_readable_history(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ids = async |options: &ImportOptions| -> Result<Vec<ChangesetId>> {
        let source = branch_after_two_changelists("//test1/a.txt");
        let built =
            build_changesets(&ctx, &repo, &source, DEPOT, None, &[7, 9, 11], options).await?;
        Ok(built
            .iter()
            .map(|built| built.changeset.get_changeset_id())
            .collect())
    };

    let without = ids(&ImportOptions::default()).await?;
    let with = ids(&approving(&[
        (9, "//test1/a.txt", 2),
        (11, "//test1/c.txt", 1),
        (5, "//test1/x.txt", 1),
    ]))
    .await?;

    assert_eq!(3, without.len());
    assert_eq!(without, with);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_unapproved_unreadable_revisions_stop_at_a_hundred(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let keys: Vec<&'static str> = (0..105)
        .map(|i| &*format!("//test1/f{i:03}.txt#1").leak())
        .collect();
    let files: Vec<FileSpec> = keys
        .iter()
        .map(|key| {
            let depot_path = key.strip_suffix("#1").unwrap_or(key);
            (depot_path, 1, Action::Add, Some(&b"x\n"[..]))
        })
        .collect();
    let source = unreadable_halfway(FakeSource::default().with_changelist(9, &files), &keys);
    // Without approvals or content reuse, a changelist uploads as it always has: the first
    // failed print fails it.
    let options = ImportOptions {
        file_concurrency: 1,
        ..approving(&[(9, "//test1/elsewhere.txt", 1)])
    };

    let error = build_changesets(&ctx, &repo, &source, DEPOT, None, &[9], &options)
        .await
        .unwrap_err();

    let unreadable: &UnreadableRevisions = error
        .downcast_ref()
        .with_context(|| format!("not unreadable: {error:#}"))?;
    assert_eq!(
        (100, true),
        (unreadable.revisions.len(), unreadable.incomplete)
    );
    assert!(
        format!("{error:#}").contains(
            "100 unapproved revision(s) of changelist 9, the first ones found; there may be more: \
             //test1/f000.txt#1, "
        ),
        "{error:#}"
    );
    assert_eq!(100, prints(&source), "the other 5 are not printed");
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_another_error_is_not_hidden_by_unreadable_revisions(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let mut source = unreadable_halfway(
        FakeSource::default().with_changelist(
            9,
            &[
                ("//test1/a.txt", 1, Action::Add, Some(b"a\n")),
                ("//test1/z.txt", 1, Action::Add, Some(b"z\n")),
            ],
        ),
        &["//test1/a.txt#1"],
    );
    // The fake fails a print it has no content for.
    source.contents.remove("//test1/z.txt#1");
    // Without approvals or content reuse, a changelist uploads as it always has: the first
    // failed print fails it.
    let options = ImportOptions {
        file_concurrency: 1,
        ..approving(&[(9, "//test1/elsewhere.txt", 1)])
    };

    let error = build_changesets(&ctx, &repo, &source, DEPOT, None, &[9], &options)
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("unexpected print of //test1/z.txt#1"),
        "{error:#}"
    );
    assert!(
        error.downcast_ref::<UnreadableRevisions>().is_none(),
        "{error:#}"
    );
    Ok(())
}

#[mononoke::test]
fn test_an_error_naming_another_revision_is_not_this_revisions() {
    let printed = FileRevision {
        revision: 2,
        ..revision("//test1/a.txt", Action::Edit, "text", None)
    };
    let failed = |depot_path: &str, revision| {
        let unreadable = ArchiveUnreadable {
            depot_path: depot_path.to_owned(),
            revision,
            missing: true,
        };
        anyhow!("Librarian checkout failed")
            .context(unreadable)
            .context("storing //test1/a.txt#2")
    };

    assert!(crate::unreadable::is_archive_unreadable(
        &failed("//test1/a.txt", 2),
        &printed
    ));
    assert!(!crate::unreadable::is_archive_unreadable(
        &failed("//test1/a.txt", 3),
        &printed
    ));
    assert!(!crate::unreadable::is_archive_unreadable(
        &failed("//test1/b.txt", 2),
        &printed
    ));
}
