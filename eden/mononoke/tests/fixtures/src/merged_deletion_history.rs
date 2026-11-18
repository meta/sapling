/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use tests_utils::CreateCommitContext;

use super::*;

/// Independent deletions, merge resolutions, directory replacements, and recreation.
pub struct MergedDeletionHistory;

#[async_trait]
impl TestRepoFixture for MergedDeletionHistory {
    const REPO_NAME: &'static str = "merged_deletion_history";

    async fn init_repo(
        fb: FacebookInit,
        repo: &impl Repo,
    ) -> Result<(
        BTreeMap<String, ChangesetId>,
        BTreeMap<String, BTreeSet<String>>,
    )> {
        let ctx = CoreContext::test_mock(fb);
        let a = CreateCommitContext::new_root(&ctx, repo)
            .add_file("file", "1")
            .add_file("dir/file", "2")
            .add_file("dir_2/file", "3")
            .add_file("dir_3/file_1", "1")
            .add_file("dir_3/file_2", "2")
            .set_message("A")
            .commit()
            .await?;
        let b = CreateCommitContext::new(&ctx, repo, vec![a])
            .delete_file("file")
            .delete_file("dir/file")
            .delete_file("dir_3/file_1")
            .add_file("dir/file_2", "file->file_2")
            .set_message("B")
            .commit()
            .await?;
        let c = CreateCommitContext::new(&ctx, repo, vec![a])
            .add_file("file", "1->2")
            .set_message("C")
            .commit()
            .await?;
        let d = CreateCommitContext::new(&ctx, repo, vec![c])
            .delete_file("dir/file")
            .delete_file("dir_2/file")
            .set_message("D")
            .commit()
            .await?;
        let e = CreateCommitContext::new_root(&ctx, repo)
            .add_file("file", "3")
            .add_file("dir_2/file", "4")
            .set_message("E")
            .commit()
            .await?;
        let f = CreateCommitContext::new(&ctx, repo, vec![e])
            .delete_file("file")
            .add_file("dir_2/file", "4->5")
            .set_message("F")
            .commit()
            .await?;
        let g = CreateCommitContext::new(&ctx, repo, vec![d, f])
            .delete_file("file")
            .add_file("dir_2/file", "4->5")
            .add_file("dir_2/file_2", "5")
            .set_message("G")
            .commit()
            .await?;
        let h = CreateCommitContext::new(&ctx, repo, vec![g])
            .delete_file("dir_3/file_2")
            .add_file("dir_2/file", "4->5")
            .add_file("dir_2/file_2", "5")
            .set_message("H")
            .commit()
            .await?;
        let i = CreateCommitContext::new(&ctx, repo, vec![b, h])
            .delete_file("dir_3/file_1")
            .delete_file("dir_3/file_2")
            .add_file("dir_2/file", "4->5")
            .add_file("dir_5/file_1", "5.1")
            .add_file("dir_5/file_2", "5.2")
            .set_message("I")
            .commit()
            .await?;
        let j = CreateCommitContext::new(&ctx, repo, vec![i])
            .delete_file("dir_5/file_1")
            .add_file("dir_4/file_1", "new")
            .set_message("J")
            .commit()
            .await?;
        let k = CreateCommitContext::new(&ctx, repo, vec![j])
            .delete_file("dir_4/file_1")
            .add_file("dir_to_file/file", "will be replaced")
            .add_file("dir_to_file/sub/deep/file", "will also be replaced")
            .set_message("K")
            .commit()
            .await?;
        let l = CreateCommitContext::new(&ctx, repo, vec![i])
            .delete_file("dir_5/file_2")
            .add_file("dir_4/file_2", "new")
            .set_message("L")
            .commit()
            .await?;
        let m = CreateCommitContext::new(&ctx, repo, vec![l])
            .delete_file("dir_4/file_2")
            .set_message("M")
            .commit()
            .await?;
        let n = CreateCommitContext::new(&ctx, repo, vec![k, m])
            .delete_file("dir_5/file_1")
            .delete_file("dir_5/file_2")
            .add_file("dir_to_file", "replaced!")
            .set_message("N")
            .commit()
            .await?;
        let o = CreateCommitContext::new(&ctx, repo, vec![n])
            .set_message("O")
            .delete_file("dir_to_file")
            .add_file("dir/file", "restored")
            .add_file("dir_to_file/sub/deep/file", "restored")
            .commit()
            .await?;
        let p = CreateCommitContext::new(&ctx, repo, vec![o])
            .set_message("P")
            .delete_file("dir/file")
            .delete_file("dir_to_file/sub/deep/file")
            .commit()
            .await?;
        let mut txn = repo.bookmarks().create_transaction(ctx.clone());
        txn.force_set(
            &BookmarkKey::new("master")?,
            p,
            BookmarkUpdateReason::TestMove,
        )?;
        txn.commit().await?;

        let history = [
            ("A", a, vec![]),
            ("B", b, vec!["A"]),
            ("C", c, vec!["A"]),
            ("D", d, vec!["C"]),
            ("E", e, vec![]),
            ("F", f, vec!["E"]),
            ("G", g, vec!["D", "F"]),
            ("H", h, vec!["G"]),
            ("I", i, vec!["B", "H"]),
            ("J", j, vec!["I"]),
            ("K", k, vec!["J"]),
            ("L", l, vec!["I"]),
            ("M", m, vec!["L"]),
            ("N", n, vec!["K", "M"]),
            ("O", o, vec!["N"]),
            ("P", p, vec!["O"]),
        ];
        let mut commits = BTreeMap::from([("master".to_owned(), p)]);
        let mut dag = BTreeMap::new();
        for (name, id, parents) in history {
            commits.insert(name.to_owned(), id);
            dag.insert(
                name.to_owned(),
                parents.into_iter().map(str::to_owned).collect(),
            );
        }
        Ok((commits, dag))
    }
}

