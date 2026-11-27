/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt::Display;

use anyhow::Result;
use async_trait::async_trait;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::LoadableError;
use blobstore::Storable;
use context::CoreContext;
use edenapi_types::AnyFileContentId;
use futures_watchdog::WatchdogExt;
use mononoke_types::Alias;
use mononoke_types::BlobstoreKey;
use mononoke_types::ContentAlias;
use mononoke_types::ContentId;
use mononoke_types::hash;

/// Key for fetching - we can access with any of the supported key types
#[derive(Debug, Copy, Clone)]
pub enum FetchKey {
    Canonical(ContentId),
    Aliased(Alias),
}

impl FetchKey {
    pub async fn content_id<B: KeyedBlobstore>(
        &self,
        ctx: &CoreContext,
        blobstore: &B,
    ) -> Result<Option<ContentId>> {
        match self.load(ctx, blobstore).await {
            Ok(content_id) => Ok(Some(content_id)),
            Err(LoadableError::Missing(_)) => Ok(None),
            Err(LoadableError::Error(err)) => Err(err),
        }
    }
}

impl Display for FetchKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Canonical(content_id) => f.write_fmt(format_args!("{content_id:?}")),
            Self::Aliased(alias) => f.write_fmt(format_args!("{alias:?}")),
        }
    }
}

impl From<ContentId> for FetchKey {
    fn from(content_id: ContentId) -> Self {
        FetchKey::Canonical(content_id)
    }
}

impl From<Alias> for FetchKey {
    fn from(alias: Alias) -> Self {
        FetchKey::Aliased(alias)
    }
}

impl From<hash::Sha256> for FetchKey {
    fn from(hash: hash::Sha256) -> Self {
        FetchKey::Aliased(Alias::Sha256(hash))
    }
}

impl From<hash::Sha1> for FetchKey {
    fn from(hash: hash::Sha1) -> Self {
        FetchKey::Aliased(Alias::Sha1(hash))
    }
}

impl From<hash::RichGitSha1> for FetchKey {
    fn from(hash: hash::RichGitSha1) -> Self {
        FetchKey::Aliased(Alias::GitSha1(hash.sha1()))
    }
}

impl From<hash::GitSha1> for FetchKey {
    fn from(hash: hash::GitSha1) -> Self {
        FetchKey::Aliased(Alias::GitSha1(hash))
    }
}

impl From<hash::Blake3> for FetchKey {
    fn from(hash: hash::Blake3) -> Self {
        FetchKey::Aliased(Alias::SeededBlake3(hash))
    }
}

impl From<AnyFileContentId> for FetchKey {
    fn from(id: AnyFileContentId) -> Self {
        match id {
            AnyFileContentId::ContentId(id) => Self::from(ContentId::from(id)),
            AnyFileContentId::Sha1(id) => Self::from(hash::Sha1::from(id)),
            AnyFileContentId::Sha256(id) => Self::from(hash::Sha256::from(id)),
            AnyFileContentId::SeededBlake3(id) => Self::from(hash::Blake3::from(id)),
        }
    }
}

impl FetchKey {
    pub fn blobstore_key(&self) -> String {
        match self {
            Self::Canonical(cid) => cid.blobstore_key(),
            Self::Aliased(alias) => alias.blobstore_key(),
        }
    }
}

#[async_trait]
impl Loadable for FetchKey {
    type Value = ContentId;

    /// Return the canonical ID for a key. It doesn't check if the corresponding content actually
    /// exists:
    /// - When called with content_id, it doesn't check the content id is stored in the blobstore
    /// - It is possible for an alias to exist before the ID if there was an interrupted store
    /// operation.
    async fn load<'a, B: KeyedBlobstore>(
        &'a self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Value, LoadableError> {
        match self {
            FetchKey::Canonical(content_id) => Ok(*content_id),
            FetchKey::Aliased(alias) => {
                alias
                    .load(ctx, blobstore)
                    .watched()
                    .with_max_poll(blobstore::BLOBSTORE_MAX_POLL_TIME_MS)
                    .await
            }
        }
    }
}

pub struct AliasBlob(pub Alias, pub ContentAlias);

#[async_trait]
impl Storable for AliasBlob {
    type Key = ();

    async fn store<'a, B: KeyedBlobstore>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Key> {
        blobstore
            .put(ctx, self.0.blobstore_key(), self.1.into_blob())
            .await
    }
}
