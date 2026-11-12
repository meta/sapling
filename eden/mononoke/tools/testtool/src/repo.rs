/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::Bookmarks;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use filestore::FilestoreConfig;
use phases::Phases;
use repo_blobstore::RepoBlobstore;
use repo_derived_data::RepoDerivedData;
use repo_identity::RepoIdentity;

#[facet::container]
#[derive(Clone)]
pub(crate) struct Repo {
    #[facet]
    repo_derived_data: RepoDerivedData,

    #[facet]
    repo_blobstore: RepoBlobstore,

    #[facet]
    filestore_config: FilestoreConfig,

    #[facet]
    commit_graph: CommitGraph,

    #[facet]
    commit_graph_writer: dyn CommitGraphWriter,

    #[facet]
    repo_identity: RepoIdentity,

    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    bookmarks: dyn Bookmarks,

    #[facet]
    phases: dyn Phases,
}
