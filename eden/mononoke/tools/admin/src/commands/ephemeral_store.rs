/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod cleanup;
mod delete;
mod info;
mod list;

use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use cleanup::EphemeralStoreCleanUpArgs;
use delete::EphemeralStoreDeleteArgs;
use ephemeral_blobstore::EphemeralBlobstoreError;
use ephemeral_blobstore::RepoEphemeralStore;
use info::EphemeralStoreInfoArgs;
use list::EphemeralStoreListArgs;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArgs;
use repo_identity::RepoIdentity;

/// Examine and maintain the contents of the ephemeral store.
#[derive(Parser)]
pub struct CommandArgs {
    /// The repository name or ID. Any changesets provided for
    /// subcommands will use this repoID for scoping.
    #[clap(flatten)]
    repo: RepoArgs,

    /// The subcommand for ephemeral store.
    #[clap(subcommand)]
    subcommand: EphemeralStoreSubcommand,
}

#[facet::container]
pub struct Repo {
    #[facet]
    repo_ephemeral_store: RepoEphemeralStore,
    #[facet]
    repo_identity: RepoIdentity,
}

#[derive(Subcommand)]
pub enum EphemeralStoreSubcommand {
    /// Cleanup expired bubbles within the ephemeral store.
    Cleanup(EphemeralStoreCleanUpArgs),
    /// Delete specific bubbles within the ephemeral store, whether or not
    /// they have expired.
    Delete(EphemeralStoreDeleteArgs),
    /// Describe metadata associated with a bubble within the ephemeral store.
    Info(EphemeralStoreInfoArgs),
    /// List out the keys of the blobs in a bubble within the ephemeral store.
    List(EphemeralStoreListArgs),
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let ctx = app.new_basic_context();
    let is_delete = matches!(args.subcommand, EphemeralStoreSubcommand::Delete(_));
    let repo: Repo = app.open_repo(&args.repo).await?;

    let subcommand_result = match args.subcommand {
        EphemeralStoreSubcommand::Cleanup(cleanup_args) => {
            cleanup::clean_bubbles(&ctx, &repo, cleanup_args).await
        }
        EphemeralStoreSubcommand::Delete(delete_args) => {
            delete::delete_bubbles(&ctx, &repo, delete_args).await
        }
        EphemeralStoreSubcommand::Info(info_args) => {
            info::bubble_info(&ctx, &repo, info_args).await
        }
        EphemeralStoreSubcommand::List(list_args) => list::list_keys(&ctx, &repo, list_args).await,
    };
    match subcommand_result {
        Err(e) => match e.downcast_ref::<EphemeralBlobstoreError>() {
            // If this repo does not have an ephemeral store, then this is a no-op,
            // except for a delete, which must not report success when it could
            // not look anything up
            Some(EphemeralBlobstoreError::NoEphemeralBlobstore(repo_id)) if !is_delete => {
                println!("Ephemeral store doesn't exist for repo {repo_id}, exiting.");
                Ok(())
            }
            // For any other error, return as-is
            _ => Err(e),
        },
        _ => Ok(()),
    }
}
