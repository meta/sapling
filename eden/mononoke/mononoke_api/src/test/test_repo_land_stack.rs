/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use anyhow::Result;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::BookmarkUpdateReason;
use bookmarks::BookmarksRef;
use bookmarks::Freshness;
use bookmarks_movement::BookmarkKindRestrictions::AnyKind;
use bookmarks_movement::PushrebaseOutcome;
use bytes::Bytes;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::FutureExt;
use futures::stream::TryStreamExt;
use hooks::PushAuthoredBy::User;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use maplit::hashset;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use tests_utils::CreateCommitContext;
use tests_utils::drawdag::create_from_dag;

use crate::errors::MononokeError;
use crate::repo::BookmarkFreshness;
use crate::repo::Repo;
use crate::repo::RepoContext;

async fn init_repo(
    ctx: &CoreContext,
) -> Result<(RepoContext<Repo>, BTreeMap<String, ChangesetId>)> {
    let repo: Repo = test_repo_factory::build_empty(ctx.fb).await?;
    let changesets = create_from_dag(
        ctx,
        &repo,
        r"
            A-B-C-G
             \ \
              \ F
               D-E
        ",
    )
    .await?;
    let mut txn = repo.bookmarks().create_transaction(ctx.clone());
    txn.force_set(
        &BookmarkKey::new("trunk")?,
        changesets["C"],
        BookmarkUpdateReason::TestMove,
    )?;
    txn.commit().await?;

    let repo_ctx = RepoContext::new_test(ctx.clone(), Arc::new(repo)).await?;
    Ok((repo_ctx, changesets))
}

#[mononoke::fbinit_test]
async fn land_stack(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, changesets) = init_repo(&ctx).await?;
    let force_local_pushrebase = false;

    // Land G - it should be rewritten even though its parent is C.
    let outcome = repo
        .land_stack(
            "trunk",
            changesets["G"],
            changesets["C"],
            None,
            AnyKind,
            User,
            force_local_pushrebase,
        )
        .await?;
    let key = BookmarkKey::new("trunk")?;
    let trunk_g = repo
        .resolve_bookmark(&key, BookmarkFreshness::MostRecent)
        .await?
        .expect("trunk should be set");
    assert_eq!(trunk_g.id(), outcome.head);
    assert_ne!(trunk_g.id(), changesets["G"]);
    assert_eq!(outcome.rebased_changesets[0].id_old, changesets["G"]);
    assert_eq!(outcome.rebased_changesets[0].id_new, trunk_g.id());

    // Land D and E, both commits should get mapped
    let outcome = repo
        .land_stack(
            "trunk",
            changesets["E"],
            changesets["A"],
            None,
            AnyKind,
            User,
            force_local_pushrebase,
        )
        .await?;
    let trunk_e = repo
        .resolve_bookmark(&key, BookmarkFreshness::MostRecent)
        .await?
        .expect("trunk should be set");
    assert_eq!(trunk_e.id(), outcome.head);
    let mapping: HashMap<_, _> = outcome
        .rebased_changesets
        .iter()
        .map(|pair| (pair.id_old, pair.id_new))
        .collect();
    assert_eq!(mapping.len(), 2);
    assert_ne!(mapping[&changesets["D"]], changesets["D"]);
    assert_ne!(mapping[&changesets["E"]], changesets["D"]);
    assert_ne!(mapping[&changesets["D"]], trunk_e.id());
    assert_eq!(mapping[&changesets["E"]], trunk_e.id());

    // Land F, its parent should be the landed version of E
    let outcome = repo
        .land_stack(
            "trunk",
            changesets["F"],
            changesets["B"],
            None,
            AnyKind,
            User,
            force_local_pushrebase,
        )
        .await?;
    let trunk_f = repo
        .resolve_bookmark(&key, BookmarkFreshness::MostRecent)
        .await?
        .expect("trunk should be set");
    assert_eq!(trunk_f.id(), outcome.head);
    assert_eq!(trunk_f.parents().await?, vec![trunk_e.id()]);

    // With everything landed, all files should be present
    let files: HashSet<_> = trunk_f
        .path_with_content("")
        .await?
        .tree()
        .await?
        .expect("root must be a tree")
        .list()
        .await?
        .into_iter()
        .map(|(name, _entry)| name)
        .collect();
    let expected_files: HashSet<_> = hashset! {"A", "B", "C", "D", "E", "F", "G"}
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(files, expected_files);

    // Check the bookmark moves created BookmarkLogUpdate entries
    let entries = repo
        .repo()
        .bookmark_update_log()
        .list_bookmark_log_entries(
            ctx.clone(),
            BookmarkKey::new("trunk")?,
            4,
            None,
            Freshness::MostRecent,
        )
        .map_ok(|(_id, cs, rs, _ts)| (cs, rs))
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(
        entries,
        vec![
            (Some(trunk_f.id()), BookmarkUpdateReason::Pushrebase),
            (Some(trunk_e.id()), BookmarkUpdateReason::Pushrebase),
            (Some(trunk_g.id()), BookmarkUpdateReason::Pushrebase),
            (Some(changesets["C"]), BookmarkUpdateReason::TestMove),
        ]
    );

    Ok(())
}

