/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use blobstore::Loadable;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::BookmarkUpdateReason;
use bookmarks::BookmarksRef;
use bookmarks::Freshness;
use changesets_creation::save_changesets;
use clap::Args;
use clap::Subcommand;
use cmdlib_cross_repo::create_commit_syncers_from_app;
use context::CoreContext;
use cross_repo_sync::CHANGE_XREPO_MAPPING_EXTRA;
use cross_repo_sync::CommitSyncContext;
use cross_repo_sync::CommitSyncData;
use cross_repo_sync::Syncers;
use cross_repo_sync::unsafe_always_rewrite_sync_commit;
use filestore::FilestoreConfigRef;
use futures::stream;
use futures::try_join;
use maplit::btreemap;
use maplit::hashmap;
use maplit::hashset;
use metaconfig_types::CommitSyncConfigVersion;
use metaconfig_types::DefaultSmallToLargeCommitSyncPathAction;
use metaconfig_types::RepoConfigRef;
use mononoke_app::MononokeApp;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::ChangesetId;
use mononoke_types::DateTime;
use mononoke_types::FileChange;
use mononoke_types::FileType;
use mononoke_types::GitLfs;
use mononoke_types::NonRootMPath;
use mutable_counters::MutableCountersRef;
use pushrebase::FAIL_PUSHREBASE_EXTRA;
use pushrebase::do_pushrebase_bonsai;
use repo_blobstore::RepoBlobstoreRef;
use repo_identity::RepoIdentityRef;
use sorted_vector_map::sorted_vector_map;
use tracing::info;

use super::Repo;

/// Commands to enable/disable pushredirection
#[derive(Args)]
pub struct PushredirectionArgs {
    #[clap(subcommand)]
    subcommand: PushredirectionSubcommand,
}

#[derive(Subcommand)]
pub enum PushredirectionSubcommand {
    /// Command to prepare rollout of pushredirection
    PrepareRollout(PrepareRolloutArgs),
    /// Command to change mapping version for a given bookmark. Note that this command doesn't check
    /// that the working copies of source and target repo are equivalent according to the new mapping.
    /// This needs to ensured before calling this command
    ChangeMappingVersion(ChangeMappingVersionArgs),
}

#[derive(Args)]
pub struct PrepareRolloutArgs {}

#[derive(Args)]
pub struct ChangeMappingVersionArgs {
    /// Change mapping via pushing a commit with a special extra set. This should become
    /// the default method, but for now let's hide behind it this arg
    #[clap(long)]
    via_extra: bool,

    /// Author of the commit that will change the mapping
    #[clap(long)]
    author: String,

    /// Date for the commit that will change the mapping (in RFC-3339 format)
    #[clap(long)]
    date: Option<String>,

    /// Path in the repo where new mapping version will be dumped
    #[clap(long)]
    dump_mapping_large_repo_path: Option<String>,

    /// Bookmark name in the large repo
    #[clap(long)]
    large_repo_bookmark: String,

    /// Oncall for the commit that will change the mapping
    #[clap(long)]
    oncall: Option<String>,

    /// Mapping version to change to
    #[clap(long)]
    version_name: String,
}

pub async fn pushredirection(
    ctx: &CoreContext,
    app: &MononokeApp,
    source_repo: Repo,
    target_repo: Repo,
    args: PushredirectionArgs,
) -> Result<()> {
    let source_repo = Arc::new(source_repo);
    let target_repo = Arc::new(target_repo);

    let commit_syncers =
        create_commit_syncers_from_app(ctx, app, source_repo.clone(), target_repo.clone()).await?;

    match args.subcommand {
        PushredirectionSubcommand::PrepareRollout(args) => {
            pushredirection_prepare_rollout(ctx, commit_syncers, args).await
        }
        PushredirectionSubcommand::ChangeMappingVersion(args) => {
            pushredirection_change_mapping_version(ctx, commit_syncers, args).await
        }
    }
}

