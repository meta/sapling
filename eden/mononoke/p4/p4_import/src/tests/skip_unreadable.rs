/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::VecDeque;

use super::content_reuse::with_revisions;
use super::unreadable_revisions::approving;
use super::unreadable_revisions::edits_of_a;
use super::unreadable_revisions::extra;
use super::unreadable_revisions::prints;
use super::unreadable_revisions::unreadable_halfway;
use super::*;

/// Skips after `retries` retries that follow each other at once.
fn skipping(retries: u32) -> ImportOptions {
    ImportOptions {
        skip_unreadable: Some(SkipUnreadable {
            retries,
            first_retry_delay: Duration::ZERO,
        }),
        ..Default::default()
    }
}

/// How a print fails.
enum Failure {
    /// The server has no archive of the revision.
    Missing,
    /// The server has an archive of the revision but cannot read it.
    Unreadable,
    ConnectionReset,
}

/// A source whose prints of a `path#rev` fail as queued for it, one failure per print, and
/// work once the queue is empty.
struct FailsFirst {
    inner: FakeSource,
    failures: Mutex<HashMap<String, VecDeque<Failure>>>,
}

impl FailsFirst {
    fn new(inner: FakeSource, key: &str, failures: impl IntoIterator<Item = Failure>) -> Self {
        let failures = HashMap::from([(key.to_owned(), failures.into_iter().collect())]);
        Self {
            inner,
            failures: Mutex::new(failures),
        }
    }
}

#[async_trait]
impl ChangeSource for FailsFirst {
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
        let failure = self
            .failures
            .lock()
            .unwrap()
            .get_mut(&key(file))
            .and_then(VecDeque::pop_front);
        match failure {
            Some(failure @ (Failure::Missing | Failure::Unreadable)) => {
                self.inner.record("print");
                Err(ArchiveUnreadable {
                    depot_path: file.depot_path.clone(),
                    revision: file.revision,
                    missing: matches!(failure, Failure::Missing),
                }
                .into())
            }
            Some(Failure::ConnectionReset) => {
                self.inner.record("print");
                Err(anyhow!("connection reset"))
            }
            None => self.inner.print(file).await,
        }
    }

    async fn integrations(
        &self,
        files: &[&FileRevision],
    ) -> Result<HashMap<String, Vec<Integration>>> {
        self.inner.integrations(files).await
    }
}