async fn new_commit(
    repo: &RepoContext<Repo>,
    parents: Vec<ChangesetId>,
    file: &str,
) -> Result<ChangesetId> {
    CreateCommitContext::new(repo.ctx(), repo.repo(), parents)
        .add_file(file, file)
        .commit()
        .await
}

async fn set_bookmark(repo: &RepoContext<Repo>, name: &str, to: ChangesetId) -> Result<()> {
    let mut txn = repo
        .repo()
        .bookmarks()
        .create_transaction(repo.ctx().clone());
    txn.force_set(&BookmarkKey::new(name)?, to, BookmarkUpdateReason::TestMove)?;
    txn.commit().await?;
    Ok(())
}

/// Trunk is A-B-C-D, with `trunk` at D. The public release branch R1-R2
/// starts at B, with `release` at R2. The draft feature branch F1-F2 starts
/// at A.
async fn init_branches_repo(
    ctx: &CoreContext,
) -> Result<(RepoContext<Repo>, BTreeMap<String, ChangesetId>)> {
    let repo: Repo = test_repo_factory::build_empty(ctx.fb).await?;
    let mut changesets = create_from_dag(ctx, &repo, "A-B-C-D").await?;
    let repo = RepoContext::new_test(ctx.clone(), Arc::new(repo)).await?;
    for (name, parent) in [("R1", "B"), ("R2", "R1"), ("F1", "A"), ("F2", "F1")] {
        let id = new_commit(&repo, vec![changesets[parent]], name).await?;
        changesets.insert(name.to_string(), id);
    }
    set_bookmark(&repo, "trunk", changesets["D"]).await?;
    set_bookmark(&repo, "release", changesets["R2"]).await?;
    Ok((repo, changesets))
}

async fn land(
    repo: &RepoContext<Repo>,
    head: ChangesetId,
    base: ChangesetId,
) -> Result<PushrebaseOutcome, MononokeError> {
    repo.land_stack("trunk", head, base, None, AnyKind, User, false)
        .await
}

async fn trunk(repo: &RepoContext<Repo>) -> Result<ChangesetId> {
    Ok(repo
        .resolve_bookmark(&BookmarkKey::new("trunk")?, BookmarkFreshness::MostRecent)
        .await?
        .expect("trunk should be set")
        .id())
}

async fn parents(repo: &RepoContext<Repo>, cs_id: ChangesetId) -> Result<Vec<ChangesetId>> {
    Ok(repo
        .changeset(cs_id)
        .await?
        .expect("changeset should exist")
        .parents()
        .await?)
}

/// The Mercurial id of `cs_id`, deriving it if needed. Messages name commits
/// by Mercurial id when they all have one, and the warm bookmarks cache
/// derives those for bookmarked commits and their ancestors in the
/// background, so a test expecting a message that names only public commits
/// derives their ids first.
async fn hg_id(repo: &RepoContext<Repo>, cs_id: ChangesetId) -> Result<String> {
    Ok(repo
        .changeset(cs_id)
        .await?
        .expect("changeset should exist")
        .hg_id()
        .await?
        .expect("hg changesets should be derivable")
        .to_string())
}

fn rebased(outcome: &PushrebaseOutcome) -> HashMap<ChangesetId, ChangesetId> {
    outcome
        .rebased_changesets
        .iter()
        .map(|pair| (pair.id_old, pair.id_new))
        .collect()
}

#[track_caller]
fn assert_invalid_request<T: std::fmt::Debug>(result: Result<T, MononokeError>, expected: &str) {
    match result {
        Err(MononokeError::InvalidRequest(message)) => assert!(
            message.contains(expected),
            "expected {expected:?} in: {message}"
        ),
        other => panic!("expected an invalid request containing {expected:?}, got {other:?}"),
    }
}

