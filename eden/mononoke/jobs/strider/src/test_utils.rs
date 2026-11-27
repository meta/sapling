/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Result;
use fbinit::FacebookInit;
use memblob::Memblob;
use test_repo_factory::TestRepoFactory;
use tests_utils::BasicTestRepo;

pub(crate) async fn test_repo(fb: FacebookInit) -> Result<(Arc<BasicTestRepo>, Memblob)> {
    let blobstore = Memblob::default();
    let repo: BasicTestRepo = TestRepoFactory::new(fb)?
        .with_blobstore(Arc::new(blobstore.clone()))
        .build()
        .await?;
    Ok((Arc::new(repo), blobstore))
}
