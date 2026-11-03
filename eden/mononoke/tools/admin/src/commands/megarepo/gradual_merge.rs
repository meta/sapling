/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

use anyhow::Result;
use anyhow::anyhow;
use blobstore::Loadable;
use bookmarks::BookmarkKey;
use bookmarks::BookmarksRef;
use commit_graph::CommitGraphRef;
use commit_id::CommitIdNames;
use commit_id::NamedCommitIdsArgs;
use commit_id::resolve_commit_id;
use context::CoreContext;
use futures::StreamExt;
use maplit::hashset;
use megarepolib::common::ChangesetArgs;
use megarepolib::common::ChangesetArgsFactory;
use megarepolib::common::StackPosition;
use megarepolib::common::create_and_save_bonsai;
use mercurial_derivation::DeriveHgChangeset;
use metaconfig_types::PushrebaseFlags;
use metaconfig_types::RepoConfigRef;
use mononoke_api::Repo;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArgs;
use mononoke_types::ChangesetId;
use mononoke_types::DateTime;
use pushrebase::do_pushrebase_bonsai;
use pushrebase_hooks::get_pushrebase_hooks;
use repo_blobstore::RepoBlobstoreRef;
use tracing::info;

use crate::commands::megarepo::common::LightResultingChangesetArgs;

#[derive(Copy, Clone, Debug)]
struct GradualMergeCommitIdNames;

impl CommitIdNames for GradualMergeCommitIdNames {
    const NAMES: &'static [(&'static str, &'static str)] = &[
        (
            "pre-deletion-commit",
            "Include only descendants of the next commit",
        ),
        (
            "last-deletion-commit",
            "Exclude ancestors of the next commit",
        ),
    ];
}

#[derive(Debug, clap::Args)]
pub struct GradualMergeArgs {
    #[clap(flatten)]
    pub repo_args: RepoArgs,

    /// Metadata for the generated commit
    #[clap(flatten)]
    pub res_cs_args: LightResultingChangesetArgs,

    #[clap(flatten)]
    commit_ids_args: NamedCommitIdsArgs<GradualMergeCommitIdNames>,

    /// Bookmark to merge into
    #[clap(long)]
    target_bookmark: String,

    /// Limit the number of commits to merge
    #[clap(long)]
    limit: Option<usize>,

    #[clap(long)]
    dry_run: bool,
}

pub async fn run(ctx: &CoreContext, app: MononokeApp, args: GradualMergeArgs) -> Result<()> {
    let repo: Repo = app.open_repo(&args.repo_args).await?;
    let pre_deletion_commit = resolve_commit_id(
        ctx,
        &repo,
        args.commit_ids_args
            .named_commit_ids()
            .get("pre-deletion-commit")
            .ok_or(anyhow!("pre-deletion-commit is required"))?,
    )
    .await?;
    let last_deletion_commit = resolve_commit_id(
        ctx,
        &repo,
        args.commit_ids_args
            .named_commit_ids()
            .get("last-deletion-commit")
            .ok_or(anyhow!("last-deletion-commit is required"))?,
    )
    .await?;
    let bookmark_to_merge_into = BookmarkKey::new(&args.target_bookmark)?;
    let merge_changeset_args_factory =
        get_gradual_merge_commits_cs_args_factory(args.res_cs_args.clone())?;

    gradual_merge(
        ctx,
        &repo,
        &pre_deletion_commit,
        &last_deletion_commit,
        &bookmark_to_merge_into,
        args.limit,
        args.dry_run,
        &repo.repo_config().pushrebase.flags,
        merge_changeset_args_factory,
    )
    .await?;

    Ok(())
}