#[mononoke::fbinit_test]
async fn land_stack_with_bookmark_ahead(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    // The client saw trunk at C; the merge base of D and S is still C.
    let s = new_commit(&repo, vec![cs["C"]], "S").await?;

    let outcome = land(&repo, s, cs["C"]).await?;

    let mapping = rebased(&outcome);
    assert_eq!(mapping.keys().copied().collect::<Vec<_>>(), vec![s]);
    assert_eq!(trunk(&repo).await?, mapping[&s]);
    assert_eq!(parents(&repo, mapping[&s]).await?, vec![cs["D"]]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_rejects_bookmark_moved_back_or_sideways(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    let s = new_commit(&repo, vec![cs["D"]], "S").await?;

    // Trunk moved back from D to C: the merge base is C, not D.
    set_bookmark(&repo, "trunk", cs["C"]).await?;
    assert_invalid_request(
        land(&repo, s, cs["D"]).await,
        &format!(
            "bookmark 'trunk' has moved incompatibly with this push: the merge base of 'trunk' and {s} is {}, but the push is based on {}. Pull and retry",
            cs["C"], cs["D"],
        ),
    );

    // Trunk moved sideways to the release branch: the merge base is B.
    set_bookmark(&repo, "trunk", cs["R2"]).await?;
    assert_invalid_request(
        land(&repo, s, cs["D"]).await,
        &format!("the merge base of 'trunk' and {s} is {}", cs["B"]),
    );
    assert_eq!(trunk(&repo).await?, cs["R2"]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_rejects_base_that_is_not_the_merge_base(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    // A stack on the release branch, based on the release head rather than
    // on the merge base with trunk.
    let s = new_commit(&repo, vec![cs["R2"]], "S").await?;

    assert_invalid_request(
        land(&repo, s, cs["R2"]).await,
        &format!(
            "the merge base of 'trunk' and {s} is {}, but the push is based on {}",
            cs["B"], cs["R2"],
        ),
    );
    assert_eq!(trunk(&repo).await?, cs["D"]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_rejects_more_than_one_merge_base(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    // Criss-cross: trunk merges X1 and X2, and the pushed head merges them
    // the other way, so both are merge bases.
    let x1 = new_commit(&repo, vec![cs["D"]], "X1").await?;
    let x2 = new_commit(&repo, vec![cs["D"]], "X2").await?;
    let t = new_commit(&repo, vec![x1, x2], "T").await?;
    set_bookmark(&repo, "trunk", t).await?;
    let h = new_commit(&repo, vec![x2, x1], "H").await?;

    for base in [x1, x2] {
        assert_invalid_request(
            land(&repo, h, base).await,
            &format!("'trunk' and {h} have more than one merge base"),
        );
    }
    assert_eq!(trunk(&repo).await?, t);
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_rejects_unrelated_head(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    let root = CreateCommitContext::new_root(repo.ctx(), repo.repo())
        .add_file("U1", "U1")
        .commit()
        .await?;
    let head = new_commit(&repo, vec![root], "U2").await?;

    assert_invalid_request(
        land(&repo, head, root).await,
        &format!("cannot land {head} onto 'trunk': they have no common ancestor"),
    );
    assert_eq!(trunk(&repo).await?, cs["D"]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_rejects_public_commits(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    // A stack on the release branch, based on its merge base with trunk:
    // landing it would rebase the release commits.
    let s = new_commit(&repo, vec![cs["R2"]], "S").await?;
    let r1 = hg_id(&repo, cs["R1"]).await?;
    let r2 = hg_id(&repo, cs["R2"]).await?;

    assert_invalid_request(
        land(&repo, s, cs["B"]).await,
        &format!("cannot rebase public commits: {r1}, {r2}"),
    );
    assert_eq!(trunk(&repo).await?, cs["D"]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_names_the_first_public_commits(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    let mut parent = cs["B"];
    let mut public = Vec::new();
    for name in ["P1", "P2", "P3", "P4"] {
        parent = new_commit(&repo, vec![parent], name).await?;
        public.push(parent);
    }
    set_bookmark(&repo, "release2", parent).await?;
    let s = new_commit(&repo, vec![parent], "S").await?;
    let p1 = hg_id(&repo, public[0]).await?;
    let p2 = hg_id(&repo, public[1]).await?;
    let p3 = hg_id(&repo, public[2]).await?;

    assert_invalid_request(
        land(&repo, s, cs["B"]).await,
        &format!("cannot rebase public commits: {p1}, {p2}, {p3}, ..."),
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_rejects_public_head(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    let c = hg_id(&repo, cs["C"]).await?;

    assert_invalid_request(
        land(&repo, cs["C"], cs["C"]).await,
        &format!("nothing to land: {c} is already an ancestor of 'trunk'"),
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_merged_in_commits(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    let trunk_key = BookmarkKey::new("trunk")?;

    // The draft side of a merge is pushed with the merge.
    let draft_merge = new_commit(&repo, vec![cs["D"], cs["F2"]], "M").await?;
    let to_land = repo
        .changesets_to_land(&trunk_key, draft_merge, cs["D"])
        .await?;
    assert_eq!(to_land.changesets, vec![cs["F1"], cs["F2"], draft_merge]);
    assert_eq!(to_land.required_root, Some(cs["D"]));

    // The public side of a merge is not.
    let release_merge = new_commit(&repo, vec![cs["D"], cs["R2"]], "M").await?;
    assert_eq!(
        repo.changesets_to_land(&trunk_key, release_merge, cs["D"])
            .await?
            .changesets,
        vec![release_merge],
    );

    // Neither is the part of a merged-in branch that is already on trunk.
    let t = new_commit(&repo, vec![cs["D"], cs["F1"]], "T").await?;
    set_bookmark(&repo, "trunk", t).await?;
    let partial_merge = new_commit(&repo, vec![t, cs["F2"]], "M").await?;
    assert_eq!(
        repo.changesets_to_land(&trunk_key, partial_merge, t)
            .await?
            .changesets,
        vec![cs["F2"], partial_merge],
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_merge_of_draft_branch(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    let merge = new_commit(&repo, vec![cs["D"], cs["F2"]], "M").await?;
    let e = new_commit(&repo, vec![cs["D"]], "E").await?;
    set_bookmark(&repo, "trunk", e).await?;

    let outcome = land(&repo, merge, cs["D"]).await?;

    // Only the merge descends from D, so only the merge is rebased. F1 and F2
    // land unchanged as its second-parent ancestry.
    let mapping = rebased(&outcome);
    assert_eq!(mapping.keys().copied().collect::<Vec<_>>(), vec![merge]);
    assert_eq!(trunk(&repo).await?, mapping[&merge]);
    assert_eq!(parents(&repo, mapping[&merge]).await?, vec![e, cs["F2"]]);
    assert_eq!(parents(&repo, cs["F2"]).await?, vec![cs["F1"]]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_merge_of_public_branch(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    let merge = new_commit(&repo, vec![cs["D"], cs["R2"]], "M").await?;
    let e = new_commit(&repo, vec![cs["D"]], "E").await?;
    set_bookmark(&repo, "trunk", e).await?;

    let outcome = land(&repo, merge, cs["D"]).await?;

    let mapping = rebased(&outcome);
    assert_eq!(mapping.keys().copied().collect::<Vec<_>>(), vec![merge]);
    assert_eq!(trunk(&repo).await?, mapping[&merge]);
    assert_eq!(parents(&repo, mapping[&merge]).await?, vec![e, cs["R2"]]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_merge_of_two_rebased_sides(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    let p1 = new_commit(&repo, vec![cs["D"]], "P1").await?;
    let p2 = new_commit(&repo, vec![cs["D"]], "P2").await?;
    let merge = new_commit(&repo, vec![p1, p2], "M").await?;
    let e = new_commit(&repo, vec![cs["D"]], "E").await?;
    set_bookmark(&repo, "trunk", e).await?;

    let outcome = land(&repo, merge, cs["D"]).await?;

    let mapping = rebased(&outcome);
    assert_eq!(mapping.len(), 3);
    assert_eq!(parents(&repo, mapping[&p1]).await?, vec![e]);
    assert_eq!(parents(&repo, mapping[&p2]).await?, vec![e]);
    assert_eq!(
        parents(&repo, mapping[&merge]).await?,
        vec![mapping[&p1], mapping[&p2]]
    );
    assert_eq!(trunk(&repo).await?, mapping[&merge]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_merge_on_bookmark(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    // Trunk has not moved since the merge was made, so nothing is rebased.
    let merge = new_commit(&repo, vec![cs["D"], cs["F2"]], "M").await?;

    let outcome = land(&repo, merge, cs["D"]).await?;

    assert_eq!(trunk(&repo).await?, outcome.head);
    assert_eq!(parents(&repo, outcome.head).await?, vec![cs["D"], cs["F2"]]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_rejects_second_parent_base(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    // Trunk merged into the feature branch: the base, D, is the merge's
    // second parent.
    let merge = new_commit(&repo, vec![cs["F2"], cs["D"]], "M").await?;
    let e = new_commit(&repo, vec![cs["D"]], "E").await?;
    set_bookmark(&repo, "trunk", e).await?;

    let err = land(&repo, merge, cs["D"])
        .await
        .expect_err("a second-parent base is forbidden by default");
    assert!(
        format!("{err:#}").contains("is not a p1 of trunk bookmark"),
        "unexpected error: {err:#}",
    );
    assert_eq!(trunk(&repo).await?, e);
    Ok(())
}

/// Runs `fut` with `scm/mononoke:pushrebase_land_stack_validation` set to
/// `setting`. The other tests run with it at 2 (reject).
async fn with_validation<T>(setting: i64, fut: impl Future<Output = T> + Send + 'static) -> T
where
    T: Send + 'static,
{
    with_just_knobs_async(
        JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:pushrebase_land_stack_validation".to_string(),
            KnobVal::Int(setting),
        )])),
        fut.boxed(),
    )
    .await
}

/// Trunk is A-B-X-T with `trunk` at T, where X changes no files. S is a
/// draft child of X.
async fn init_empty_commit_repo(
    ctx: &CoreContext,
) -> Result<(RepoContext<Repo>, BTreeMap<String, ChangesetId>)> {
    let repo: Repo = test_repo_factory::build_empty(ctx.fb).await?;
    let mut cs = create_from_dag(ctx, &repo, "A-B").await?;
    let repo = RepoContext::new_test(ctx.clone(), Arc::new(repo)).await?;
    let x = CreateCommitContext::new(repo.ctx(), repo.repo(), vec![cs["B"]])
        .commit()
        .await?;
    cs.insert("X".to_string(), x);
    let t = new_commit(&repo, vec![x], "T").await?;
    cs.insert("T".to_string(), t);
    let s = new_commit(&repo, vec![x], "S").await?;
    cs.insert("S".to_string(), s);
    set_bookmark(&repo, "trunk", t).await?;
    Ok((repo, cs))
}

#[mononoke::fbinit_test]
async fn land_stack_validation_of_mismatched_base(fb: FacebookInit) -> Result<()> {
    // S is based on B instead of X, the merge base with trunk, so X, which
    // is public, is in the stack too. It changes no files, so pushrebase
    // lands it again without conflicts. Settings 0 and 1 land the stack as
    // given; 2 rejects it.
    for setting in [0, 1, 2] {
        let ctx = CoreContext::test_mock(fb);
        let (repo, cs) = init_empty_commit_repo(&ctx).await?;
        let result = with_validation(setting, {
            let repo = repo.clone();
            let (s, b) = (cs["S"], cs["B"]);
            async move { land(&repo, s, b).await }
        })
        .await;
        if setting < 2 {
            let mapping = rebased(&result?);
            assert_eq!(
                mapping.keys().copied().collect::<HashSet<_>>(),
                hashset! {cs["X"], cs["S"]},
                "setting {setting}",
            );
        } else {
            assert_invalid_request(
                result,
                &format!(
                    "the merge base of 'trunk' and {} is {}, but the push is based on {}",
                    cs["S"], cs["X"], cs["B"],
                ),
            );
            assert_eq!(trunk(&repo).await?, cs["T"]);
        }
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_validation_of_public_commits(fb: FacebookInit) -> Result<()> {
    // A stack on the release branch, based on its merge base with trunk.
    // Settings 0 and 1 land it, rebasing the public release commits; 2
    // rejects it.
    for setting in [0, 1, 2] {
        let ctx = CoreContext::test_mock(fb);
        let (repo, cs) = init_branches_repo(&ctx).await?;
        let s = new_commit(&repo, vec![cs["R2"]], "S").await?;
        let result = with_validation(setting, {
            let repo = repo.clone();
            let b = cs["B"];
            async move { land(&repo, s, b).await }
        })
        .await;
        if setting < 2 {
            let mapping = rebased(&result?);
            assert_eq!(
                mapping.keys().copied().collect::<HashSet<_>>(),
                hashset! {cs["R1"], cs["R2"], s},
                "setting {setting}",
            );
        } else {
            assert_invalid_request(result, "cannot rebase public commits: ");
            assert_eq!(trunk(&repo).await?, cs["D"]);
        }
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_validation_of_merge(fb: FacebookInit) -> Result<()> {
    // M merges F2 into D, based on B instead of D, its merge base with
    // trunk. At 0, the commits from B to M go to pushrebase unchecked, and
    // only those: F1 and F2 are not pushed. They include C and D, which are
    // already on trunk, so pushrebase fails on the files they change. At 1
    // and 2, the merge is rejected with the merge-base message.
    for setting in [0, 1, 2] {
        let ctx = CoreContext::test_mock(fb);
        let (repo, cs) = init_branches_repo(&ctx).await?;
        let merge = new_commit(&repo, vec![cs["D"], cs["F2"]], "M").await?;
        let trunk_key = BookmarkKey::new("trunk")?;
        let to_land = with_validation(setting, {
            let repo = repo.clone();
            let (b, trunk_key) = (cs["B"], trunk_key.clone());
            async move { repo.changesets_to_land(&trunk_key, merge, b).await }
        })
        .await;
        let result = with_validation(setting, {
            let repo = repo.clone();
            let b = cs["B"];
            async move { land(&repo, merge, b).await }
        })
        .await;
        if setting == 0 {
            let to_land = to_land?;
            assert_eq!(
                to_land.changesets.into_iter().collect::<HashSet<_>>(),
                hashset! {cs["C"], cs["D"], merge},
            );
            assert_eq!(to_land.required_root, None);
            match result {
                Err(MononokeError::PushrebaseConflicts(conflicts)) => assert_eq!(
                    conflicts
                        .iter()
                        .map(|conflict| conflict.left.to_string())
                        .collect::<Vec<_>>(),
                    vec!["C", "D"],
                ),
                other => panic!("expected pushrebase conflicts, got {other:?}"),
            }
        } else {
            let message = format!(
                "the merge base of 'trunk' and {merge} is {}, but the push is based on {}",
                cs["D"], cs["B"],
            );
            assert_invalid_request(to_land, &message);
            assert_invalid_request(result, &message);
        }
        assert_eq!(trunk(&repo).await?, cs["D"], "setting {setting}");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_swaps_second_parent_base(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    let merge = new_commit(&repo, vec![cs["F2"], cs["D"]], "M").await?;
    let e = new_commit(&repo, vec![cs["D"]], "E").await?;
    set_bookmark(&repo, "trunk", e).await?;
    let pushvars = HashMap::from([("MAINLINE_PARENT_FIRST".to_string(), Bytes::from("true"))]);

    let outcome = repo
        .land_stack(
            "trunk",
            merge,
            cs["D"],
            Some(&pushvars),
            AnyKind,
            User,
            false,
        )
        .await?;

    // The merge is rebased with trunk as its first parent; F2 stays its
    // second parent.
    let mapping = rebased(&outcome);
    assert_eq!(mapping.keys().copied().collect::<Vec<_>>(), vec![merge]);
    assert_eq!(trunk(&repo).await?, mapping[&merge]);
    assert_eq!(parents(&repo, mapping[&merge]).await?, vec![e, cs["F2"]]);
    Ok(())
}

#[mononoke::fbinit_test]
async fn land_stack_rejects_bookmark_moved_back_after_check(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let (repo, cs) = init_branches_repo(&ctx).await?;
    let merge = new_commit(&repo, vec![cs["D"], cs["F2"]], "M").await?;
    let trunk_key = BookmarkKey::new("trunk")?;

    // The merge is checked with trunk at D, then trunk moves back to B
    // before it lands. Rebased from A, the closest root under B, the push
    // would also rebase the public C and D; it is rejected instead.
    let to_land = repo.changesets_to_land(&trunk_key, merge, cs["D"]).await?;
    set_bookmark(&repo, "trunk", cs["B"]).await?;
    let result = repo
        .land_changesets(trunk_key, to_land, None, AnyKind, User, false)
        .await;
    assert_invalid_request(
        result,
        "bookmark 'trunk' moved while the push was being checked. Pull and retry",
    );
    assert_eq!(trunk(&repo).await?, cs["B"]);
    Ok(())
}
