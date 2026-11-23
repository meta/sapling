/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use tokio::sync::Semaphore;

use super::*;

fn with_uploads() -> ImportOptions {
    ImportOptions {
        copy_sources_with_uploads: true,
        ..Default::default()
    }
}

/// A source whose prints and integration records each wait until the other has been asked
/// for, so a changelist is fetched only if the two are in flight together.
struct PrintsAndRecordsWaitForEachOther {
    inner: FakeSource,
    printing: Semaphore,
    asked_for_records: Semaphore,
}

impl PrintsAndRecordsWaitForEachOther {
    fn new(inner: FakeSource) -> Self {
        Self {
            inner,
            printing: Semaphore::new(0),
            asked_for_records: Semaphore::new(0),
        }
    }
}

#[async_trait]
impl ChangeSource for PrintsAndRecordsWaitForEachOther {
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
        self.printing.add_permits(1);
        self.asked_for_records.acquire().await?.forget();
        self.inner.print(file).await
    }

    async fn integrations(
        &self,
        files: &[&FileRevision],
    ) -> Result<HashMap<String, Vec<Integration>>> {
        // One for each print of the changelist.
        self.asked_for_records.add_permits(2);
        self.printing.acquire().await?.forget();
        self.inner.integrations(files).await
    }
}

/// An add and a branch from a file the depot does not have, in one changelist.
fn add_and_branch() -> FakeSource {
    FakeSource::default().with_changelist(
        7,
        &[
            ("//test1/a.txt", 1, Action::Add, Some(b"one\n")),
            ("//test1/b.txt", 1, Action::Branch, Some(b"x\n")),
        ],
    )
}

#[mononoke::fbinit_test]
async fn test_integration_records_are_asked_for_while_contents_upload(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let source = PrintsAndRecordsWaitForEachOther::new(
        add_and_branch().with_branch("//test1/b.txt", "//test1/nowhere.txt"),
    );

    let built = tokio::time::timeout(
        Duration::from_secs(60),
        build_changesets(&ctx, &repo, &source, DEPOT, None, &[7], &with_uploads()),
    )
    .await
    .context("the prints and the integration records were not in flight together")??;

    let [built] = &built[..] else {
        panic!("one changeset expected: {built:?}");
    };
    assert_eq!(2, built.timings.uploaded_files);
    assert_eq!(
        1, built.dropped_copy_sources,
        "the branch's source is not a file in the parent"
    );
    Ok(())
}

/// The changeset of each changelist, and what the last one's `c.txt` was copied from.
async fn history(
    ctx: &CoreContext,
    repo: &TestRepo,
    options: &ImportOptions,
) -> Result<(Vec<ChangesetId>, Option<(NonRootMPath, ChangesetId)>)> {
    let source = branch_after_two_changelists("//test1/a.txt");
    let built = build_changesets(ctx, repo, &source, DEPOT, None, &[7, 9, 11], options).await?;
    let ids: Vec<_> = built
        .iter()
        .map(|built| built.changeset.get_changeset_id())
        .collect();
    let last = &built.last().context("nothing built")?.changeset;
    let copy_from = files(ctx, repo, last).await?[0].2.clone();
    Ok((ids, copy_from))
}

#[mononoke::fbinit_test]
async fn test_the_changesets_are_the_same_either_way(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let one_after_the_other = history(&ctx, &repo, &ImportOptions::default()).await?;
    let together = history(&ctx, &repo, &with_uploads()).await?;

    let (ids, copy_from) = &together;
    assert_eq!(
        Some((path("a.txt"), ids[1])),
        *copy_from,
        "c.txt is copied from a.txt as changelist 9 left it"
    );
    assert_eq!(one_after_the_other, together);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_failed_integration_records_name_the_changelist(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    // No records for the branch: the fake fails its filelog.
    let source = add_and_branch();

    let error = build_changesets(&ctx, &repo, &source, DEPOT, None, &[7], &with_uploads())
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}")
            .contains("building changelist 7: unexpected filelog of //test1/b.txt#1"),
        "{error:?}"
    );
    Ok(())
}