#[mononoke::fbinit_test]
async fn test_a_revision_unreadable_on_every_try_is_skipped_and_recorded(
    fb: FacebookInit,
) -> Result<()> {
    for retries in [0, 5] {
        let (ctx, dir) = scuba_logging_context(fb)?;
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let master = BookmarkKey::new("master")?;
        let source = unreadable_halfway(edits_of_a(), &["//test1/a.txt#2"]);

        let batch = import_changelists(
            &ctx,
            &repo,
            &source,
            DEPOT,
            &master,
            &[7, 9, 11],
            &skipping(retries),
        )
        .await?;

        // Four readable files, and a.txt#2 once and then once per retry.
        assert_eq!(
            4 + 1 + retries as usize,
            prints(&source),
            "{retries} retries"
        );
        let [_, skipped, edit] = &batch.changelists[..] else {
            panic!("three changelists expected: {batch:?}");
        };
        assert_eq!(
            Some(edit.cs_id),
            bookmark_target(&ctx, &repo, &master).await?
        );
        assert_eq!(1, skipped.unreadable_revisions);
        let changeset = skipped.cs_id.load(&ctx, repo.repo_blobstore()).await?;
        assert_eq!(
            vec![
                ("a.txt".to_owned(), None, None),
                ("b.txt".to_owned(), Some(Bytes::from_static(b"b\n")), None),
            ],
            files(&ctx, &repo, &changeset).await?,
            "a.txt is deleted and b.txt is imported"
        );
        assert_eq!(
            Some(&b"2\ta.txt\n"[..]),
            extra(&changeset, p4_transform::UNREADABLE_REVS_EXTRA)
        );

        let rows = scuba_rows(&dir, "P4 revision skipped")?;
        let [row] = &rows[..] else {
            panic!("one skipped revision expected: {rows:?}");
        };
        assert_eq!(row["p4_change"], 9);
        assert_eq!(row["changeset_id"], skipped.cs_id.to_string());
        assert_eq!(row["depot_path"], "//test1/a.txt");
        assert_eq!(row["revision"], 2);
        assert_eq!(row["skip_reason"], "retries_failed");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_a_revision_readable_on_a_retry_keeps_its_content(fb: FacebookInit) -> Result<()> {
    let (ctx, dir) = scuba_logging_context(fb)?;
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = FailsFirst::new(
        edits_of_a(),
        "//test1/a.txt#2",
        [Failure::Missing, Failure::Missing],
    );

    let batch =
        import_changelists(&ctx, &repo, &source, DEPOT, &master, &[7, 9], &skipping(5)).await?;

    assert_eq!(
        3 + 3,
        prints(&source.inner),
        "three other files, and a.txt#2 three times"
    );
    let edit = &batch.changelists[1];
    assert_eq!(0, edit.unreadable_revisions);
    let changeset = edit.cs_id.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(
        vec![
            ("a.txt".to_owned(), Some(Bytes::from_static(b"two\n")), None),
            ("b.txt".to_owned(), Some(Bytes::from_static(b"b\n")), None),
        ],
        files(&ctx, &repo, &changeset).await?
    );
    assert_eq!(None, extra(&changeset, p4_transform::UNREADABLE_REVS_EXTRA));
    assert_eq!(
        Vec::<Value>::new(),
        scuba_rows(&dir, "P4 revision skipped")?
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_another_failure_on_a_retry_fails_the_changelist(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = FailsFirst::new(
        edits_of_a(),
        "//test1/a.txt#2",
        [Failure::Missing, Failure::ConnectionReset],
    );

    let error = import_changelists(&ctx, &repo, &source, DEPOT, &master, &[7, 9], &skipping(5))
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("connection reset"),
        "{error:#}"
    );
    assert_eq!(None, bookmark_target(&ctx, &repo, &master).await?);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_an_archive_the_server_has_but_cannot_read_is_never_skipped(
    fb: FacebookInit,
) -> Result<()> {
    // On the first print, and on a retry after the archive was reported missing.
    for failures in [
        vec![Failure::Unreadable],
        vec![Failure::Missing, Failure::Missing, Failure::Unreadable],
    ] {
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let master = BookmarkKey::new("master")?;
        let tries = failures.len();
        let source = FailsFirst::new(edits_of_a(), "//test1/a.txt#2", failures);

        let error = import_changelists(&ctx, &repo, &source, DEPOT, &master, &[7, 9], &skipping(5))
            .await
            .unwrap_err();

        let unreadable = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<UnreadableRevisions>())
            .unwrap_or_else(|| panic!("{error:#}"));
        assert_eq!(
            vec!["//test1/a.txt#2".to_owned()],
            unreadable
                .revisions
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            3 + tries,
            prints(&source.inner),
            "three other files, and a.txt#2 not again after that failure"
        );
        assert_eq!(None, bookmark_target(&ctx, &repo, &master).await?);
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_each_wait_is_twice_the_one_before(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    tokio::time::pause();
    let options = ImportOptions {
        skip_unreadable: Some(SkipUnreadable {
            retries: 5,
            first_retry_delay: Duration::from_secs(4),
        }),
        ..Default::default()
    };
    // The revision prints after this many failures: 4, 4 + 8, ... and 4 + 8 + 16 + 32 + 64 s.
    for (failures, secs) in [(1, 4), (2, 12), (3, 28), (4, 60), (5, 124)] {
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let source = FailsFirst::new(
            edits_of_a(),
            "//test1/a.txt#2",
            (0..failures).map(|_| Failure::Missing),
        );
        let started = tokio::time::Instant::now();

        let built = build_changesets(&ctx, &repo, &source, DEPOT, None, &[7, 9], &options).await?;

        assert_eq!(0, built[1].skipped_revisions.len(), "{failures} failures");
        let waited = started.elapsed();
        assert!(
            (Duration::from_secs(secs)..Duration::from_secs(secs + 1)).contains(&waited),
            "{failures} failures: {waited:?}"
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_an_approved_revision_is_not_retried(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let source = unreadable_halfway(edits_of_a(), &["//test1/a.txt#2", "//test1/b.txt#1"]);
    let options = ImportOptions {
        skip_unreadable: skipping(5).skip_unreadable,
        ..approving(&[(9, "//test1/a.txt", 2)])
    };

    let built = build_changesets(&ctx, &repo, &source, DEPOT, None, &[7, 9], &options).await?;

    assert_eq!(
        2 + 1 + 6,
        prints(&source),
        "7's two files, a.txt#2 once, and b.txt#1 once and five times more"
    );
    let mut skipped = built[1].skipped_revisions.clone();
    skipped.sort_by(|a, b| a.depot_path.cmp(&b.depot_path));
    assert_eq!(
        vec![
            SkippedRevision {
                depot_path: "//test1/a.txt".to_owned(),
                revision: 2,
                reason: SkipReason::Approved,
            },
            SkippedRevision {
                depot_path: "//test1/b.txt".to_owned(),
                revision: 1,
                reason: SkipReason::RetriesFailed,
            },
        ],
        skipped
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_a_copy_of_stored_content_is_reused_not_skipped(fb: FacebookInit) -> Result<()> {
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
        &[("//test1/b.txt", 1, Action::Branch, "text", b"one\n")],
    )
    .with_branch("//test1/b.txt", "//test1/a.txt");
    // The branch's own archive is gone, but its content is a.txt's, stored by then.
    let source = unreadable_halfway(source, &["//test1/b.txt#1"]);
    let options = ImportOptions {
        content_reuse: ContentReuse::On,
        changelist_lookahead: 1,
        ..skipping(5)
    };

    let built = build_changesets(&ctx, &repo, &source, DEPOT, None, &[7, 9], &options).await?;

    assert_eq!(1, prints(&source), "only the add is printed");
    assert_eq!(Vec::<SkippedRevision>::new(), built[1].skipped_revisions);
    let branch = files(&ctx, &repo, &built[1].changeset).await?;
    assert_eq!(
        vec![("b.txt".to_owned(), Some(Bytes::from_static(b"one\n")))],
        branch
            .into_iter()
            .map(|(path, content, _)| (path, content))
            .collect::<Vec<_>>()
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_skipping_changes_no_changeset_of_a_readable_history(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let ids = |built: Vec<BuiltChangeset>| -> Vec<ChangesetId> {
        built
            .iter()
            .map(|built| built.changeset.get_changeset_id())
            .collect()
    };

    let without = build_changesets(
        &ctx,
        &repo,
        &edits_of_a(),
        DEPOT,
        None,
        &[7, 9, 11],
        &ImportOptions::default(),
    )
    .await?;
    let with = build_changesets(
        &ctx,
        &repo,
        &edits_of_a(),
        DEPOT,
        None,
        &[7, 9, 11],
        &skipping(5),
    )
    .await?;

    assert_eq!(3, with.len());
    assert_eq!(ids(without), ids(with));
    Ok(())
}
