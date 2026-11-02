/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::Bookmarks;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use filestore::FilestoreConfig;
use git_source_of_truth::GitSourceOfTruthConfig;
use hook_manager::HookManager;
use metaconfig_types::RepoConfig;
use phases::Phases;
use pushrebase_mutation_mapping::PushrebaseMutationMapping;
use repo_blobstore::RepoBlobstore;
use repo_bookmark_attrs::RepoBookmarkAttrs;
use repo_cross_repo::RepoCrossRepo;
use repo_derived_data::RepoDerivedData;
use repo_identity::RepoIdentity;
use repo_lock::RepoLock;
use repo_permission_checker::RepoPermissionChecker;

#[facet::container]
#[derive(Clone)]
pub struct Repo {
    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    bonsai_git_mapping: dyn BonsaiGitMapping,

    #[facet]
    bonsai_globalrev_mapping: dyn BonsaiGlobalrevMapping,

    #[facet]
    bookmarks: dyn Bookmarks,

    #[facet]
    phases: dyn Phases,

    #[facet]
    pushrebase_mutation_mapping: dyn PushrebaseMutationMapping,

    #[facet]
    repo_bookmark_attrs: RepoBookmarkAttrs,

    #[facet]
    repo_config: RepoConfig,

    #[facet]
    repo_derived_data: RepoDerivedData,

    #[facet]
    repo_blobstore: RepoBlobstore,

    #[facet]
    filestore_config: FilestoreConfig,

    #[facet]
    repo_cross_repo: RepoCrossRepo,

    #[facet]
    repo_identity: RepoIdentity,

    #[facet]
    repo_permission_checker: dyn RepoPermissionChecker,

    #[facet]
    repo_lock: dyn RepoLock,

    #[facet]
    commit_graph: CommitGraph,

    #[facet]
    commit_graph_writer: dyn CommitGraphWriter,

    #[facet]
    git_source_of_truth_config: dyn GitSourceOfTruthConfig,

    #[facet]
    hook_manager: HookManager,
}
