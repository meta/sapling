/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Result;
use changeset_info::ChangesetInfo;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::TryStreamExt;
use futures_lazy_shared::LazyShared;
use mononoke_macros::mononoke;
use mononoke_types::BonsaiChangeset;
use mononoke_types::MPath;
use mononoke_types::SubtreeChange;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;

use crate::ChangedPathsRestrictionOptions;
use crate::changeset::ChangesetContext;
use crate::repo::Repo;
use crate::repo::RepoContext;

async fn parent_context(fb: FacebookInit) -> Result<ChangesetContext<Repo>> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = TestRepoFactory::new(fb)?.build().await?;
    let parent_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file", "content")
        .commit()
        .await?;
    let repo_ctx = RepoContext::new_test(ctx, Arc::new(repo)).await?;
    Ok(ChangesetContext::new(repo_ctx, parent_id))
}

fn with_deep_sources(
    parent: &ChangesetContext<Repo>,
    bonsai: BonsaiChangeset,
    count: usize,
) -> Result<BonsaiChangeset> {
    let mut bonsai = bonsai.into_mut();
    bonsai.parents = vec![parent.id()];
    for i in 0..count {
        bonsai.subtree_changes.insert(
            MPath::new(format!("destination{i}"))?,
            SubtreeChange::deep_copy(MPath::new("missing-source")?, parent.id()),
        );
    }
    bonsai.freeze()
}

#[mononoke::fbinit_test]
async fn net_endpoint_comparison_keeps_subtree_sources_uncapped(fb: FacebookInit) -> Result<()> {
    let parent = parent_context(fb).await?;
    let bonsai = with_deep_sources(&parent, parent.bonsai_changeset().await?, 10_001)?;
    let sources = parent.restriction_subtree_copy_sources(Some(&bonsai));
    for destination in [0, 10_000] {
        let (source, source_path, replacement) = ChangesetContext::get_subtree_copy_source(
            &sources,
            &parent,
            &MPath::new(format!("destination{destination}/file"))?,
        )?;
        assert_eq!(source.id(), parent.id());
        assert_eq!(source_path, MPath::new("missing-source/file")?);
        assert!(replacement.is_some());
    }

    let mut tip = ChangesetContext::new(parent.repo_ctx().clone(), bonsai.get_changeset_id());
    tip.changeset_info = LazyShared::new_ready(Ok(ChangesetInfo::new(tip.id(), bonsai.clone())));
    tip.bonsai_changeset = LazyShared::new_ready(Ok(bonsai));
    tip.root_content_manifest_id =
        LazyShared::new_ready(Ok(parent.root_content_manifest_id().await?));
    let changes = tip
        .changed_paths_restriction_info(
            None,
            None,
            ChangedPathsRestrictionOptions {
                check_permissions: false,
                include_permission_changes: true,
                include_subtree_source_changes: false,
            },
        )
        .await?
        .try_collect::<Vec<_>>()
        .await?;
    assert!(
        changes.is_empty(),
        "identical endpoints must not inspect nonexistent copy sources"
    );
    Ok(())
}
