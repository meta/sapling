/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;

use anyhow::anyhow;
use async_trait::async_trait;
use bonsai_p4_mapping::AddP4MappingErrorKind;
use bookmarks::Bookmarks;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use fbinit::FacebookInit;
use filestore::FilestoreConfig;
use futures::StreamExt;
use futures::stream;
use futures::stream::BoxStream;
use metaconfig_types::FilestoreParams;
use mononoke_macros::mononoke;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::FileChange;
use mononoke_types::GitLfs;
use mononoke_types::P4ChangelistId;
use mononoke_types_mocks::changesetid::ONES_CSID;
use mononoke_types_mocks::contentid::ONES_CTID;
use p4_types::Action;
use p4_types::FileRevision;
use p4_types::Integration;
use p4_types::IntegrationHow;
use repo_blobstore::RepoBlobstore;
use repo_derived_data::RepoDerivedData;
use repo_identity::RepoIdentity;
use test_repo_factory::TestRepoFactory;
use unodes::RootUnodeManifestId;

use super::*;

#[facet::container]
struct TestRepo {
    #[facet]
    bonsai_p4_mapping: dyn BonsaiP4Mapping,
    #[facet]
    bookmarks: dyn Bookmarks,
    #[facet]
    commit_graph: CommitGraph,
    #[facet]
    commit_graph_writer: dyn CommitGraphWriter,
    #[facet]
    filestore_config: FilestoreConfig,
    #[facet]
    repo_blobstore: RepoBlobstore,
    #[facet]
    repo_derived_data: RepoDerivedData,
    #[facet]
    repo_identity: RepoIdentity,
}

fn chunks(chunks: &[&[u8]]) -> impl Stream<Item = Result<Bytes>> + Send + use<> {
    let chunks: Vec<_> = chunks
        .iter()
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    stream::iter(chunks)
}

async fn upload_and_fetch(
    ctx: &CoreContext,
    repo: &TestRepo,
    content: &[&[u8]],
) -> Result<(ContentMetadataV2, Bytes)> {
    let size = content.iter().map(|c| c.len() as u64).sum();
    let metadata = upload_content(ctx, repo, size, chunks(content)).await?;
    let fetched = filestore::fetch_concat(repo.repo_blobstore(), ctx, metadata.content_id).await?;
    Ok((metadata, fetched))
}