async fn pushredirection_prepare_rollout(
    ctx: &CoreContext,
    commit_syncers: Syncers<Arc<Repo>>,
    _args: PrepareRolloutArgs,
) -> Result<()> {
    let commit_sync_data = commit_syncers.large_to_small;

    if commit_sync_data
        .get_live_commit_sync_config()
        .push_redirector_enabled_for_public(
            ctx,
            commit_sync_data.get_small_repo().repo_identity().id(),
        )
        .await?
    {
        return Err(anyhow!(
            "not allowed to run prepare-rollout if pushredirection is enabled",
        ));
    }

    let small_repo = commit_sync_data.get_small_repo();
    let large_repo = commit_sync_data.get_large_repo();

    let largest_id = large_repo
        .bookmark_update_log()
        .get_largest_log_id(ctx.clone(), Freshness::MostRecent)
        .await?
        .ok_or_else(|| anyhow!("No bookmarks update log entries for large repo"))?;

    let counter = backsyncer::format_counter(&large_repo.repo_identity().id());
    info!(
        "setting value {} to counter {} for repo {}",
        largest_id,
        counter,
        small_repo.repo_identity().id()
    );
    let res = small_repo
        .mutable_counters()
        .set_counter(
            ctx,
            &counter,
            largest_id.try_into().unwrap(),
            None, // prev_value
        )
        .await?;

    if !res {
        Err(anyhow!("failed to set backsyncer counter"))
    } else {
        info!("successfully updated the counter");
        Ok(())
    }
}

async fn pushredirection_change_mapping_version(
    ctx: &CoreContext,
    commit_syncers: Syncers<Arc<Repo>>,
    args: ChangeMappingVersionArgs,
) -> Result<()> {
    let commit_sync_data = commit_syncers.large_to_small;

    let large_repo = commit_sync_data.get_large_repo();
    let small_repo = commit_sync_data.get_small_repo();

    let mapping_version = CommitSyncConfigVersion(args.version_name);
    if !commit_sync_data.version_exists(&mapping_version).await? {
        return Err(anyhow!("{mapping_version} version does not exist"));
    }

    let author = args.author;

    let author_date = args.date.map_or_else(
        || Ok(DateTime::now()),
        |date| DateTime::from_rfc3339(date.as_str()),
    )?;

    let oncall_msg_part = args.oncall.map(|o| format!("\n\nOncall Short Name: {o}\n"));

    let commit_msg = format!(
        "Changing synced mapping version to {} for {}->{} sync{}",
        mapping_version,
        large_repo.repo_identity().name(),
        small_repo.repo_identity().name(),
        oncall_msg_part.as_deref().unwrap_or("")
    );

    let dump_mapping_file = args
        .dump_mapping_large_repo_path
        .map(NonRootMPath::new)
        .transpose()?;

    let large_bookmark = BookmarkKey::new(args.large_repo_bookmark)?;
    let small_bookmark = commit_sync_data
        .rename_bookmark(&large_bookmark)
        .await?
        .ok_or_else(|| anyhow!("{large_bookmark} bookmark doesn't remap to small repo"))?;

    let large_bookmark_value = get_bookmark_value(ctx, large_repo, &large_bookmark).await?;
    let small_bookmark_value = get_bookmark_value(ctx, small_repo, &small_bookmark).await?;

    if args.via_extra {
        return change_mapping_via_extras(
            ctx,
            &commit_sync_data,
            &mapping_version,
            large_bookmark,
            large_bookmark_value,
            dump_mapping_file,
            author,
            author_date,
            commit_msg,
        )
        .await;
    }

    if commit_sync_data
        .get_live_commit_sync_config()
        .push_redirector_enabled_for_public(
            ctx,
            commit_sync_data.get_small_repo().repo_identity().id(),
        )
        .await?
    {
        return Err(anyhow!(
            "not allowed to run change-mapping-version if pushredirection is enabled",
        ));
    }

    let large_cs_id = create_commit_for_mapping_change(
        ctx,
        &commit_sync_data,
        large_bookmark_value,
        &mapping_version,
        false,
        dump_mapping_file,
        author,
        author_date,
        commit_msg,
    )
    .await?;

    let maybe_rewritten_small_cs_id = unsafe_always_rewrite_sync_commit(
        ctx,
        large_cs_id,
        &commit_sync_data,
        Some(hashmap! {
          large_bookmark_value => small_bookmark_value,
        }),
        &mapping_version,
        CommitSyncContext::AdminChangeMapping,
    )
    .await?;

    let rewritten_small_cs_id = maybe_rewritten_small_cs_id
        .ok_or_else(|| anyhow!("{large_cs_id} was rewritten into non-existent commit"))?;

    let large_repo_bookmark_move = move_bookmark(
        ctx,
        large_repo,
        &large_bookmark,
        large_bookmark_value,
        large_cs_id,
    );

    let small_repo_bookmark_move = move_bookmark(
        ctx,
        small_repo,
        &small_bookmark,
        small_bookmark_value,
        rewritten_small_cs_id,
    );

    try_join!(large_repo_bookmark_move, small_repo_bookmark_move)?;

    Ok(())
}

