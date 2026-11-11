/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::Result;
use bonsai_hg_mapping::BonsaiHgMappingEntry;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use context::CoreContext;
use ephemeral_blobstore::RepoEphemeralStoreRef;
use fbinit::FacebookInit;
use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use mercurial_types::HgChangesetId;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use test_repo_factory::TestRepoFactory;

use crate::repo::Repo;
use crate::repo::RepoWithBubble;

async fn bubble_repo_resolves_bubble_hg(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: Repo = TestRepoFactory::new(fb)?
        .with_ephemeral_store()
        .build()
        .await?;
    let hg_cs_id = HgChangesetId::from_str("1111111111111111111111111111111111111111")?;
    let bcs_id =
        ChangesetId::from_str("1111111111111111111111111111111111111111111111111111111111111111")?;

    let bubble = repo
        .repo_ephemeral_store()
        .create_bubble(&ctx, None, vec![])
        .await?;
    repo.with_bubble(bubble.clone())
        .bonsai_hg_mapping()
        .add(&ctx, BonsaiHgMappingEntry { hg_cs_id, bcs_id })
        .await?;

    let bubble_repo = repo.with_bubble(bubble);
    assert_eq!(
        bubble_repo
            .bonsai_hg_mapping()
            .get_bonsai_from_hg(&ctx, hg_cs_id)
            .await?,
        Some(bcs_id)
    );
    assert_eq!(
        bubble_repo
            .bonsai_hg_mapping()
            .get_hg_from_bonsai(&ctx, bcs_id)
            .await?,
        Some(hg_cs_id)
    );
    assert_eq!(
        repo.bonsai_hg_mapping()
            .get_bonsai_from_hg(&ctx, hg_cs_id)
            .await?,
        None,
        "the base repo never sees bubble rows"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_bubble_repo_resolves_bubble_hg(fb: FacebookInit) -> Result<()> {
    with_just_knobs_async(
        JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:enable_bubble_hg_mapping".to_string(),
            KnobVal::Bool(true),
        )])),
        bubble_repo_resolves_bubble_hg(fb).boxed(),
    )
    .await
}
