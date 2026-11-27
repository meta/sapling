/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use async_stream::try_stream;
use async_trait::async_trait;
use blobstore::Loadable;
use futures::stream::StreamExt;

use crate::BonsaiChangeset;
use crate::ChangesetId;
use crate::DerivableType;
use crate::audit::CommitInfo;
use crate::audit::LoadContext;
use crate::audit::LoadError;
use crate::audit::ObjectId;
use crate::audit::StoredObject;
use crate::audit::TaggedContext;
use crate::audit::Validatable;
use crate::audit::WalkError;
use crate::audit::WalkStream;
use crate::audit::Walkable;

#[async_trait]
impl<R: Send + Sync + 'static> Validatable<R> for ChangesetId {
    const NAME: &'static str = "Changeset";

    type Loaded = BonsaiChangeset;

    async fn load(&self, ctx: &TaggedContext, _repo: &R) -> Result<BonsaiChangeset, LoadError> {
        Ok(Loadable::load(self, ctx, ctx.blobstore()).await?)
    }
}

/// The core family: the changeset and the content of every file its file
/// changes add or modify, each content once. A merge's file changes also list
/// each path where it keeps one parent's file, so that content is emitted
/// again: core reads no manifest to compare with its parents.
impl<R: Send + Sync + 'static> Walkable<R> for BonsaiChangeset {
    const NAME: &'static str = "core";
    const DERIVED_TYPE: Option<DerivableType> = None;

    fn walk_new(
        ctx: LoadContext,
        repo: Arc<R>,
        commit: Arc<CommitInfo>,
        _max_loads: NonZeroUsize,
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
            for content in contents {
                yield StoredObject::new(content);
            }
        }
        .boxed()
    }
}