impl MergedDeletionHistory {
    /// Complete deleted-manifest nodes: `None` is a live path retaining child history.
    pub const DELETED_MANIFESTS: &[(&str, &[(&str, Option<&str>)])] = &[
        (
            "B",
            &[
                ("", None),
                ("dir", None),
                ("dir/file", Some("B")),
                ("dir_3", None),
                ("dir_3/file_1", Some("B")),
                ("file", Some("B")),
            ],
        ),
        (
            "D",
            &[
                ("", None),
                ("dir", Some("D")),
                ("dir/file", Some("D")),
                ("dir_2", Some("D")),
                ("dir_2/file", Some("D")),
            ],
        ),
        (
            "G",
            &[
                ("", None),
                ("dir", Some("D")),
                ("dir/file", Some("D")),
                ("file", Some("G")),
            ],
        ),
        (
            "H",
            &[
                ("", None),
                ("dir", Some("D")),
                ("dir/file", Some("D")),
                ("dir_3", None),
                ("dir_3/file_2", Some("H")),
                ("file", Some("G")),
            ],
        ),
        (
            "I",
            &[
                ("", None),
                ("dir", None),
                ("dir/file", Some("I")),
                ("dir_3", Some("I")),
                ("dir_3/file_1", Some("I")),
                ("dir_3/file_2", Some("I")),
                ("file", Some("I")),
            ],
        ),
        (
            "N",
            &[
                ("", None),
                ("dir", None),
                ("dir/file", Some("I")),
                ("dir_3", Some("I")),
                ("dir_3/file_1", Some("I")),
                ("dir_3/file_2", Some("I")),
                ("dir_4", Some("N")),
                ("dir_4/file_1", Some("K")),
                ("dir_4/file_2", Some("M")),
                ("dir_5", Some("N")),
                ("dir_5/file_1", Some("N")),
                ("dir_5/file_2", Some("N")),
                ("dir_to_file", None),
                ("dir_to_file/file", Some("N")),
                ("dir_to_file/sub", Some("N")),
                ("dir_to_file/sub/deep", Some("N")),
                ("dir_to_file/sub/deep/file", Some("N")),
                ("file", Some("I")),
            ],
        ),
        (
            "O",
            &[
                ("", None),
                ("dir_3", Some("I")),
                ("dir_3/file_1", Some("I")),
                ("dir_3/file_2", Some("I")),
                ("dir_4", Some("N")),
                ("dir_4/file_1", Some("K")),
                ("dir_4/file_2", Some("M")),
                ("dir_5", Some("N")),
                ("dir_5/file_1", Some("N")),
                ("dir_5/file_2", Some("N")),
                ("dir_to_file", None),
                ("dir_to_file/file", Some("N")),
                ("file", Some("I")),
            ],
        ),
        (
            "P",
            &[
                ("", None),
                ("dir", None),
                ("dir/file", Some("P")),
                ("dir_3", Some("I")),
                ("dir_3/file_1", Some("I")),
                ("dir_3/file_2", Some("I")),
                ("dir_4", Some("N")),
                ("dir_4/file_1", Some("K")),
                ("dir_4/file_2", Some("M")),
                ("dir_5", Some("N")),
                ("dir_5/file_1", Some("N")),
                ("dir_5/file_2", Some("N")),
                ("dir_to_file", Some("P")),
                ("dir_to_file/file", Some("N")),
                ("dir_to_file/sub", Some("P")),
                ("dir_to_file/sub/deep", Some("P")),
                ("dir_to_file/sub/deep/file", Some("P")),
                ("file", Some("I")),
            ],
        ),
    ];
}
