/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod acl_parent_trees;
mod delete_digests;
mod file_info;
mod tree_info;
mod upload;

use acl_parent_trees::CasStoreAclParentTreesArgs;
use anyhow::Result;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use bonsai_p4_mapping::BonsaiP4Mapping;
use bonsai_svnrev_mapping::BonsaiSvnrevMapping;
use bookmarks::BookmarkUpdateLog;
use bookmarks::Bookmarks;
use clap::Parser;
use clap::Subcommand;
use commit_graph::CommitGraph;
use delete_digests::CasStoreDeleteDigestsArgs;
use file_info::CasStoreFileInfoArgs;
use metaconfig_types::RepoConfig;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArgs;
use repo_blobstore::RepoBlobstore;
use repo_derived_data::RepoDerivedData;
use repo_identity::RepoIdentity;
use restricted_paths::RestrictedPaths;
use tree_info::CasStoreTreeInfoArgs;
use upload::CasStoreUploadArgs;

/// Examine and maintain the contents of the cas store.
#[derive(Parser)]
pub struct CommandArgs {
    /// The repository name or ID. Any changesets provided for
    /// subcommands will use this repoID for scoping.
    #[clap(flatten)]
    repo: RepoArgs,

    /// The subcommand for cas store.
    #[clap(subcommand)]
    subcommand: CasStoreSubcommand,
}

#[facet::container]
#[derive(Clone)]
pub struct Repo {
    #[facet]
    repo_identity: RepoIdentity,

    #[facet]
    repo_config: RepoConfig,

    #[facet]
    repo_blobstore: RepoBlobstore,

    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    bonsai_git_mapping: dyn BonsaiGitMapping,

    #[facet]
    bonsai_svnrev_mapping: dyn BonsaiSvnrevMapping,

    #[facet]
    bonsai_p4_mapping: dyn BonsaiP4Mapping,

    #[facet]
    bonsai_globalrev_mapping: dyn BonsaiGlobalrevMapping,

    #[facet]
    bookmarks: dyn Bookmarks,

    #[facet]
    bookmark_update_log: dyn BookmarkUpdateLog,

    #[facet]
    commit_graph: CommitGraph,

    #[facet]
    repo_derived_data: RepoDerivedData,

    #[facet]
    restricted_paths: RestrictedPaths,
}

#[derive(Subcommand)]
pub enum CasStoreSubcommand {
    /// Show information related to CAS about a tree using its hgid.
    TreeInfo(CasStoreTreeInfoArgs),
    /// Show information related to CAS about a tree using its hgid.
    FileInfo(CasStoreFileInfoArgs),
    /// Upload a specific (augmented) tree, file or data for a given commit recursively into the cas store.
    Upload(CasStoreUploadArgs),
    /// Find the trees uploaded to the cas store that have a restriction root as a direct child.
    AclParentTrees(CasStoreAclParentTreesArgs),
    /// Delete digests from the cas store.
    DeleteDigests(CasStoreDeleteDigestsArgs),
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let ctx = app.new_basic_context();
    let repo: Repo = app.open_repo(&args.repo).await?;

    match args.subcommand {
        CasStoreSubcommand::Upload(upload_args) => {
            upload::cas_store_upload(&ctx, &repo, upload_args).await
        }
        CasStoreSubcommand::TreeInfo(tree_info_args) => {
            tree_info::tree_info(&ctx, &repo, tree_info_args).await
        }
        CasStoreSubcommand::FileInfo(file_info_args) => {
            file_info::file_info(&ctx, &repo, file_info_args).await
        }
        CasStoreSubcommand::AclParentTrees(acl_parent_trees_args) => {
            acl_parent_trees::acl_parent_trees(&ctx, &repo, acl_parent_trees_args).await
        }
        CasStoreSubcommand::DeleteDigests(delete_digests_args) => {
            delete_digests::delete_digests(&ctx, &repo, delete_digests_args).await
        }
    }
}
