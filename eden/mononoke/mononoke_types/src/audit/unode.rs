/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use async_trait::async_trait;
use blobstore::Loadable;

use crate::FileUnodeId;
use crate::ManifestUnodeId;
use crate::audit::LoadError;
use crate::audit::TaggedContext;
use crate::audit::Validatable;
use crate::unode::FileUnode;
use crate::unode::ManifestUnode;

#[async_trait]
impl<R: Send + Sync + 'static> Validatable<R> for ManifestUnodeId {
    const NAME: &'static str = "UnodeManifest";

    type Loaded = ManifestUnode;

    async fn load(&self, ctx: &TaggedContext, _repo: &R) -> Result<ManifestUnode, LoadError> {
        Ok(Loadable::load(self, ctx, ctx.blobstore()).await?)
    }
}

#[async_trait]
impl<R: Send + Sync + 'static> Validatable<R> for FileUnodeId {
    const NAME: &'static str = "UnodeFile";

    type Loaded = FileUnode;

    async fn load(&self, ctx: &TaggedContext, _repo: &R) -> Result<FileUnode, LoadError> {
        Ok(Loadable::load(self, ctx, ctx.blobstore()).await?)
    }
}
