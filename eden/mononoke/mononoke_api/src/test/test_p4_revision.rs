/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use context::CoreContext;
use fbinit::FacebookInit;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::NonRootMPath;
use mononoke_types::P4_FILE_REVS_EXTRA;
use mononoke_types::P4ChangelistId;
use mononoke_types::P4FileRevs;
use mononoke_types::path::MPath;
use tests_utils::CreateCommitContext;

use crate::MononokeError;
use crate::P4LastChange;
use crate::P4PathRevision;
use crate::RepoContext;
use crate::repo::Repo;

/// The extras the importer writes for changelist `changelist`, whose files got the
/// revisions `revs` (in path order, as the importer sorts them).
fn p4_extras(changelist: u64, revs: &[(&str, u32)]) -> Vec<(String, Vec<u8>)> {
    let file_revs: P4FileRevs = revs
        .iter()
        .map(|(path, rev)| (NonRootMPath::new(path).expect("valid path"), *rev))
        .collect();
    P4ChangelistId::new(changelist)
        .to_hg_extras()
        .into_iter()
        .chain([(
            P4_FILE_REVS_EXTRA.to_string(),
            file_revs.as_bytes().to_vec(),
        )])
        .collect()
}

fn with_extras<'a>(
    mut commit: CreateCommitContext<'a, Repo>,
    extras: Vec<(String, Vec<u8>)>,
) -> CreateCommitContext<'a, Repo> {
    for (key, value) in extras {
        commit = commit.add_extra(key, value);
    }
    commit
}

/// A mirror's linear history. `legacy` predates the importer recording anything.
///
/// ```text
/// legacy      : + legacy.txt                        (no Perforce extras)
/// c1 (CL 100) : + a.txt#1, + dir/b.txt#1
/// c2 (CL 101) :   dir/b.txt#2
/// c3 (CL 102) : - a.txt#2
/// c4 (CL 103) : + a.txt#3 (re-added), + c.txt#1 (branched from dir/b.txt)
/// c5 (CL 104) :   dir/b.txt#3
/// broken      :   a.txt#4 edited, but the extra lists another file
/// ```
struct Mirror {
    repo: RepoContext<Repo>,
    legacy: ChangesetId,
    c1: ChangesetId,
    c2: ChangesetId,
    c3: ChangesetId,
    c4: ChangesetId,
    c5: ChangesetId,
    broken: ChangesetId,
}

async fn mirror(ctx: &CoreContext) -> Result<Mirror> {
    let repo: Repo = test_repo_factory::build_empty(ctx.fb).await?;
    let legacy = CreateCommitContext::new_root(ctx, &repo)
        .add_file("legacy.txt", "legacy")
        .commit()
        .await?;
    let c1 = with_extras(
        CreateCommitContext::new(ctx, &repo, vec![legacy])
            .add_file("a.txt", "a1")
            .add_file("dir/b.txt", "b1"),
        p4_extras(100, &[("a.txt", 1), ("dir/b.txt", 1)]),
    )
    .commit()
    .await?;
    let c2 = with_extras(
        CreateCommitContext::new(ctx, &repo, vec![c1]).add_file("dir/b.txt", "b2"),
        p4_extras(101, &[("dir/b.txt", 2)]),
    )
    .commit()
    .await?;
    let c3 = with_extras(
        CreateCommitContext::new(ctx, &repo, vec![c2]).delete_file("a.txt"),
        p4_extras(102, &[("a.txt", 2)]),
    )
    .commit()
    .await?;
    let c4 = with_extras(
        CreateCommitContext::new(ctx, &repo, vec![c3])
            .add_file("a.txt", "a3")
            .add_file_with_copy_info("c.txt", "b2", (c3, "dir/b.txt")),
        p4_extras(103, &[("a.txt", 3), ("c.txt", 1)]),
    )
    .commit()
    .await?;
    let c5 = with_extras(
        CreateCommitContext::new(ctx, &repo, vec![c4]).add_file("dir/b.txt", "b3"),
        p4_extras(104, &[("dir/b.txt", 3)]),
    )
    .commit()
    .await?;
    let broken = with_extras(
        CreateCommitContext::new(ctx, &repo, vec![c5]).add_file("a.txt", "a4"),
        p4_extras(105, &[("other.txt", 4)]),
    )
    .commit()
    .await?;
    let repo = RepoContext::new_test(ctx.clone(), Arc::new(repo)).await?;
    Ok(Mirror {
        repo,
        legacy,
        c1,
        c2,
        c3,
        c4,
        c5,
        broken,
    })
}

