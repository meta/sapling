/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Tests of `repo_derived_data`'s audit `Mapping`, whose crate is below
//! `test_repo_factory`.

use anyhow::Result;
use context::CoreContext;
use deleted_manifest::RootDeletedManifestV2Id;
use fbinit::FacebookInit;
use mononoke_macros::mononoke;
use mononoke_types::audit::StoredObject;
use repo_derived_data::audit::Mapping;
use tests_utils::CreateCommitContext;
use tests_utils::audit::load_ctx;

use crate::test_utils::TestRepo;
use crate::test_utils::test_repo;

#[mononoke::fbinit_test]
async fn absent_mapping_fails_validation_as_missing(fb: FacebookInit) -> Result<()> {
    // Given a stored mapping object for a commit that was never derived
    let ctx = CoreContext::test_mock(fb);
    let (repo, _blobstore) = test_repo(fb).await?;
    let a = CreateCommitContext::new_root(&ctx, &*repo)
        .add_file("a", "a")
        .commit()
        .await?;
    let mapping = StoredObject::<TestRepo>::new(Mapping::<RootDeletedManifestV2Id>::new(a));

    // When it is loaded like any other enumerated object
    let error = mapping
        .validate(&load_ctx(&ctx, &*repo), &*repo, false)
        .await
        .expect_err("A's deleted manifest was never derived");

    // Then the absence is a failure, not the benign `not_derived` outcome
    assert_eq!(error.class(), "missing");
    Ok(())
}
