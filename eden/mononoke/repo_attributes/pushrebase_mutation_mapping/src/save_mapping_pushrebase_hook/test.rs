/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::Bookmarks;
use borrowed::borrowed;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use context::CoreContext;
use fbinit::FacebookInit;
use filestore::FilestoreConfig;
use maplit::hashset;
use metaconfig_types::PushrebaseFlags;
use mononoke_macros::mononoke;
use mononoke_types_mocks::repo;
use pushrebase::do_pushrebase_bonsai;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedData;
use repo_identity::RepoIdentity;
use repo_identity::RepoIdentityRef;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;
use tests_utils::bookmark;

use super::SaveMappingPushrebaseHook;
use crate::get_prepushrebase_ids;

#[facet::container]
#[derive(Clone)]
struct Repo {
    #[facet]
    commit_graph: CommitGraph,

    #[facet]
    commit_graph_writer: dyn CommitGraphWriter,

    #[facet]
    repo_identity: RepoIdentity,

    #[facet]
    repo_blobstore: RepoBlobstore,

    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    repo_derived_data: RepoDerivedData,

    #[facet]
    bookmarks: dyn Bookmarks,

    #[facet]
    filestore_config: FilestoreConfig,
}

#[mononoke::fbinit_test]
async fn pushrebase_saves_mapping(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);

    let mut repo_factory = TestRepoFactory::new(fb)?;
    let repo: Repo = repo_factory.with_id(repo::REPO_ONE).build().await?;

    borrowed!(ctx, repo);

    let root = CreateCommitContext::new_root(ctx, repo).commit().await?;

    let master = bookmark(ctx, repo, "master").set_to(root).await?;
    let main = bookmark(ctx, repo, "main").set_to(root).await?;

    let cs = CreateCommitContext::new(ctx, repo, vec![root])
        .commit()
        .await?
        .load(ctx, repo.repo_blobstore())
        .await?;

    let hooks = [SaveMappingPushrebaseHook::new(repo.repo_identity().id())];

    // Pushrebase the same commit onto different bookmarks that are pointing to
    // the same commit (root).
    do_pushrebase_bonsai(
        ctx,
        repo,
        &PushrebaseFlags {
            rewritedates: false,
            ..Default::default()
        },
        &master,
        &hashset![cs.clone()],
        &hooks,
        None,
    )
    .await?;

    let rebased = do_pushrebase_bonsai(
        ctx,
        repo,
        &PushrebaseFlags {
            rewritedates: false,
            ..Default::default()
        },
        &main,
        &hashset![cs.clone()],
        &hooks,
        None,
    )
    .await?
    .head;

    let prepushrebase_ids = get_prepushrebase_ids(
        ctx,
        &repo_factory.metadata_db().read_connection,
        repo.repo_identity().id(),
        rebased,
    )
    .await?;

    assert_eq!(
        prepushrebase_ids,
        vec![cs.get_changeset_id(), cs.get_changeset_id()]
    );

    Ok(())
}