impl Mirror {
    /// What `commit_path_last_changed` does: the last change, then its revision.
    async fn p4_revision(
        &self,
        at: ChangesetId,
        path: &str,
    ) -> Result<Option<P4PathRevision<Repo>>, MononokeError> {
        let path = self
            .repo
            .changeset(at)
            .await?
            .expect("commit exists")
            .path_with_history(path)
            .await?;
        let (last_change, exists) = match path.last_modified().await? {
            Some(last_modified) => (last_modified, true),
            None => match path.last_deleted().await? {
                Some(last_deleted) => (last_deleted, false),
                None => return Ok(None),
            },
        };
        path.p4_revision(&last_change, exists).await
    }

    /// What `commit_multiple_path_last_changed` does for `paths` at `at`.
    async fn p4_path_revisions(
        &self,
        at: ChangesetId,
        paths: &[&str],
    ) -> Result<HashMap<MPath, P4PathRevision<Repo>>, MononokeError> {
        let changeset = self.repo.changeset(at).await?.expect("commit exists");
        let mut last_changes = Vec::new();
        for path in paths {
            let context = changeset.path_with_history(*path).await?;
            let (last_change, exists) = match context.last_modified().await? {
                Some(last_modified) => (last_modified, true),
                None => match context.last_deleted().await? {
                    Some(last_deleted) => (last_deleted, false),
                    None => continue,
                },
            };
            last_changes.push(P4LastChange {
                path: context.path().clone(),
                is_file: exists && context.is_file().await?,
                changeset: last_change,
                exists,
            });
        }
        self.repo.p4_path_revisions(last_changes).await
    }

    /// `(changeset, changelist, revision, exists)` of the path's revision at `at`.
    async fn revision(&self, at: ChangesetId, path: &str) -> Result<(ChangesetId, u64, u32, bool)> {
        let found = self
            .p4_revision(at, path)
            .await?
            .expect("path has a revision");
        assert_eq!(found.path, NonRootMPath::new(path)?, "path as recorded");
        Ok((
            found.changeset.id(),
            found.changelist.id(),
            found.revision,
            found.exists,
        ))
    }
}

