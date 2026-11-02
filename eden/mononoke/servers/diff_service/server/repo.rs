/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Slim repo facet container for the diff service.
//!
//! Contains only the attributes reachable from the diff/commit_compare code
//! paths, which is a strict subset of `mononoke_api::Repo`.

use std::sync::Arc;

use acl_regions::AclRegions;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use bonsai_p4_mapping::BonsaiP4Mapping;
use bonsai_svnrev_mapping::BonsaiSvnrevMapping;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use ephemeral_blobstore::Bubble;
use ephemeral_blobstore::RepoEphemeralStore;
use metaconfig_types::RepoConfig;
use mononoke_api::RepoWithBubble;
use mutable_renames::MutableRenames;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentity;
use repo_permission_checker::RepoPermissionChecker;
use restricted_paths::RestrictedPaths;

#[facet::container]
#[derive(Clone)]
pub struct Repo {
    #[facet]
    pub repo_identity: RepoIdentity,

    #[facet]
    pub repo_config: RepoConfig,

    #[facet]
    pub repo_blobstore: RepoBlobstore,

    #[facet]
    pub repo_derived_data: RepoDerivedData,

    #[facet]
    pub repo_permission_checker: dyn RepoPermissionChecker,

    #[facet]
    pub acl_regions: dyn AclRegions,

    #[facet]
    pub restricted_paths: RestrictedPaths,

    #[facet]
    pub repo_ephemeral_store: RepoEphemeralStore,

    #[facet]
    pub mutable_renames: MutableRenames,

    #[facet]
    pub commit_graph: CommitGraph,

    #[facet]
    pub commit_graph_writer: dyn CommitGraphWriter,

    #[facet]
    pub bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    pub bonsai_git_mapping: dyn BonsaiGitMapping,

    #[facet]
    pub bonsai_globalrev_mapping: dyn BonsaiGlobalrevMapping,

    #[facet]
    pub bonsai_svnrev_mapping: dyn BonsaiSvnrevMapping,

    #[facet]
    pub bonsai_p4_mapping: dyn BonsaiP4Mapping,
}

impl RepoWithBubble for Repo {
    fn with_bubble(&self, bubble: Bubble) -> Self {
        let repo_blobstore = Arc::new(bubble.wrap_repo_blobstore(self.repo_blobstore().clone()));
        let commit_graph = Arc::new(bubble.repo_commit_graph(self));
        let commit_graph_writer = bubble.repo_commit_graph_writer(self);
        let repo_derived_data = Arc::new(self.repo_derived_data().for_bubble(bubble.clone()));
        let restricted_paths = Arc::new(self.restricted_paths.for_bubble(bubble));

        Self {
            repo_blobstore,
            commit_graph,
            commit_graph_writer,
            repo_derived_data,
            restricted_paths,
            ..self.clone()
        }
    }
}