#[mononoke::fbinit_test]
async fn test_content_reads_back_byte_identical(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let (metadata, fetched) = upload_and_fetch(&ctx, &repo, &[b"hello\nworld\n"]).await?;

    assert_eq!(&b"hello\nworld\n"[..], fetched);
    assert_eq!(12, metadata.total_size);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_content_stored_in_filestore_chunks_reads_back_byte_identical(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = TestRepoFactory::new(fb)?
        .with_config_override(|config| {
            config.filestore = Some(FilestoreParams {
                chunk_size: 1024 * 1024,
                concurrency: 2,
            })
        })
        .build()
        .await?;
    let content: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 1024 * 1024 + 7]).collect();
    let content: Vec<&[u8]> = content.iter().map(Vec::as_slice).collect();

    let (metadata, fetched) = upload_and_fetch(&ctx, &repo, &content).await?;

    assert_eq!(content.concat(), fetched);
    assert_eq!(5 * (1024 * 1024 + 7), metadata.total_size);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_empty_content(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let (metadata, fetched) = upload_and_fetch(&ctx, &repo, &[]).await?;

    assert!(fetched.is_empty());
    assert_eq!(0, metadata.total_size);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_content_longer_than_size_is_rejected(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let error = upload_content(&ctx, &repo, 3, chunks(&[b"abcd"]))
        .await
        .unwrap_err();

    assert!(format!("{error:#}").contains("Invalid size"), "{error:?}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_content_shorter_than_size_is_rejected(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let error = upload_content(&ctx, &repo, 5, chunks(&[b"abcd"]))
        .await
        .unwrap_err();

    assert!(format!("{error:#}").contains("Invalid size"), "{error:?}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_same_content_gets_the_same_content_id(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let (first, _) = upload_and_fetch(&ctx, &repo, &[b"same bytes"]).await?;
    let (second, _) = upload_and_fetch(&ctx, &repo, &[b"same ", b"bytes"]).await?;

    assert_eq!(first.content_id, second.content_id);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_symlink_is_stored_without_the_trailing_newline(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let metadata = upload_file_content(
        &ctx,
        &repo,
        FileType::Symlink,
        19,
        chunks(&[b"UPPERCASE-", b"TEST.TXT\n"]),
    )
    .await?;
    let fetched = filestore::fetch_concat(repo.repo_blobstore(), &ctx, metadata.content_id).await?;

    assert_eq!(&b"UPPERCASE-TEST.TXT"[..], fetched);
    assert_eq!(18, metadata.total_size);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_symlink_without_trailing_newline_is_rejected(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let error = upload_file_content(&ctx, &repo, FileType::Symlink, 6, chunks(&[b"target"]))
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("does not end with a newline"),
        "{error:?}"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_symlink_with_two_trailing_newlines_is_rejected(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let error = upload_file_content(&ctx, &repo, FileType::Symlink, 8, chunks(&[b"target\n\n"]))
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("more than one newline"),
        "{error:?}"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_symlink_shorter_than_size_is_rejected(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let error = upload_file_content(&ctx, &repo, FileType::Symlink, 8, chunks(&[b"target\n"]))
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("symlink content is 7 bytes but Perforce reports 8"),
        "{error:?}"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_symlink_longer_than_size_is_rejected(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let error = upload_file_content(
        &ctx,
        &repo,
        FileType::Symlink,
        6,
        chunks(&[b"targ", b"et\n", b"never read"]),
    )
    .await
    .unwrap_err();

    assert!(
        format!("{error:#}").contains("longer than Perforce reports (6 bytes)"),
        "{error:?}"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_symlink_over_64_kib_is_rejected_before_reading(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let unreadable =
        stream::poll_fn(|_| -> Poll<Option<Result<Bytes>>> { panic!("content was read") });

    let error = upload_file_content(&ctx, &repo, FileType::Symlink, 64 * 1024 + 1, unreadable)
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("symlink of 65537 bytes is larger than we support"),
        "{error:?}"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_regular_file_keeps_its_trailing_newline(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let metadata =
        upload_file_content(&ctx, &repo, FileType::Regular, 7, chunks(&[b"target\n"])).await?;
    let fetched = filestore::fetch_concat(repo.repo_blobstore(), &ctx, metadata.content_id).await?;

    assert_eq!(&b"target\n"[..], fetched);
    assert_eq!(7, metadata.total_size);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_regular_file_of_the_wrong_size_is_rejected(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let error = upload_file_content(&ctx, &repo, FileType::Regular, 6, chunks(&[b"target\n"]))
        .await
        .unwrap_err();

    assert!(format!("{error:#}").contains("Invalid size"), "{error:?}");
    Ok(())
}

/// Knows only what it is given: anything else fails the test.
#[derive(Default)]
struct FakeSource {
    changelists: HashMap<u64, Changelist>,
    /// Keyed by `path#rev`.
    contents: HashMap<String, &'static [u8]>,
    /// Keyed by `path#rev`.
    integrations: HashMap<String, Vec<Integration>>,
    /// The server's latest changelist, if not its newest one here.
    latest: Option<u64>,
    /// Each `(after, upto]` listed.
    listed: Mutex<Vec<(u64, u64)>>,
    describes: InFlight,
    prints: InFlight,
    filelogs: InFlight,
}

/// Counts the calls in flight at once.
#[derive(Default)]
struct InFlight {
    now: AtomicUsize,
    max: AtomicUsize,
}

impl InFlight {
    /// Lets other calls start before this one returns.
    async fn call(&self) {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(now, Ordering::SeqCst);
        let mut yielded = false;
        futures::future::poll_fn(|cx| {
            if yielded {
                return Poll::Ready(());
            }
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        })
        .await;
        self.now.fetch_sub(1, Ordering::SeqCst);
    }

    fn max(&self) -> usize {
        self.max.load(Ordering::SeqCst)
    }
}

fn key(file: &FileRevision) -> String {
    format!("{}#{}", file.depot_path, file.revision)
}

#[async_trait]
impl ChangeSource for FakeSource {
    async fn latest_change(&self) -> Result<u64> {
        let newest = self.changelists.keys().max().copied().unwrap_or(0);
        Ok(self.latest.unwrap_or(newest))
    }

    /// Every changelist it has in the range, whatever its depot.
    async fn submitted_changes(
        &self,
        _depot_root: &str,
        after: u64,
        upto: u64,
    ) -> Result<Vec<u64>> {
        self.listed.lock().unwrap().push((after, upto));
        let mut changes: Vec<_> = self
            .changelists
            .keys()
            .copied()
            .filter(|change| (after + 1..=upto).contains(change))
            .collect();
        changes.sort();
        Ok(changes)
    }

    async fn describe(&self, change: u64) -> Result<Changelist> {
        self.describes.call().await;
        self.changelists
            .get(&change)
            .cloned()
            .ok_or_else(|| anyhow!("unexpected describe of {change}"))
    }

    async fn print(&self, file: &FileRevision) -> Result<BoxStream<'static, Result<Bytes>>> {
        self.prints.call().await;
        let content = self
            .contents
            .get(&key(file))
            .ok_or_else(|| anyhow!("unexpected print of {}", key(file)))?;
        Ok(chunks(&[content]).boxed())
    }

    async fn integrations(
        &self,
        files: &[&FileRevision],
    ) -> Result<HashMap<String, Vec<Integration>>> {
        self.filelogs.call().await;
        files
            .iter()
            .map(|file| {
                let integrations = self.integrations.get(&key(file)).cloned();
                let integrations =
                    integrations.ok_or_else(|| anyhow!("unexpected filelog of {}", key(file)))?;
                Ok((file.depot_path.clone(), integrations))
            })
            .collect()
    }
}

type FileSpec = (&'static str, u32, Action, Option<&'static [u8]>);

impl FakeSource {
    fn with_changelist(mut self, number: u64, files: &[FileSpec]) -> Self {
        let files = files
            .iter()
            .map(|&(depot_path, rev, action, content)| {
                let file = FileRevision {
                    revision: rev,
                    size: content.map(|content| content.len() as u64),
                    ..revision(depot_path, action, "text", None)
                };
                if let Some(content) = content {
                    self.contents.insert(key(&file), content);
                }
                file
            })
            .collect();
        let changelist = Changelist {
            number,
            user: "alice".to_owned(),
            time: 1_760_000_000 + number,
            description: format!("change {number}\n"),
            description_bytes: None,
            files,
        };
        self.changelists.insert(number, changelist);
        self
    }

    fn with_branch(mut self, target: &str, source: &str) -> Self {
        let integration = Integration {
            how: IntegrationHow::BranchFrom,
            file: source.to_owned(),
            start_rev: None,
            end_rev: 1,
        };
        self.integrations
            .insert(format!("{target}#1"), vec![integration]);
        self
    }
}

fn revision(depot_path: &str, action: Action, file_type: &str, size: Option<u64>) -> FileRevision {
    FileRevision {
        depot_path: depot_path.to_owned(),
        action,
        file_type: file_type.to_owned(),
        revision: 1,
        size,
        digest: None,
    }
}

#[mononoke::fbinit_test]
async fn test_changelist_contents_are_stored_by_depot_path(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let source = FakeSource {
        contents: HashMap::from([
            ("//test1/add.txt#1".to_owned(), &b"hello\n"[..]),
            ("//test1/run.sh#1".to_owned(), &b"#!/bin/sh\n"[..]),
            ("//test1/link#1".to_owned(), &b"add.txt\n"[..]),
        ]),
        ..Default::default()
    };
    let changelist = Changelist {
        number: 42,
        user: "alice".to_owned(),
        time: 0,
        description: "mixed".to_owned(),
        description_bytes: None,
        files: vec![
            revision("//test1/add.txt", Action::Add, "text", Some(6)),
            revision("//test1/run.sh", Action::Edit, "text+x", Some(10)),
            revision("//test1/gone.txt", Action::Delete, "text", None),
            revision("//test1/link", Action::Add, "symlink", Some(8)),
            revision("//other/add.txt", Action::Add, "text", Some(6)),
        ],
    };

    let contents = upload_changelist_contents(
        &ctx,
        &repo,
        &source,
        "//test1/",
        &changelist,
        ImportOptions::default().file_concurrency,
    )
    .await?;

    let mut stored = Vec::new();
    for (path, metadata) in &contents {
        let content =
            filestore::fetch_concat(repo.repo_blobstore(), &ctx, metadata.content_id).await?;
        stored.push((path.as_str(), content));
    }
    stored.sort();
    assert_eq!(
        vec![
            ("//test1/add.txt", Bytes::from_static(b"hello\n")),
            ("//test1/link", Bytes::from_static(b"add.txt")),
            ("//test1/run.sh", Bytes::from_static(b"#!/bin/sh\n")),
        ],
        stored
    );
    Ok(())
}

/// An add, an edit, a branch copy and a delete over two changelists.
fn two_changelists() -> FakeSource {
    FakeSource::default()
        .with_changelist(
            7,
            &[
                ("//test1/a.txt", 1, Action::Add, Some(b"one\n")),
                ("//test1/gone.txt", 1, Action::Add, Some(b"bye\n")),
            ],
        )
        .with_changelist(
            9,
            &[
                ("//test1/a.txt", 2, Action::Edit, Some(b"two\n")),
                ("//test1/b.txt", 1, Action::Branch, Some(b"one\n")),
                ("//test1/gone.txt", 2, Action::Delete, None),
            ],
        )
        .with_branch("//test1/b.txt", "//test1/a.txt")
}

fn path(path: &str) -> NonRootMPath {
    NonRootMPath::new(path).unwrap()
}

/// The files `changeset` changes: content for a change, `None` for a deletion.
async fn files(
    ctx: &CoreContext,
    repo: &TestRepo,
    changeset: &BonsaiChangeset,
) -> Result<Vec<(String, Option<Bytes>, Option<(NonRootMPath, ChangesetId)>)>> {
    let mut files = Vec::new();
    for (path, change) in changeset.file_changes() {
        let content = match change.content_id() {
            Some(id) => Some(filestore::fetch_concat(repo.repo_blobstore(), ctx, id).await?),
            None => None,
        };
        files.push((path.to_string(), content, change.copy_from().cloned()));
    }
    Ok(files)
}

#[mononoke::fbinit_test]
async fn test_changelists_build_a_linear_history(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let built = build_changesets(
        &ctx,
        &repo,
        &two_changelists(),
        "//test1/",
        None,
        &[7, 9],
        &ImportOptions::default(),
    )
    .await?;

    let [first, second] = &built[..] else {
        panic!("two changesets expected: {built:?}");
    };
    assert_eq!((7, 9), (first.change, second.change));
    assert_eq!(0, second.dropped_copy_sources);
    let (first, second) = (&first.changeset, &second.changeset);
    let first_id = first.get_changeset_id();
    assert_eq!(0, first.parents().count());
    assert_eq!(vec![first_id], second.parents().collect::<Vec<_>>());
    assert_eq!(P4ChangelistId::new(7), P4ChangelistId::from_bcs(first)?);
    assert_eq!(P4ChangelistId::new(9), P4ChangelistId::from_bcs(second)?);
    assert_eq!(
        vec![
            ("a.txt".to_owned(), Some(Bytes::from_static(b"one\n")), None),
            (
                "gone.txt".to_owned(),
                Some(Bytes::from_static(b"bye\n")),
                None
            ),
        ],
        files(&ctx, &repo, first).await?
    );
    assert_eq!(
        vec![
            ("a.txt".to_owned(), Some(Bytes::from_static(b"two\n")), None),
            (
                "b.txt".to_owned(),
                Some(Bytes::from_static(b"one\n")),
                Some((path("a.txt"), first_id))
            ),
            ("gone.txt".to_owned(), None, None),
        ],
        files(&ctx, &repo, second).await?
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_failure_names_the_changelist(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let error = build_changesets(
        &ctx,
        &repo,
        &two_changelists(),
        "//test1/",
        None,
        &[7, 8],
        &ImportOptions::default(),
    )
    .await
    .unwrap_err();

    assert!(
        format!("{error:#}").contains("building changelist 8: unexpected describe of 8"),
        "{error:?}"
    );
    Ok(())
}

/// A changeset adding (`true`) or deleting (`false`) each path.
fn changeset_of(changes: &[(&str, bool)]) -> BonsaiChangeset {
    let change = |added| match added {
        true => FileChange::tracked(ONES_CTID, FileType::Regular, 1, None, GitLfs::FullContent),
        false => FileChange::Deletion,
    };
    BonsaiChangesetMut {
        file_changes: changes
            .iter()
            .map(|&(p, added)| (path(p), change(added)))
            .collect(),
        ..Default::default()
    }
    .freeze()
    .unwrap()
}

#[mononoke::test]
fn test_file_replacing_a_directory_or_becoming_one_is_not_a_file() {
    let history = [
        changeset_of(&[("a/b", true), ("d", true)]),
        changeset_of(&[("a", true), ("d/e", true)]),
    ];

    let regular = Some(Some((ONES_CTID, FileType::Regular)));
    assert_eq!(Some(None), file_after(history.iter(), &path("a/b")));
    assert_eq!(regular, file_after(history.iter(), &path("a")));
    assert_eq!(Some(None), file_after(history[..1].iter(), &path("a")));
    assert_eq!(Some(None), file_after(history.iter(), &path("d")));
    assert_eq!(None, file_after(history.iter(), &path("c")));
}

#[mononoke::fbinit_test]
async fn test_saved_parent_gives_the_file_content_and_type(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let imported = import_changelists(
        &ctx,
        &repo,
        &two_changelists(),
        DEPOT,
        &master,
        &[7, 9],
        &ImportOptions::default(),
    )
    .await?
    .changelists;
    let base = Some(imported[1].cs_id);
    let head = imported[1].cs_id.load(&ctx, repo.repo_blobstore()).await?;
    let a = head
        .file_changes()
        .find_map(|(p, change)| (*p == path("a.txt")).then(|| change.content_id()))
        .flatten();

    let paths = [path("a.txt"), path("gone.txt")];
    let base_root = base_root_for(&ctx, &repo, base, &[], paths.iter()).await?;
    assert!(base_root.is_some());
    let no_copies = base_root_for(&ctx, &repo, base, &[], std::iter::empty()).await?;
    assert_eq!(None, no_copies);

    let file = file_in_parent(&ctx, &repo, base_root, &[], &paths[0]).await?;
    let gone = file_in_parent(&ctx, &repo, base_root, &[], &paths[1]).await?;

    assert_eq!(a.map(|id| (id, FileType::Regular)), file);
    assert_eq!(None, gone);
    Ok(())
}

/// `c.txt` branched from `source` on top of `two_changelists`.
fn branch_after_two_changelists(source: &str) -> FakeSource {
    two_changelists()
        .with_changelist(11, &[("//test1/c.txt", 1, Action::Branch, Some(b"x\n"))])
        .with_branch("//test1/c.txt", source)
}

async fn copy_from_of_c(
    ctx: &CoreContext,
    repo: &TestRepo,
    base: Option<ChangesetId>,
    changes: &[u64],
    source: &str,
) -> Result<(Option<(NonRootMPath, ChangesetId)>, usize)> {
    let source = branch_after_two_changelists(source);
    let built = build_changesets(
        ctx,
        repo,
        &source,
        "//test1/",
        base,
        changes,
        &ImportOptions::default(),
    )
    .await?;
    let last = built.last().unwrap();
    let copy_from = files(ctx, repo, &last.changeset).await?[0].2.clone();
    Ok((copy_from, last.dropped_copy_sources))
}

#[mononoke::fbinit_test]
async fn test_copy_source_must_be_a_file_in_the_unsaved_parent(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;

    let (copy_from, dropped) =
        copy_from_of_c(&ctx, &repo, None, &[7, 9, 11], "//test1/b.txt").await?;
    assert!(copy_from.is_some_and(|(path, _)| path == self::path("b.txt")));
    assert_eq!(0, dropped);
    for missing in ["//test1/gone.txt", "//test1/never.txt"] {
        let (copy_from, dropped) = copy_from_of_c(&ctx, &repo, None, &[7, 9, 11], missing).await?;
        assert_eq!((None, 1), (copy_from, dropped), "{missing}");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_copy_source_must_be_a_file_in_the_saved_parent(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = two_changelists();
    let imported = import_changelists(
        &ctx,
        &repo,
        &source,
        DEPOT,
        &master,
        &[7, 9],
        &ImportOptions::default(),
    )
    .await?
    .changelists;
    let base = Some(imported[1].cs_id);

    let (copy_from, dropped) = copy_from_of_c(&ctx, &repo, base, &[11], "//test1/a.txt").await?;
    assert_eq!(
        (Some((path("a.txt"), imported[1].cs_id)), 0),
        (copy_from, dropped)
    );
    let (copy_from, dropped) = copy_from_of_c(&ctx, &repo, base, &[11], "//test1/gone.txt").await?;
    assert_eq!((None, 1), (copy_from, dropped));
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_copy_with_several_sources_is_counted_as_ambiguous(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let mut source = branch_after_two_changelists("//test1/a.txt");
    let branch_from = |file: &str| Integration {
        how: IntegrationHow::BranchFrom,
        file: file.to_owned(),
        start_rev: None,
        end_rev: 1,
    };
    source.integrations.insert(
        "//test1/c.txt#1".to_owned(),
        vec![branch_from("//test1/a.txt"), branch_from("//test1/b.txt")],
    );

    let built = build_changesets(
        &ctx,
        &repo,
        &source,
        DEPOT,
        None,
        &[7, 9, 11],
        &ImportOptions::default(),
    )
    .await?;

    let last = built.last().unwrap();
    assert_eq!(None, files(&ctx, &repo, &last.changeset).await?[0].2);
    assert_eq!(
        (1, 0),
        (last.ambiguous_copy_sources, last.dropped_copy_sources)
    );
    assert_eq!(0, built[1].ambiguous_copy_sources);
    Ok(())
}

const DEPOT: &str = "//test1/";

async fn bookmark_target(
    ctx: &CoreContext,
    repo: &TestRepo,
    bookmark: &BookmarkKey,
) -> Result<Option<ChangesetId>> {
    repo.bookmarks()
        .get(ctx.clone(), bookmark, Freshness::MostRecent)
        .await
}

async fn mapped(ctx: &CoreContext, repo: &TestRepo, change: u64) -> Result<Option<ChangesetId>> {
    repo.bonsai_p4_mapping
        .get_bonsai_from_p4_changelist_id(ctx, P4ChangelistId::new(change))
        .await
}

#[mononoke::fbinit_test]
async fn test_import_moves_the_bookmark_and_maps_each_changelist(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let options = ImportOptions {
        derive: vec![DerivableType::Fsnodes],
        ..Default::default()
    };

    let imported = import_changelists(
        &ctx,
        &repo,
        &two_changelists(),
        DEPOT,
        &master,
        &[7, 9],
        &options,
    )
    .await?
    .changelists;

    let built = build_changesets(
        &ctx,
        &repo,
        &two_changelists(),
        DEPOT,
        None,
        &[7, 9],
        &ImportOptions::default(),
    )
    .await?;
    let expected: Vec<_> = built
        .iter()
        .map(|built| (built.change, built.changeset.get_changeset_id()))
        .collect();
    let ids: Vec<_> = imported.iter().map(|i| (i.change, i.cs_id)).collect();
    assert_eq!(expected, ids);
    assert_eq!(
        Some(imported[1].cs_id),
        bookmark_target(&ctx, &repo, &master).await?
    );
    assert_eq!(Some(9), resume_point(&ctx, &repo, &master).await?);
    for (change, cs_id) in ids {
        assert_eq!(Some(cs_id), mapped(&ctx, &repo, change).await?);
        assert_eq!(
            Some(P4ChangelistId::new(change)),
            repo.bonsai_p4_mapping
                .get_p4_changelist_id_from_bonsai(&ctx, cs_id)
                .await?
        );
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_next_batch_continues_from_the_bookmark(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = two_changelists();
    assert_eq!(None, resume_point(&ctx, &repo, &master).await?);

    let first = import_changelists(
        &ctx,
        &repo,
        &source,
        DEPOT,
        &master,
        &[7],
        &ImportOptions::default(),
    )
    .await?
    .changelists;
    let second = import_changelists(
        &ctx,
        &repo,
        &source,
        DEPOT,
        &master,
        &[9],
        &ImportOptions::default(),
    )
    .await?
    .changelists;

    let head = second[0].cs_id.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(vec![first[0].cs_id], head.parents().collect::<Vec<_>>());
    assert_eq!(
        Some(second[0].cs_id),
        bookmark_target(&ctx, &repo, &master).await?
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_batch_not_after_the_checkpoint_is_rejected(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = two_changelists();
    import_changelists(
        &ctx,
        &repo,
        &source,
        DEPOT,
        &master,
        &[7, 9],
        &ImportOptions::default(),
    )
    .await?;
    let head = bookmark_target(&ctx, &repo, &master).await?;

    for batch in [&[9, 11][..], &[8]] {
        let error = import_changelists(
            &ctx,
            &repo,
            &source,
            DEPOT,
            &master,
            batch,
            &ImportOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("already at changelist 9"),
            "{error:?}"
        );
    }
    let error = import_changelists(
        &ctx,
        &repo,
        &source,
        DEPOT,
        &master,
        &[11, 10],
        &ImportOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("not in increasing order"),
        "{error:?}"
    );
    assert_eq!(head, bookmark_target(&ctx, &repo, &master).await?);
    Ok(())
}

/// The mapping and the bookmark commit together: a mapping conflict leaves neither.
#[mononoke::fbinit_test]
async fn test_failed_mapping_write_leaves_the_bookmark(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let elsewhere = BonsaiP4MappingEntry::new(ONES_CSID, P4ChangelistId::new(9));
    repo.bonsai_p4_mapping
        .bulk_import(&ctx, &[elsewhere])
        .await?;

    let error = import_changelists(
        &ctx,
        &repo,
        &two_changelists(),
        DEPOT,
        &master,
        &[7, 9],
        &ImportOptions::default(),
    )
    .await
    .unwrap_err();

    assert!(
        matches!(
            error.downcast_ref(),
            Some(AddP4MappingErrorKind::Conflict {
                conflicting: 1,
                total: 2
            })
        ),
        "{error:?}"
    );
    assert_eq!(None, bookmark_target(&ctx, &repo, &master).await?);
    assert_eq!(None, mapped(&ctx, &repo, 7).await?);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_changelist_without_files_is_an_empty_commit(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = FakeSource::default().with_changelist(5, &[]);

    let imported = import_changelists(
        &ctx,
        &repo,
        &source,
        DEPOT,
        &master,
        &[5],
        &ImportOptions::default(),
    )
    .await?
    .changelists;

    let changeset = imported[0].cs_id.load(&ctx, repo.repo_blobstore()).await?;
    assert_eq!(0, changeset.file_changes().count());
    assert_eq!(Some(imported[0].cs_id), mapped(&ctx, &repo, 5).await?);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_import_next_continues_after_the_bookmark_and_reports_each_changelist(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let mut source = two_changelists()
        .with_changelist(
            11,
            &[
                ("//other/x.txt", 1, Action::Add, Some(b"x\n")),
                ("//test1/c.txt", 1, Action::Branch, Some(b"c\n")),
                ("//test1/d.txt", 1, Action::Branch, Some(b"d\n")),
                ("//test1/dir/f.txt", 1, Action::Add, Some(b"f\n")),
            ],
        )
        .with_changelist(
            13,
            &[
                ("//test1/dir", 1, Action::Add, Some(b"now a file\n")),
                ("//test1/dir/f.txt", 2, Action::Delete, None),
            ],
        )
        .with_branch("//test1/c.txt", "//test1/never.txt")
        .with_branch("//test1/d.txt", "//test1/a.txt");
    source
        .integrations
        .get_mut("//test1/d.txt#1")
        .unwrap()
        .push(Integration {
            how: IntegrationHow::BranchFrom,
            file: "//test1/b.txt".to_owned(),
            start_rev: None,
            end_rev: 1,
        });
    let report = |imported: &[ImportedChangelist]| -> Vec<_> {
        imported
            .iter()
            .map(|i| {
                (
                    i.change,
                    i.file_changes,
                    i.other_depot_files,
                    i.shadowed_deletions,
                    i.dropped_copy_sources,
                    i.ambiguous_copy_sources,
                )
            })
            .collect()
    };

    let first = import_next(
        &ctx,
        &repo,
        &source,
        DEPOT,
        &master,
        1,
        &ImportOptions::default(),
    )
    .await?;
    let rest = import_next(
        &ctx,
        &repo,
        &source,
        DEPOT,
        &master,
        5,
        &ImportOptions::default(),
    )
    .await?;
    let none = import_next(
        &ctx,
        &repo,
        &source,
        DEPOT,
        &master,
        5,
        &ImportOptions::default(),
    )
    .await?;

    assert_eq!(vec![(7, 2, 0, 0, 0, 0)], report(&first.changelists));
    assert_eq!(
        vec![(9, 3, 0, 0, 0, 0), (11, 3, 1, 0, 1, 1), (13, 1, 0, 1, 0, 0)],
        report(&rest.changelists)
    );
    assert_eq!(ImportedBatch::default(), none);
    assert_eq!(
        Some(rest.changelists[2].cs_id),
        bookmark_target(&ctx, &repo, &master).await?
    );
    Ok(())
}

/// Deriving a type the repo doesn't enable would fail the batch after it is saved.
#[mononoke::fbinit_test]
async fn test_types_the_repo_does_not_enable_are_not_derived(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = TestRepoFactory::new(fb)?
        .with_config_override(|config| {
            if let Some(active) = config.derived_data_config.get_active_config_mut() {
                active.types.remove(&DerivableType::Fsnodes);
            }
        })
        .build()
        .await?;
    let master = BookmarkKey::new("master")?;
    let options = ImportOptions {
        derive: vec![DerivableType::Fsnodes, DerivableType::Unodes],
        ..Default::default()
    };

    let imported = import_changelists(
        &ctx,
        &repo,
        &two_changelists(),
        DEPOT,
        &master,
        &[7, 9],
        &options,
    )
    .await?;

    let head = imported.changelists[1].cs_id;
    let unodes = repo
        .repo_derived_data()
        .fetch_derived::<RootUnodeManifestId>(&ctx, head)
        .await?;
    assert!(unodes.is_some());
    assert_eq!(Some(head), bookmark_target(&ctx, &repo, &master).await?);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_default_options_derive_content_manifests(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;

    let imported = import_changelists(
        &ctx,
        &repo,
        &two_changelists(),
        DEPOT,
        &master,
        &[7, 9],
        &ImportOptions::default(),
    )
    .await?;

    let head = imported.changelists[1].cs_id;
    let manifest = repo
        .repo_derived_data()
        .fetch_derived::<RootContentManifestId>(&ctx, head)
        .await?;
    assert!(manifest.is_some());
    Ok(())
}

fn with_changelists(numbers: &[u64]) -> FakeSource {
    numbers
        .iter()
        .fold(FakeSource::default(), |source, &number| {
            source.with_changelist(number, &[])
        })
}

#[mononoke::fbinit_test]
async fn test_next_changes_walk_windows_past_sparse_changelists(_fb: FacebookInit) -> Result<()> {
    let source = with_changelists(&[3, 25, 26, 61]);

    let changes = next_changes(&source, DEPOT, None, 10, 10).await?;

    assert_eq!(vec![3, 25, 26, 61], changes);
    assert_eq!(
        vec![
            (0, 10),
            (10, 20),
            (20, 30),
            (30, 40),
            (40, 50),
            (50, 60),
            (60, 61)
        ],
        *source.listed.lock().unwrap()
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_next_changes_stop_at_the_window_that_reaches_the_limit(
    _fb: FacebookInit,
) -> Result<()> {
    let source = with_changelists(&[3, 4, 5, 12, 25]);

    let changes = next_changes(&source, DEPOT, Some(3), 2, 10).await?;

    assert_eq!(vec![4, 5], changes);
    assert_eq!(vec![(3, 13)], *source.listed.lock().unwrap());
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_next_changes_stop_at_the_latest_changelist(_fb: FacebookInit) -> Result<()> {
    let source = FakeSource {
        latest: Some(30),
        ..with_changelists(&[3, 25, 26])
    };

    let changes = next_changes(&source, DEPOT, Some(26), 10, 10).await?;
    let none_after_latest = next_changes(&source, DEPOT, Some(30), 10, 10).await?;

    assert_eq!(Vec::<u64>::new(), changes);
    assert_eq!(Vec::<u64>::new(), none_after_latest);
    assert_eq!(vec![(26, 30)], *source.listed.lock().unwrap());
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_each_changelist_reports_how_many_files_it_uploaded(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;

    let batch = import_changelists(
        &ctx,
        &repo,
        &two_changelists(),
        DEPOT,
        &master,
        &[7, 9],
        &ImportOptions::default(),
    )
    .await?;

    let uploaded: Vec<_> = batch
        .changelists
        .iter()
        .map(|i| (i.change, i.timings.uploaded_files))
        .collect();
    // 9's deletion has no content to upload.
    assert_eq!(vec![(7, 2), (9, 2)], uploaded);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_import_next_lists_in_the_configured_window(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = two_changelists();
    let options = ImportOptions {
        changes_window: 4,
        ..Default::default()
    };

    let batch = import_next(&ctx, &repo, &source, DEPOT, &master, 5, &options).await?;

    assert_eq!(2, batch.changelists.len());
    assert_eq!(vec![(0, 4), (4, 8), (8, 9)], *source.listed.lock().unwrap());
    Ok(())
}

/// `two_changelists`, then 11 branching `count` files from `a.txt`.
fn branches_of_a(count: usize) -> FakeSource {
    let targets: Vec<&'static str> = (0..count)
        .map(|i| &*format!("//test1/copy{i}.txt").leak())
        .collect();
    let files: Vec<FileSpec> = targets
        .iter()
        .map(|&target| (target, 1, Action::Branch, Some(&b"one\n"[..])))
        .collect();
    targets.iter().fold(
        two_changelists().with_changelist(11, &files),
        |source, target| source.with_branch(target, "//test1/a.txt"),
    )
}

#[mononoke::fbinit_test]
async fn test_files_are_fetched_at_most_file_concurrency_at_once(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let build = async |source: &FakeSource, file_concurrency| -> Result<Vec<_>> {
        // One changelist at a time: the bound is per changelist.
        let options = ImportOptions {
            file_concurrency,
            changelist_lookahead: 1,
            ..Default::default()
        };
        let built =
            build_changesets(&ctx, &repo, source, DEPOT, None, &[7, 9, 11], &options).await?;
        Ok(built.iter().map(|b| b.changeset.clone()).collect())
    };
    let (sequential, concurrent) = (branches_of_a(10), branches_of_a(10));

    let one_at_a_time = build(&sequential, 1).await?;
    let three_at_a_time = build(&concurrent, 3).await?;

    assert_eq!((1, 1), (sequential.prints.max(), sequential.filelogs.max()));
    // One batched `integrations` call per changelist.
    assert_eq!((3, 1), (concurrent.prints.max(), concurrent.filelogs.max()));
    assert_eq!(one_at_a_time, three_at_a_time);
    assert_eq!(10, three_at_a_time[2].file_changes().count());
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_zero_file_concurrency_still_builds(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let source = FakeSource::default()
        .with_changelist(3, &[])
        .with_changelist(5, &[("//test1/a.txt", 1, Action::Add, Some(b"one\n"))]);
    let options = ImportOptions {
        file_concurrency: 0,
        ..Default::default()
    };

    let built = build_changesets(&ctx, &repo, &source, DEPOT, None, &[3, 5], &options).await?;

    let changed: Vec<_> = built
        .iter()
        .map(|b| b.changeset.file_changes().count())
        .collect();
    assert_eq!(vec![0, 1], changed);
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_changelists_are_fetched_ahead_at_most_changelist_lookahead_at_once(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let build = async |source: &FakeSource, changelist_lookahead| -> Result<Vec<_>> {
        let options = ImportOptions {
            changelist_lookahead,
            ..Default::default()
        };
        let built =
            build_changesets(&ctx, &repo, source, DEPOT, None, &[7, 9, 11], &options).await?;
        Ok(built.iter().map(|b| b.changeset.clone()).collect())
    };
    let (sequential, ahead) = (branches_of_a(4), branches_of_a(4));

    let one_at_a_time = build(&sequential, 1).await?;
    let two_at_a_time = build(&ahead, 2).await?;

    assert_eq!(1, sequential.describes.max());
    assert_eq!(2, ahead.describes.max());
    assert_eq!(one_at_a_time, two_at_a_time);
    let parents: Vec<_> = two_at_a_time
        .iter()
        .map(|c| c.parents().collect::<Vec<_>>())
        .collect();
    let ids: Vec<_> = two_at_a_time.iter().map(|c| c.get_changeset_id()).collect();
    assert_eq!(vec![vec![], vec![ids[0]], vec![ids[1]]], parents);
    Ok(())
}