#[mononoke::fbinit_test]
async fn test_p4_revision_is_the_last_change_at_or_before(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let m = mirror(&ctx).await?;

    // At the commit that changed the path.
    assert_eq!(m.revision(m.c1, "a.txt").await?, (m.c1, 100, 1, true));
    assert_eq!(m.revision(m.c1, "dir/b.txt").await?, (m.c1, 100, 1, true));
    // At a later commit that touched another path: unchanged, so the same answer.
    assert_eq!(m.revision(m.c2, "a.txt").await?, (m.c1, 100, 1, true));
    // An edit is a new revision.
    assert_eq!(m.revision(m.c2, "dir/b.txt").await?, (m.c2, 101, 2, true));
    assert_eq!(m.revision(m.c5, "dir/b.txt").await?, (m.c5, 104, 3, true));
    // A deletion is a revision the path no longer exists at.
    assert_eq!(m.revision(m.c3, "a.txt").await?, (m.c3, 102, 2, false));
    // Re-adding continues the numbering.
    assert_eq!(m.revision(m.c4, "a.txt").await?, (m.c4, 103, 3, true));
    assert_eq!(m.revision(m.c5, "a.txt").await?, (m.c4, 103, 3, true));
    // A branched file starts at #1 at its new path, from the branching commit.
    assert_eq!(m.revision(m.c4, "c.txt").await?, (m.c4, 103, 1, true));
    assert_eq!(m.revision(m.c5, "c.txt").await?, (m.c4, 103, 1, true));
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_p4_revision_has_nothing_to_say_about(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let m = mirror(&ctx).await?;

    // Nothing ever existed there.
    assert!(m.p4_revision(m.c5, "nonexistent.txt").await?.is_none());
    // Before the path was added.
    assert!(m.p4_revision(m.legacy, "a.txt").await?.is_none());
    // Directories have no Perforce revision.
    assert!(m.p4_revision(m.c5, "dir").await?.is_none());
    assert!(m.p4_revision(m.c5, "").await?.is_none());
    // Last changed by a commit that records no revisions.
    assert!(m.p4_revision(m.c5, "legacy.txt").await?.is_none());
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_p4_revision_rejects_a_file_the_importer_did_not_record(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let m = mirror(&ctx).await?;
    let error = match m.p4_revision(m.broken, "a.txt").await {
        Err(error) => error,
        Ok(_) => panic!("a changed file missing from the extra is an importer bug"),
    };
    assert!(
        matches!(error, MononokeError::InternalError(_)),
        "{error:?}"
    );
    assert!(error.to_string().contains("a.txt"), "{error}");
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_p4_path_revisions_serve_several_paths_at_once(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let m = mirror(&ctx).await?;
    // At c3: a.txt and c.txt are not there yet / deleted, dir/b.txt and dir share c2.
    let found = m
        .p4_path_revisions(
            m.c3,
            &[
                "a.txt",
                "dir/b.txt",
                "dir",
                "legacy.txt",
                "nonexistent.txt",
                "",
            ],
        )
        .await?;
    let mut paths = found
        .keys()
        .map(|path| path.to_string())
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(
        paths,
        ["a.txt", "dir/b.txt"],
        "a directory, the root, a never-existed path and a pre-extra file have none"
    );
    let a = &found[&MPath::try_from("a.txt")?];
    assert_eq!((a.changeset.id(), a.revision, a.exists), (m.c3, 2, false));
    let b = &found[&MPath::try_from("dir/b.txt")?];
    assert_eq!((b.changeset.id(), b.revision, b.exists), (m.c2, 2, true));

    // At c1: two files sharing the one last-changed commit.
    let found = m.p4_path_revisions(m.c1, &["a.txt", "dir/b.txt"]).await?;
    assert_eq!(found.len(), 2);
    assert!(
        found
            .values()
            .all(|p4| p4.changeset.id() == m.c1 && p4.revision == 1)
    );

    // A changed file its commit did not record is an error here too.
    let error = match m.p4_path_revisions(m.broken, &["a.txt", "dir/b.txt"]).await {
        Err(error) => error,
        Ok(_) => panic!("a changed file missing from the extra is an importer bug"),
    };
    assert!(
        matches!(error, MononokeError::InternalError(_)),
        "{error:?}"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_p4_file_revisions_serve_several_paths(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let m = mirror(&ctx).await?;
    let c1 = m.repo.changeset(m.c1).await?.expect("commit exists");
    let revisions = c1.p4_file_revisions().await?.expect("c1 records revisions");
    assert_eq!(revisions.changelist(), P4ChangelistId::new(100));
    assert_eq!(revisions.revision(&NonRootMPath::new("a.txt")?)?, Some(1));
    assert_eq!(
        revisions.revision(&NonRootMPath::new("dir/b.txt")?)?,
        Some(1)
    );
    assert_eq!(revisions.revision(&NonRootMPath::new("dir")?)?, None);
    assert!(
        revisions
            .file_revision(&NonRootMPath::new("other.txt")?)
            .is_err()
    );
    let legacy = m.repo.changeset(m.legacy).await?.expect("commit exists");
    assert!(legacy.p4_file_revisions().await?.is_none());
    Ok(())
}
