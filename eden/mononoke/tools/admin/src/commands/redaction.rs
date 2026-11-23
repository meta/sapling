/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod aws_sync;
mod create_key_list;
pub(crate) mod delete_from_cas;
mod list;

use anyhow::Result;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use bonsai_p4_mapping::BonsaiP4Mapping;
use bonsai_svnrev_mapping::BonsaiSvnrevMapping;
use bookmarks::Bookmarks;
use clap::Parser;
use clap::Subcommand;
use create_key_list::RedactionCreateKeyListArgs;
use create_key_list::RedactionFetchKeyListArgs;
use create_key_list::RedactionSyncKeyListsFromJsonArgs;
use create_key_list::RedactionSyncToAwsArgs;
use delete_from_cas::RedactionDeleteFromCasArgs;
use list::RedactionListArgs;
use metaconfig_types::RepoConfig;
use mononoke_app::MononokeApp;
use repo_blobstore::RepoBlobstore;
use repo_derived_data::RepoDerivedData;
use repo_identity::RepoIdentity;

/// Manage redaction of repository contents
#[derive(Parser)]
pub struct CommandArgs {
    #[clap(subcommand)]
    subcommand: RedactionSubcommand,
}

#[facet::container]
pub struct Repo {
    #[facet]
    repo_config: RepoConfig,

    #[facet]
    bookmarks: dyn Bookmarks,

    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    bonsai_git_mapping: dyn BonsaiGitMapping,

    #[facet]
    bonsai_globalrev_mapping: dyn BonsaiGlobalrevMapping,

    #[facet]
    bonsai_svnrev_mapping: dyn BonsaiSvnrevMapping,

    #[facet]
    bonsai_p4_mapping: dyn BonsaiP4Mapping,

    #[facet]
    repo_blobstore: RepoBlobstore,

    #[facet]
    repo_derived_data: RepoDerivedData,

    #[facet]
    repo_identity: RepoIdentity,
}

#[derive(Subcommand)]
pub enum RedactionSubcommand {
    /// Create a key list using files in a changeset.
    CreateKeyList(RedactionCreateKeyListArgs),
    /// List the redacted files in a commit.
    List(RedactionListArgs),
    /// Fetch a key list from its key list id.
    FetchKeyList(RedactionFetchKeyListArgs),
    /// Sync all active prod key lists to the AWS shadow blobstore.
    SyncToAws(RedactionSyncToAwsArgs),
    /// Delete the content of enforced key lists from CAS.
    DeleteFromCas(RedactionDeleteFromCasArgs),
    /// Internal batch operation used by the AWS sync orchestrator.
    #[clap(hide = true)]
    SyncKeyListsFromJson(RedactionSyncKeyListsFromJsonArgs),
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let ctx = app.new_basic_context();

    match args.subcommand {
        RedactionSubcommand::CreateKeyList(create_args) => {
            create_key_list::create_key_list_from_commit_files(&ctx, &app, create_args).await?
        }
        RedactionSubcommand::List(list_args) => list::list(&ctx, &app, list_args).await?,
        RedactionSubcommand::FetchKeyList(fetch_args) => {
            create_key_list::fetch_key_list(&ctx, &app, fetch_args).await?
        }
        RedactionSubcommand::SyncToAws(sync_args) => {
            create_key_list::sync_all_key_lists_to_aws(&ctx, &app, sync_args).await?
        }
        RedactionSubcommand::DeleteFromCas(delete_args) => {
            delete_from_cas::delete_from_cas(&ctx, &app, delete_args).await?
        }
        RedactionSubcommand::SyncKeyListsFromJson(sync_args) => {
            create_key_list::sync_key_lists_from_json(&ctx, &app, sync_args).await?
        }
    }

    Ok(())
}
