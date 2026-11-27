/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::anyhow;
use async_stream::try_stream;
use async_trait::async_trait;
use blobstore::Loadable;
use futures::stream;
use futures::stream::StreamExt;

use crate::Alias;
use crate::BlobstoreKey;
use crate::BonsaiChangeset;
use crate::ChangesetId;
use crate::ContentId;
use crate::ContentMetadataV2;
use crate::ContentMetadataV2Id;
use crate::DerivableType;
use crate::audit::CommitInfo;
use crate::audit::LoadContext;
use crate::audit::LoadError;
use crate::audit::Notice;
use crate::audit::ObjectId;
use crate::audit::RecomputeContentMetadata;
use crate::audit::StoredObject;
use crate::audit::TaggedContext;
use crate::audit::Validatable;
use crate::audit::WalkError;
use crate::audit::WalkStream;
use crate::audit::Walkable;
use crate::audit::file_content::StoredMetadata;
use crate::audit::file_content::load_stored_metadata;

#[async_trait]
impl<R: Send + Sync + 'static> Validatable<R> for ChangesetId {
    const NAME: &'static str = "Changeset";

    type Loaded = BonsaiChangeset;

    async fn load(&self, ctx: &TaggedContext, _repo: &R) -> Result<BonsaiChangeset, LoadError> {
        Ok(Loadable::load(self, ctx, ctx.blobstore()).await?)
    }

    fn storage_key(&self, _repo: &R) -> Option<String> {
        Some(self.blobstore_key())
    }
}

/// The core family: the changeset, the content of every file its file
/// changes add or modify, each once, and each content's metadata and alias
/// mappings. A merge's file changes also list each path where it keeps one
/// parent's file, so that content is emitted again: core reads no manifest to
/// compare with its parents. The metadata must name its content, and each
/// alias mapping must map to it. Absent metadata, and metadata in the format
/// stored before spring 2023, are recomputed from the content and emitted
/// with a [`Notice`], so they are counted, not failed; the content's seeded
/// Blake3 alias is then not checked, only the other three.
impl<R: RecomputeContentMetadata + Send + Sync + 'static> Walkable<R> for BonsaiChangeset {
    const NAME: &'static str = "core";
    const DERIVED_TYPE: Option<DerivableType> = None;

    fn walk_new(
        ctx: LoadContext,
        repo: Arc<R>,
        commit: Arc<CommitInfo>,
        max_loads: NonZeroUsize,
    ) -> WalkStream<R> {
        try_stream! {
            let id = ObjectId::of::<R, _>(&commit.changeset_id);
            let changeset = Validatable::<R>::load(
                &commit.changeset_id,
                &ctx.tag(id.clone()),
                &*repo,
            )
            .await
            .map_err(|error| WalkError::load(id, error))?;
            // A content added at several paths is checked once.
            let mut seen = HashSet::new();
            let contents = changeset
                .simplified_file_changes()
                .filter_map(|(_path, change)| Some(change?.content_id()))
                .filter(|content| seen.insert(*content))
                .collect::<Vec<_>>();
            yield StoredObject::preloaded(commit.changeset_id, changeset);
            // Each content's aliases are named by its metadata, so the
            // metadata is loaded here rather than by the runner.
            let mut contents = stream::iter(contents)
                .map(|content| {
                    let ctx = &ctx;
                    let repo = &*repo;
                    async move { (content, load_metadata(ctx, repo, content).await) }
                })
                .buffered(max_loads.get());
            while let Some((content, metadata)) = contents.next().await {
                yield StoredObject::new(content);
                let (metadata, notice) = metadata?;
                // Contents whose metadata had to be recomputed may never have
                // had a seeded Blake3 alias, which was stored together with
                // current metadata.
                let seeded_blake3 = notice
                    .is_none()
                    .then_some(Alias::SeededBlake3(metadata.seeded_blake3));
                let aliases = [
                    Alias::Sha1(metadata.sha1),
                    Alias::Sha256(metadata.sha256),
                    Alias::GitSha1(metadata.git_sha1.sha1()),
                ]
                .into_iter()
                .chain(seeded_blake3);
                yield StoredObject::preloaded(ContentMetadataV2Id::from(content), metadata)
                    .with_notice(notice);
                for alias in aliases {
                    yield StoredObject::expecting(alias, content);
                }
            }
        }
        .boxed()
    }
}

/// Loads the metadata of a content the commit added, with what was tolerated
/// to get it. Absent metadata, and metadata in the format stored before
/// spring 2023, are recomputed from the content, as production readers do;
/// an old record must hold the recomputed hashes, and nothing is stored. When
/// the content cannot be read to recompute it, the content's failure is
/// reported, as Walker does.
async fn load_metadata<R: RecomputeContentMetadata + Send + Sync + 'static>(
    ctx: &LoadContext,
    repo: &R,
    content: ContentId,
) -> Result<(ContentMetadataV2, Option<Notice>), WalkError> {
    let metadata_id = ContentMetadataV2Id::from(content);
    let id = ObjectId::of::<R, _>(&metadata_id);
    let loaded = load_stored_metadata(&metadata_id, &ctx.tag(id.clone())).await;
    match loaded {
        Ok(StoredMetadata::Current(metadata)) if metadata.content_id == content => {
            Ok((metadata, None))
        }
        Ok(StoredMetadata::Current(metadata)) => Err(WalkError::load(
            id,
            LoadError::Unreadable(anyhow!(
                "the metadata names content {}, not {content}",
                metadata.content_id
            )),
        )),
        Ok(StoredMetadata::OldFormat(old)) => {
            let metadata = recompute_metadata(ctx, repo, content).await?;
            old.check(&metadata)
                .map_err(|error| WalkError::load(id, error))?;
            Ok((metadata, Some(Notice::OldFormatContentMetadata)))
        }
        Err(LoadError::Missing(_)) => {
            let metadata = recompute_metadata(ctx, repo, content).await?;
            Ok((metadata, Some(Notice::AbsentContentMetadata)))
        }
        Err(error) => Err(WalkError::load(id, error)),
    }
}

/// Recomputes a content's metadata, its reads tagged as the content's. A
/// failure is reported against the content: when the computation fails other
/// than on the content's own blob, the content is loaded again to tell an
/// absent chunk, which is missing, from other failures.
async fn recompute_metadata<R: RecomputeContentMetadata + Send + Sync + 'static>(
    ctx: &LoadContext,
    repo: &R,
    content: ContentId,
) -> Result<ContentMetadataV2, WalkError> {
    let content_id = ObjectId::of::<R, _>(&content);
    let tagged = ctx.tag(content_id.clone());
    let error = match repo.recompute_content_metadata(&tagged, content).await {
        Ok(metadata) => return Ok(metadata),
        Err(error @ LoadError::Missing(_)) => return Err(WalkError::load(content_id, error)),
        Err(error) => error,
    };
    match <ContentId as Validatable<R>>::load(&content, &tagged, repo).await {
        Ok(_) => Err(WalkError::load(content_id, error)),
        Err(content_error) => Err(WalkError::load(content_id, content_error)),
    }
}