/// Finds all commits that needs to be merged, regardless of whether they've been merged
/// already or not.
async fn find_all_commits_to_merge(
    ctx: &CoreContext,
    repo: &Repo,
    pre_deletion_commit: ChangesetId,
    last_deletion_commit: ChangesetId,
) -> Result<Vec<ChangesetId>> {
    info!("Finding all commits to merge...");
    let commits_to_merge = repo
        .commit_graph()
        .range_stream(ctx, pre_deletion_commit, last_deletion_commit)
        .await?
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .rev()
        .collect::<Vec<_>>();

    Ok(commits_to_merge)
}
/// Out of all commits that need to be merged find commits that haven't been merged yet
async fn find_unmerged_commits(
    ctx: &CoreContext,
    repo: &Repo,
    mut commits_to_merge: Vec<(ChangesetId, StackPosition)>,
    bookmark_to_merge_into: &BookmarkKey,
) -> Result<Vec<(ChangesetId, StackPosition)>> {
    info!("Finding commits that haven't been merged yet...");
    let first = if let Some(first) = commits_to_merge.first() {
        first
    } else {
        return Ok(vec![]);
    };

    let bookmark_value = repo
        .bookmarks()
        .get(
            ctx.clone(),
            bookmark_to_merge_into,
            bookmarks::Freshness::MostRecent,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("Bookmark {bookmark_to_merge_into} doesn't exist"))?;

    // Let's check if any commits has been merged already - to do that it's enough
    // to check if the first commit has been merged or not i.e. check if this commit
    // is ancestor of bookmark_to_merge_into or not.
    let has_merged_any = repo
        .commit_graph()
        .is_ancestor(ctx, first.0, bookmark_value)
        .await?;

    if !has_merged_any {
        return Ok(commits_to_merge);
    }

    // At this point we know that we've already merged at least a single commit.
    // Let's find the last commit that has been merged already.
    // To do that we do a bfs traversal starting from bookmark_to_merge_into, and
    // we stop as soon as we've found a commit from commits_to_merge list - because
    // we are merging starting from the top deletion commit, the latest merged commit
    // will be the closest to bookmark_to_merge_into
    //
    //   O <- bookmark_to_merge_into
    //   |
    //  ...
    //   M_B  <- we'll find this commit before M_A,  it's parent is commit B.
    //   |
    //  ...
    //   |
    //   M_A
    //   |
    //   O <- head where we want to merge
    //   |
    //   |      A - deletion commit, it was merged first in M_A
    //   O      |
    //   |      B - deletion commit, it was merged second in M_B
    //   O      |
    //          C <- it hasn't been merged yet
    //         ...
    //
    let mut commits_to_idx = HashMap::new();
    for (cs_id, stack_pos) in commits_to_merge.iter() {
        commits_to_idx.insert(*cs_id, *stack_pos);
    }

    let mut queue = VecDeque::new();
    queue.push_back(bookmark_value);
    let mut visited = HashSet::new();
    visited.insert(bookmark_value);
    while let Some(cs_id) = queue.pop_back() {
        if let Some(found_idx) = commits_to_idx.get(&cs_id) {
            return Ok(commits_to_merge.split_off(found_idx.0 + 1));
        }

        let parents = repo.commit_graph().changeset_parents(ctx, cs_id).await?;
        for p in parents {
            if visited.insert(p) {
                queue.push_back(p);
            }
        }
    }

    Ok(commits_to_merge)
}

async fn push_merge_commit(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id_to_merge: ChangesetId,
    bookmark_to_merge_into: &BookmarkKey,
    merge_changeset_args: ChangesetArgs,
    pushrebase_flags: &PushrebaseFlags,
) -> Result<ChangesetId> {
    info!("Preparing to merge {}", cs_id_to_merge);
    let bookmark_value = repo
        .bookmarks()
        .get(
            ctx.clone(),
            bookmark_to_merge_into,
            bookmarks::Freshness::MostRecent,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("Bookmark {bookmark_to_merge_into} doesn't exist"))?;

    let merge_cs_id = create_and_save_bonsai(
        ctx,
        repo,
        vec![bookmark_value, cs_id_to_merge],
        Default::default(),
        merge_changeset_args,
    )
    .await?;

    info!("Created merge changeset {}", merge_cs_id);

    let merge_hg_cs_id = repo.derive_hg_changeset(ctx, merge_cs_id).await?;
    info!("Generated hg changeset {}", merge_hg_cs_id);
    info!("Now running pushrebase...");

    let merge_cs = merge_cs_id.load(ctx, repo.repo_blobstore()).await?;
    let pushrebase_hooks = get_pushrebase_hooks(
        ctx,
        repo,
        bookmark_to_merge_into,
        &repo.repo_config().pushrebase,
        None,
    )
    .await?;

    let pushrebase_res = do_pushrebase_bonsai(
        ctx,
        repo,
        pushrebase_flags,
        bookmark_to_merge_into,
        &hashset![merge_cs],
        &pushrebase_hooks,
    )
    .await?;

    info!("Pushrebased to {}", pushrebase_res.head);
    Ok(pushrebase_res.head)
}