async fn change_mapping_via_extras(
    ctx: &CoreContext,
    commit_sync_data: &CommitSyncData<Arc<Repo>>,
    mapping_version: &CommitSyncConfigVersion,
    large_bookmark: BookmarkKey,
    large_bookmark_value: ChangesetId,
    dump_mapping_file: Option<NonRootMPath>,
    author: String,
    author_date: DateTime,
    commit_msg: String,
) -> Result<()> {
    // XXX(mitrandir): remove this check once this mode works regardless of sync direction
    if !commit_sync_data
        .get_live_commit_sync_config()
        .push_redirector_enabled_for_public(
            ctx,
            commit_sync_data.get_small_repo().repo_identity().id(),
        )
        .await?
        && std::env::var("MONONOKE_ADMIN_ALWAYS_ALLOW_MAPPING_CHANGE_VIA_EXTRA").is_err()
    {
        return Err(anyhow!(
            "not allowed to run change-mapping-version if pushredirection is not enabled"
        ));
    }

    let large_repo = commit_sync_data.get_large_repo();

    let large_cs_id = create_commit_for_mapping_change(
        ctx,
        commit_sync_data,
        large_bookmark_value,
        mapping_version,
        true,
        dump_mapping_file,
        author,
        author_date,
        commit_msg,
    )
    .await?;

    let pushrebase_flags = &large_repo.repo_config().pushrebase.flags;
    let pushrebase_hooks = bookmarks_movement::get_pushrebase_hooks(
        ctx,
        large_repo,
        &large_bookmark,
        &large_repo.repo_config().pushrebase,
        None,
    )
    .await?;

    let bcs = large_cs_id.load(ctx, large_repo.repo_blobstore()).await?;
    let pushrebase_res = do_pushrebase_bonsai(
        ctx,
        large_repo,
        pushrebase_flags,
        &large_bookmark,
        &hashset![bcs],
        &pushrebase_hooks,
        None,
    )
    .await?;

    println!("{}", pushrebase_res.head);

    Ok(())
}

async fn create_commit_for_mapping_change(
    ctx: &CoreContext,
    commit_sync_data: &CommitSyncData<Arc<Repo>>,
    parent: ChangesetId,
    mapping_version: &CommitSyncConfigVersion,
    add_mapping_change_extra: bool,
    dump_mapping_file: Option<NonRootMPath>,
    author: String,
    author_date: DateTime,
    commit_msg: String,
) -> Result<ChangesetId> {
    let mut extras = sorted_vector_map! {
        FAIL_PUSHREBASE_EXTRA.to_string() => b"1".to_vec(),
    };
    if add_mapping_change_extra {
        extras.insert(
            CHANGE_XREPO_MAPPING_EXTRA.to_string(),
            mapping_version.0.clone().into_bytes(),
        );
    }

    let file_changes =
        create_file_changes(ctx, commit_sync_data, mapping_version, dump_mapping_file).await?;

    // Create an empty commit on top of large bookmark
    let bcs = BonsaiChangesetMut {
        parents: vec![parent],
        author,
        author_date,
        message: commit_msg,
        hg_extra: extras,
        file_changes: file_changes.into(),
        ..Default::default()
    }
    .freeze()?;

    let large_cs_id = bcs.get_changeset_id();
    save_changesets(ctx, &commit_sync_data.get_large_repo(), vec![bcs]).await?;

    Ok(large_cs_id)
}

