/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use blobstore::Blobstore;
use blobstore::BlobstoreBytes;
use blobstore::Loadable;
use blobstore::PutBehaviour;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::Bookmarks;
use bytes::Bytes;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use context::CoreContext;
use fbinit::FacebookInit;
use fbthrift::compact_protocol;
use filestore::FilestoreConfig;
use memblob::Memblob;
use metaconfig_types::RepoConfig;
use mononoke_types::BlobstoreKey;
use mononoke_types::BlobstoreValue;
use mononoke_types::ContentId;
use mononoke_types::ContentMetadataV2;
use mononoke_types::ContentMetadataV2Id;
use mononoke_types::audit::LoadError;
use mononoke_types::audit::RecomputeContentMetadata;
use mononoke_types::audit::TaggedContext;
use mononoke_types::thrift;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedData;
use repo_identity::RepoIdentity;
use repo_identity::RepoIdentityRef;
use test_repo_factory::TestRepoFactory;
use tests_utils::BasicTestRepo;

/// The shared test repo, which can also recompute content metadata.
#[facet::container]
pub(crate) struct TestRepo {
    #[delegate(
        dyn BonsaiHgMapping,
        dyn Bookmarks,
        CommitGraph,
        dyn CommitGraphWriter,
        FilestoreConfig,
        RepoBlobstore,
        RepoConfig,
        RepoDerivedData,
        RepoIdentity
    )]
    basic: BasicTestRepo,
}

#[async_trait]
impl RecomputeContentMetadata for TestRepo {
    async fn recompute_content_metadata(
        &self,
        ctx: &TaggedContext,
        content: ContentId,
    ) -> Result<ContentMetadataV2, LoadError> {
        crate::families::recompute_content_metadata(ctx, content).await
    }
}

pub(crate) async fn test_repo(fb: FacebookInit) -> Result<(Arc<TestRepo>, Memblob)> {
    let blobstore = Memblob::default();
    let repo: TestRepo = TestRepoFactory::new(fb)?
        .with_blobstore(Arc::new(blobstore.clone()))
        .build()
        .await?;
    Ok((Arc::new(repo), blobstore))
}

/// Stores `content`'s metadata again with `edit` applied to its Thrift form,
/// replacing the record the filestore stored, and returns the bytes stored.
pub(crate) async fn rewrite_metadata(
    ctx: &CoreContext,
    repo: &TestRepo,
    blobstore: &Memblob,
    content: ContentId,
    edit: impl FnOnce(&mut thrift::content::ContentMetadataV2),
) -> Result<Bytes> {
    let metadata_id = ContentMetadataV2Id::from(content);
    let metadata = Loadable::load(&metadata_id, ctx, repo.repo_blobstore()).await?;
    let mut record: thrift::content::ContentMetadataV2 =
        compact_protocol::deserialize(metadata.into_blob().data())?;
    edit(&mut record);
    let bytes = compact_protocol::serialize(&record);
    put_raw(
        ctx,
        repo,
        blobstore,
        metadata_id.blobstore_key(),
        bytes.clone(),
    )
    .await?;
    Ok(bytes)
}

/// Removes the metadata fields added in spring 2023, as metadata stored
/// before then lacks them.
pub(crate) fn remove_late_fields(record: &mut thrift::content::ContentMetadataV2) {
    record.is_generated = None;
    record.is_partially_generated = None;
    record.seeded_blake3 = None;
}

/// Replaces the bytes stored under the repo's `key`.
async fn put_raw(
    ctx: &CoreContext,
    repo: &TestRepo,
    blobstore: &Memblob,
    key: String,
    bytes: Bytes,
) -> Result<()> {
    blobstore
        .put_explicit(
            ctx,
            repo_key(repo, key),
            BlobstoreBytes::from_bytes(bytes),
            PutBehaviour::Overwrite,
        )
        .await?;
    Ok(())
}

/// The repo's storage key for `key`, as its blobstore stores it.
fn repo_key(repo: &TestRepo, key: String) -> String {
    format!("{}{}", repo.repo_identity().id().prefix(), key)
}