/// This function implements a strategy to merge a large repository into another
/// while avoiding sudden increase in the working copy size.
/// Normally this function should be called after a list of deletion has been created
/// (see pre_merge_delete functions). Each of these deletion commits decreases the
/// size of the working copy, and so each of the merge adds just part of the working copy.
/// See <https://fburl.com/ujc6sv7x> for more details.
///
///
///   M_B
///   |
///  ...
///   |
///   M_A
///   |
///   O <- head where we want to merge
///   |
///   |      A - deletion commit
///   O      |
///   |      B - deletion commit
///   O      |
///          O <- commit that we want to merge
///         ...
///
/// M_A, M_B are merge commits that merge deletion commits. Note that the top deletion commit A
/// is merged first, and deletion commit B is merged next.
pub(crate) async fn gradual_merge(
    ctx: &CoreContext,
    repo: &Repo,
    pre_deletion_commit: &ChangesetId,
    last_deletion_commit: &ChangesetId,
    bookmark_to_merge_into: &BookmarkKey,
    limit: Option<usize>,
    dry_run: bool,
    pushrebase_flags: &PushrebaseFlags,
    merge_changeset_args_factory: Box<dyn ChangesetArgsFactory>,
) -> Result<HashMap<ChangesetId, ChangesetId>> {
    let commits_to_merge =
        find_all_commits_to_merge(ctx, repo, *pre_deletion_commit, *last_deletion_commit).await?;

    info!("{} total commits to merge", commits_to_merge.len());

    let commits_to_merge = commits_to_merge
        .into_iter()
        .enumerate()
        .map(|(idx, cs_id)| (cs_id, StackPosition(idx)))
        .collect();

    let unmerged_commits =
        find_unmerged_commits(ctx, repo, commits_to_merge, bookmark_to_merge_into).await?;
    let unmerged_commits = if let Some(limit) = limit {
        unmerged_commits.into_iter().take(limit).collect()
    } else {
        unmerged_commits
    };
    info!("merging {} commits", unmerged_commits.len());

    let mut res = HashMap::new();
    if !dry_run {
        for (cs_id, stack_pos) in unmerged_commits {
            let merge_changeset_args = merge_changeset_args_factory(stack_pos);
            let res_cs_id = push_merge_commit(
                ctx,
                repo,
                cs_id,
                bookmark_to_merge_into,
                merge_changeset_args,
                pushrebase_flags,
            )
            .await?;
            res.insert(cs_id, res_cs_id);
        }
    } else {
        for (cs_id, stack_pos) in unmerged_commits {
            info!(
                "merging commits {}, with stack position {:?}",
                cs_id, stack_pos
            );
        }
    }

    Ok(res)
}

fn get_gradual_merge_commits_cs_args_factory(
    res_cs_args: LightResultingChangesetArgs,
) -> Result<Box<dyn ChangesetArgsFactory>> {
    let datetime = res_cs_args
        .datetime
        .as_deref()
        .map_or_else(|| Ok(DateTime::now()), DateTime::from_rfc3339)?;
    Ok(Box::new(move |stack_pos: StackPosition| ChangesetArgs {
        author: res_cs_args.commit_author.to_string(),
        message: format!(
            "[MEGAREPO GRADUAL MERGE] {} ({})",
            res_cs_args.commit_message, stack_pos.0
        ),
        datetime,
        bookmark: None,
        mark_public: false,
    }))
}

#[cfg(test)]
mod test {
    use anyhow::Error;
    use fbinit::FacebookInit;
    use maplit::hashmap;
    use mononoke_macros::mononoke;
    use mononoke_types::DateTime;
    use mononoke_types::NonRootMPath;
    use tests_utils::CreateCommitContext;
    use tests_utils::bookmark;
    use tests_utils::drawdag::create_from_dag;
    use tests_utils::list_working_copy_utf8;

    use super::*;

    #[mononoke::fbinit_test]
    async fn test_find_all_commits_to_merge(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, pre_deletion_commit, deletion_commits) = create_repo(&ctx).await?;
        let commits_to_merge = find_all_commits_to_merge(
            &ctx,
            &repo,
            pre_deletion_commit,
            *deletion_commits.last().unwrap(),
        )
        .await?;
        let mut expected = vec![pre_deletion_commit];
        expected.extend(deletion_commits);
        expected.reverse();
        assert_eq!(commits_to_merge, expected);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_find_unmerged_commits(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, pre_deletion_commit, deletion_commits) = create_repo(&ctx).await?;
        let commits_to_merge = find_all_commits_to_merge(
            &ctx,
            &repo,
            pre_deletion_commit,
            *deletion_commits.last().unwrap(),
        )
        .await?;