async fn create_file_changes(
    ctx: &CoreContext,
    commit_sync_data: &CommitSyncData<Arc<Repo>>,
    mapping_version: &CommitSyncConfigVersion,
    dump_mapping_file: Option<NonRootMPath>,
) -> Result<BTreeMap<NonRootMPath, FileChange>> {
    let path = if let Some(path) = dump_mapping_file {
        path
    } else {
        return Ok(Default::default());
    };

    let large_repo = commit_sync_data.get_large_repo();
    let small_repo = commit_sync_data.get_small_repo();

    // This "dump-mapping-file" is going to be created in the large repo,
    // but this file needs to rewrite to a small repo as well. If it doesn't
    // rewrite to a small repo, then the whole mapping change commit isn't
    // going to exist in the small repo.

    let movers = commit_sync_data
        .get_movers_by_version(mapping_version)
        .await?;

    let mover = if commit_sync_data.get_source_repo().repo_identity().id()
        == large_repo.repo_identity().id()
    {
        movers.mover
    } else {
        movers.reverse_mover
    };

    if mover.move_path(&path)?.is_none() {
        return Err(anyhow!(
            "cannot dump mapping to a file because path doesn't rewrite to a small repo"
        ));
    }

    // Now get the mapping and create json with it
    let commit_sync_config = commit_sync_data
        .get_live_commit_sync_config()
        .get_commit_sync_config_by_version(large_repo.repo_identity().id(), mapping_version)
        .await?;

    let small_repo_sync_config = commit_sync_config
        .small_repos
        .get(&small_repo.repo_identity().id())
        .ok_or_else(|| {
            anyhow!(
                "small repo {} not found in {} mapping",
                small_repo.repo_identity().id(),
                mapping_version
            )
        })?;

    let default_prefix = match &small_repo_sync_config.default_action {
        DefaultSmallToLargeCommitSyncPathAction::Preserve => String::new(),
        DefaultSmallToLargeCommitSyncPathAction::PrependPrefix(prefix) => prefix.to_string(),
    };

    let mut map = serde_json::Map::new();
    map.insert("default_prefix".to_string(), default_prefix.into());
    let mut map_overrides = serde_json::Map::new();
    for (key, value) in &small_repo_sync_config.map {
        map_overrides.insert(key.to_string(), value.to_string().into());
    }
    map.insert("overrides".to_string(), map_overrides.into());

    let content = (get_generated_string() + &serde_json::to_string_pretty(&map)?).into_bytes();
    let content = bytes::Bytes::from(content);
    let size = content.len() as u64;
    let content_metadata = filestore::store(
        large_repo.repo_blobstore(),
        *large_repo.filestore_config(),
        ctx,
        &filestore::StoreRequest::new(size),
        stream::once(async move { Ok(content) }),
    )
    .await?;

    let file_change = FileChange::tracked(
        content_metadata.content_id,
        FileType::Regular,
        size,
        None,
        GitLfs::FullContent,
    );

    Ok(btreemap! {
        path => file_change,
    })
}

// Mark content as (at)generated to discourage people from modifying it
// manually.
// However split this so that this source file is not marked as generated
fn get_generated_string() -> String {
    "\x40generated by the megarepo bind, reach out to Source Control @ FB with any questions\n"
        .to_owned()
}

async fn get_bookmark_value(
    ctx: &CoreContext,
    repo: &Arc<Repo>,
    bookmark: &BookmarkKey,
) -> Result<ChangesetId> {
    let maybe_bookmark_value = repo
        .bookmarks()
        .get(ctx.clone(), bookmark, bookmarks::Freshness::MostRecent)
        .await?;

    maybe_bookmark_value.ok_or_else(|| {
        anyhow!(
            "{} is not found in {}",
            bookmark,
            repo.repo_identity().name()
        )
    })
}

async fn move_bookmark(
    ctx: &CoreContext,
    repo: &Arc<Repo>,
    bookmark: &BookmarkKey,
    prev_value: ChangesetId,
    new_value: ChangesetId,
) -> Result<()> {
    let mut book_txn = repo.bookmarks().create_transaction(ctx.clone());

    info!(
        "moving {} to {} in {}",
        bookmark,
        new_value,
        repo.repo_identity().name()
    );
    book_txn.update(
        bookmark,
        new_value,
        prev_value,
        BookmarkUpdateReason::ManualMove,
    )?;

    let res = book_txn.commit().await?.is_some();

    if res {
        Ok(())
    } else {
        Err(anyhow!(
            "failed to move bookmark {} in {}",
            bookmark,
            repo.repo_identity().name()
        ))
    }
}
