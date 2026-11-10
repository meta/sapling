/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;

use anyhow::Error;
use anyhow::Result;
use blobstore::Loadable;
use bookmarks::BookmarkKey;
use clap::Args;
use commit_id::parse_commit_id;
use context::CoreContext;
use futures::StreamExt;
use futures::TryFutureExt;
use futures::TryStreamExt;
use futures::stream;
use metaconfig_types::RepoConfigRef;
use repo_blobstore::RepoBlobstoreRef;

use super::Repo;

#[derive(Args)]
pub struct CommitPushrebaseArgs {
    /// Destination Bookmark to pushrebase onto
    #[clap(long, short = 'B')]
    bookmark: BookmarkKey,

    /// Source Commit ID to pushrebase (bottom of the stack if pushrebasing a stack)
    #[clap(long, short = 's')]
    source: String,

    /// Top Commit ID of the source stack, if pushrebasing a stack
    #[clap(long, short = 't')]
    top: Option<String>,
}

pub async fn pushrebase(
    ctx: &CoreContext,
    repo: &Repo,
    pushrebase_args: CommitPushrebaseArgs,
) -> Result<()> {
    let source = parse_commit_id(ctx, repo, &pushrebase_args.source).await?;

    let csids = if let Some(top) = &pushrebase_args.top {
        let top = parse_commit_id(ctx, repo, top).await?;
        super::resolve_stack(ctx, repo, source, top).await?
    } else {
        vec![source]
    };

    let pushrebase_flags = &repo.repo_config().pushrebase.flags;
    let pushrebase_hooks = bookmarks_movement::get_pushrebase_hooks(
        ctx,
        repo,
        &pushrebase_args.bookmark,
        &repo.repo_config().pushrebase,
        None,
    )
    .await
    .map_err(Error::from)?;

    let bonsais = stream::iter(csids)
        .map(|csid| async move {
            csid.load(ctx, repo.repo_blobstore())
                .map_err(Error::from)
                .await
        })
        .buffered(100)
        .try_collect::<HashSet<_>>()
        .await?;

    let result = pushrebase::do_pushrebase_bonsai(
        ctx,
        repo,
        pushrebase_flags,
        &pushrebase_args.bookmark,
        &bonsais,
        &pushrebase_hooks,
        None,
    )
    .map_err(Error::from)
    .await?;

    println!("{}", result.head);

    Ok(())
}