        let commits_to_merge = commits_to_merge
            .into_iter()
            .enumerate()
            .map(|(idx, cs_id)| (cs_id, StackPosition(idx)))
            .collect::<Vec<_>>();
        let head = BookmarkKey::new("head")?;
        let unmerged_commits =
            find_unmerged_commits(&ctx, &repo, commits_to_merge.clone(), &head).await?;

        assert_eq!(commits_to_merge, unmerged_commits);

        // Now merge a single commit into "head"
        let head_value = repo
            .bookmarks()
            .get(ctx.clone(), &head, bookmarks::Freshness::MostRecent)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Bookmark {head} doesn't exist"))?;

        let merge = CreateCommitContext::new(
            &ctx,
            &repo,
            vec![head_value, unmerged_commits.first().unwrap().0],
        )
        .commit()
        .await?;
        bookmark(&ctx, &repo, "head").set_to(merge).await?;

        let unmerged_commits =
            find_unmerged_commits(&ctx, &repo, commits_to_merge.clone(), &head).await?;
        let mut expected = commits_to_merge.clone();
        expected.remove(0);
        assert_eq!(expected, unmerged_commits);

        // Merge next commit into head
        let head_value = repo
            .bookmarks()
            .get(ctx.clone(), &head, bookmarks::Freshness::MostRecent)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Bookmark {head} doesn't exist"))?;

        let merge = CreateCommitContext::new(
            &ctx,
            &repo,
            vec![head_value, unmerged_commits.first().unwrap().0],
        )
        .commit()
        .await?;
        bookmark(&ctx, &repo, "head").set_to(merge).await?;

        let unmerged_commits =
            find_unmerged_commits(&ctx, &repo, commits_to_merge.clone(), &head).await?;
        let mut expected = commits_to_merge.clone();
        expected.remove(0);
        expected.remove(0);
        assert_eq!(expected, unmerged_commits);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_gradual_merge(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, pre_deletion_commit, deletion_commits) = create_repo(&ctx).await?;

        let pushrebase_flags = PushrebaseFlags {
            rewritedates: true,
            forbid_p2_root_rebases: true,
            casefolding_check: true,
            recursion_limit: None,
            ..Default::default()
        };

        // Test dry-run mode
        let merged = gradual_merge(
            &ctx,
            &repo,
            &pre_deletion_commit,
            deletion_commits.last().unwrap(),
            &BookmarkKey::new("head")?,
            None,
            true, // dry_run
            &pushrebase_flags,
            get_default_merge_args_factory(),
        )
        .await?;
        assert!(merged.is_empty());

        let merged = gradual_merge(
            &ctx,
            &repo,
            &pre_deletion_commit,
            deletion_commits.last().unwrap(),
            &BookmarkKey::new("head")?,
            None,
            false, // dry_run
            &pushrebase_flags,
            get_default_merge_args_factory(),
        )
        .await?;
        verify_gradual_merges(&ctx, &repo, merged, pre_deletion_commit, &deletion_commits).await?;

