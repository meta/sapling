/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use async_trait::async_trait;
use blobstore::Loadable;

use crate::DeletedManifestV2Id;
use crate::audit::LoadError;
use crate::audit::TaggedContext;
use crate::audit::Validatable;
use crate::deleted_manifest_v2::DeletedManifestV2;
use crate::deleted_manifest_v2::ShardedMapNodeDMv2Id;
use crate::sharded_map::ShardedMapNode;

#[async_trait]
impl<R: Send + Sync + 'static> Validatable<R> for DeletedManifestV2Id {
    const NAME: &'static str = "DeletedManifestV2";

    type Loaded = DeletedManifestV2;

    async fn load(&self, ctx: &TaggedContext, _repo: &R) -> Result<DeletedManifestV2, LoadError> {
        Ok(Loadable::load(self, ctx, ctx.blobstore()).await?)
    }
}

#[async_trait]
impl<R: Send + Sync + 'static> Validatable<R> for ShardedMapNodeDMv2Id {
    const NAME: &'static str = "DeletedManifestV2MapNode";

    type Loaded = ShardedMapNode<DeletedManifestV2Id>;

    async fn load(
        &self,
        ctx: &TaggedContext,
        _repo: &R,
    ) -> Result<ShardedMapNode<DeletedManifestV2Id>, LoadError> {
        Ok(Loadable::load(self, ctx, ctx.blobstore()).await?)
    }
}