        Ok(())
    }
    #[mononoke::fbinit_test]
    async fn test_gradual_merge_with_limit(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, pre_deletion_commit, deletion_commits) = create_repo(&ctx).await?;

        let pushrebase_flags = PushrebaseFlags {
            rewritedates: true,
            forbid_p2_root_rebases: true,
            casefolding_check: true,
            recursion_limit: None,
            ..Default::default()
        };

        let mut result = HashMap::new();
        for _ in 0..3 {
            let merged = gradual_merge(
                &ctx,
                &repo,
                &pre_deletion_commit,
                deletion_commits.last().unwrap(),
                &BookmarkKey::new("head")?,
                Some(1),
                false, // dry_run
                &pushrebase_flags,
                get_default_merge_args_factory(),
            )
            .await?;
            assert_eq!(merged.len(), 1);
            result.extend(merged)
        }
        verify_gradual_merges(&ctx, &repo, result, pre_deletion_commit, &deletion_commits).await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_stack_position(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, pre_deletion_commit, deletion_commits) = create_repo(&ctx).await?;

        let args_factory = Box::new(|stack_pos: StackPosition| ChangesetArgs {
            author: "author".to_string(),
            message: format!("{}", stack_pos.0),
            datetime: DateTime::now(),
            bookmark: None,
            mark_public: false,
        });

        let pushrebase_flags = PushrebaseFlags {
            rewritedates: true,
            forbid_p2_root_rebases: true,
            casefolding_check: true,
            recursion_limit: None,
            ..Default::default()
        };

        let mut result = HashMap::new();
        for i in 0..3 {
            let merged = gradual_merge(
                &ctx,
                &repo,
                &pre_deletion_commit,
                deletion_commits.last().unwrap(),
                &BookmarkKey::new("head")?,
                Some(1),
                false, // dry_run
                &pushrebase_flags,
                args_factory.clone(),
            )
            .await?;
            assert_eq!(merged.len(), 1);
            let pushrebased_cs_id = merged.values().next().unwrap();
            let bcs_id = pushrebased_cs_id.load(&ctx, repo.repo_blobstore()).await?;
            assert_eq!(bcs_id.message(), format!("{i}"));
            result.extend(merged)
        }
        verify_gradual_merges(&ctx, &repo, result, pre_deletion_commit, &deletion_commits).await?;

        Ok(())
    }

    async fn create_repo(
        ctx: &CoreContext,
    ) -> Result<(Repo, ChangesetId, Vec<ChangesetId>), Error> {
        let repo: Repo = test_repo_factory::build_empty(ctx.fb).await?;

        let dag = create_from_dag(
            ctx,
            &repo,
            r##"
                A-B-C
 
                D-E-F
              "##,
        )
        .await?;
        let pre_deletion_commit = *dag.get("F").unwrap();

        // Create deletion commits
        let first_deletion_commit = CreateCommitContext::new(ctx, &repo, vec![pre_deletion_commit])
            .delete_file("F")
            .commit()
            .await?;

        let second_deletion_commit =
            CreateCommitContext::new(ctx, &repo, vec![first_deletion_commit])
                .delete_file("E")
                .commit()
                .await?;

        bookmark(ctx, &repo, "head")
            .set_to(*dag.get("C").unwrap())
            .await?;

        Ok((
            repo,
            pre_deletion_commit,
            vec![first_deletion_commit, second_deletion_commit],
        ))
    }

    async fn verify_gradual_merges(
        ctx: &CoreContext,
        repo: &Repo,
        gradual_merge_result: HashMap<ChangesetId, ChangesetId>,
        pre_deletion_commit: ChangesetId,
        deletion_commits: &[ChangesetId],
    ) -> Result<(), Error> {
        assert_eq!(gradual_merge_result.len(), 3);
        let working_copy = list_working_copy_utf8(
            ctx,
            &repo,
            *gradual_merge_result.get(&pre_deletion_commit).unwrap(),
        )
        .await?;
        assert_eq!(
            hashmap! {
                NonRootMPath::new("A")? => "A".to_string(),
                NonRootMPath::new("B")? => "B".to_string(),
                NonRootMPath::new("C")? => "C".to_string(),
                NonRootMPath::new("D")? => "D".to_string(),
                NonRootMPath::new("E")? => "E".to_string(),
                NonRootMPath::new("F")? => "F".to_string(),
            },
            working_copy
        );

        let working_copy = list_working_copy_utf8(
            ctx,
            &repo,
            *gradual_merge_result.get(&deletion_commits[0]).unwrap(),
        )
        .await?;
        assert_eq!(
            hashmap! {
                NonRootMPath::new("A")? => "A".to_string(),
                NonRootMPath::new("B")? => "B".to_string(),
                NonRootMPath::new("C")? => "C".to_string(),
                NonRootMPath::new("D")? => "D".to_string(),
                NonRootMPath::new("E")? => "E".to_string(),
            },
            working_copy
        );

        let working_copy = list_working_copy_utf8(
            ctx,
            &repo,
            *gradual_merge_result.get(&deletion_commits[1]).unwrap(),
        )
        .await?;
        assert_eq!(
            hashmap! {
                NonRootMPath::new("A")? => "A".to_string(),
                NonRootMPath::new("B")? => "B".to_string(),
                NonRootMPath::new("C")? => "C".to_string(),
                NonRootMPath::new("D")? => "D".to_string(),
            },
            working_copy
        );

        for merge_cs_id in gradual_merge_result.values() {
            let hg_cs_id = repo.derive_hg_changeset(ctx, *merge_cs_id).await?;
            let hg_cs = hg_cs_id.load(ctx, repo.repo_blobstore()).await?;
            assert!(hg_cs.files().is_empty());
        }
        Ok(())
    }

    fn get_default_merge_args_factory() -> Box<dyn ChangesetArgsFactory> {
        Box::new(|_| ChangesetArgs {
            author: "author".to_string(),
            message: "merge".to_string(),
            datetime: DateTime::now(),
            bookmark: None,
            mark_public: false,
        })
    }
}
