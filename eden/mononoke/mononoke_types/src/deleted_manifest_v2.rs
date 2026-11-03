/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;

use anyhow::Result;
use blobstore::KeyedBlobstore;
use blobstore::Storable;
use bytes::Bytes;
use context::CoreContext;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream::BoxStream;

use crate::MPathElement;
use crate::ThriftConvert;
use crate::blob::Blob;
use crate::blob::BlobstoreValue;
use crate::blob::DeletedManifestV2Blob;
use crate::deleted_manifest_common::DeletedManifestCommon;
use crate::sharded_map::MapValue;
use crate::sharded_map::ShardedMapNode;
use crate::sharded_map::ShardedTraversalOutput;
use crate::thrift;
use crate::typed_hash::BlobstoreKey;
use crate::typed_hash::ChangesetId;
use crate::typed_hash::DeletedManifestV2Context;
use crate::typed_hash::DeletedManifestV2Id;
use crate::typed_hash::IdContext;
pub use crate::typed_hash::ShardedMapNodeDMv2Id;

#[allow(clippy::doc_lazy_continuation)]
/// Deleted Manifest is a data structure that tracks deleted files and commits where they
/// were deleted. This manifest was designed in addition to Unodes to make following file history
/// across deletions possible.
///
/// Both directories and files are represented by the same data structure, which consists of:
/// * `optional<linknode>`: if set, a changeset where this path was deleted
/// * subentries: a map from base name to the deleted manifest for this path
///
/// Even though the manifest tracks only deleted paths, it will still have entries for the
/// existing directories where files were deleted. Optional field `linknode` indicates whether the
/// path still exists (not set) or it was deleted.
///
/// Q&A
///
/// Why the manifest has same data structure for files and directories?
///
/// Deleted manifest doesn't differ files from directories, because any file path can be
/// reincarnated after the deletion as a directory and vice versa. The manifest doesn't need
/// to know whether the path is a directory or a file, the only important information is "if the
/// path was deleted, which changeset did it?"
///
/// Why we don't keep a path_hash even though it provides uniqueness of entries?
///
/// The deleted manifest entry doesn't have a path_hash, that means the entries are identical
/// for different files deleted in the same commit. This is fine as soon as we don't care about the
/// the uniqueness, but about the fact that the files were deleted. So directory entry will have
/// links to the same entry for different deleted files.
/// However, if one of such files is recreated as a directory, we anyway create a new entry for it:
/// manifest entries are immutable.
///
/// How we derive deleted manifest?
///
/// Assuming we have a computed deleted manifests for all the current commits, for a new
/// changeset:
/// 1. For each deleted file create a new manifest entry with a linknode to the changeset, where
///    file was deleted.
/// 2. For each recreated file remove this file from the manifest.
/// 3. Remove directory manifest if it still exists and don’t have deleted children anymore.
/// 4. Create new manifest nodes recursively.
/// 5. Finalize the conversion by recording the mapping from changeset id to the root deleted
///    files manifest hash.
///
/// Where does point a linknode for a file that was marked as deleted in a merge commit?
///
/// Linknode will point to the merge commit itself, if the file deletion was made in some of the
/// parents and was accepted in merge commit.
///
/// How do we perform tracking file history across deletions?
///
/// Assume we have a commit graph, where:
/// * - file was deleted
/// o - file exists
///
///   o A
///   |
///   * B
///  / \
/// *   * C
/// |   |
/// : D o E
/// |   |
/// o F :
///
/// 1. Check deleted manifest from node B: the file was deleted with linknode to B.
/// 2. Need to consider all ancestors of B:
///    * check unodes for the parents: if the file exists,
///      traverse the history
///    * if not, check the deleted manifest whether the file
///      existed and was deleted
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeletedManifestV2 {
    linknode: Option<ChangesetId>,
    pub subentries: ShardedMapNode<DeletedManifestV2Id>,
}

impl MapValue for DeletedManifestV2Id {
    type Id = crate::typed_hash::ShardedMapNodeDMv2Id;
    type Context = crate::typed_hash::ShardedMapNodeDMv2Context;
}

#[async_trait::async_trait]
impl DeletedManifestCommon for DeletedManifestV2 {
    type Id = DeletedManifestV2Id;

    async fn copy_and_update_subentries(
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
        current: Option<Self>,
        linknode: Option<ChangesetId>,
        subentries_to_update: BTreeMap<MPathElement, Option<Self::Id>>,
    ) -> Result<Self> {
        let subentries = current.map(|mf| mf.subentries).unwrap_or_default();
        let subentries = subentries
            .update(
                ctx,
                blobstore,
                subentries_to_update
                    .into_iter()
                    .map(|(k, v)| (Bytes::copy_from_slice(k.as_ref()), v))
                    .collect(),
                |_| (),
            )
            .await?;
        Ok(Self::new(linknode, subentries))
    }

    fn linknode(&self) -> Option<&ChangesetId> {
        self.linknode.as_ref()
    }

    fn is_empty(&self) -> bool {
        self.subentries.is_empty()
    }

    fn id(&self) -> Self::Id {
        *self.clone().into_blob().id()
    }

    async fn lookup(
        &self,
        ctx: &CoreContext,
        blobstore: &impl KeyedBlobstore,
        basename: &MPathElement,
    ) -> Result<Option<Self::Id>> {
        self.subentries
            .lookup(ctx, blobstore, basename.as_ref())
            .await
    }

    fn into_subentries<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
    ) -> BoxStream<'a, Result<(MPathElement, Self::Id)>> {
        self.subentries
            .into_entries(ctx, blobstore)
            .and_then(|(k, v)| async move { anyhow::Ok((MPathElement::from_smallvec(k)?, v)) })
            .boxed()
    }
}

impl ThriftConvert for DeletedManifestV2 {
    const NAME: &'static str = "DeletedManifestV2";

    type Thrift = thrift::deleted_manifest::DeletedManifestV2;
    fn from_thrift(t: thrift::deleted_manifest::DeletedManifestV2) -> Result<DeletedManifestV2> {
        Ok(Self {
            linknode: t.linknode.map(ChangesetId::from_thrift).transpose()?,
            subentries: ShardedMapNode::from_thrift(t.subentries)?,
        })
    }

    fn into_thrift(self) -> thrift::deleted_manifest::DeletedManifestV2 {
        thrift::deleted_manifest::DeletedManifestV2 {
            linknode: self.linknode.map(ChangesetId::into_thrift),
            subentries: self.subentries.into_thrift(),
        }
    }
}

impl DeletedManifestV2 {
    pub fn new(
        linknode: Option<ChangesetId>,
        subentries: ShardedMapNode<DeletedManifestV2Id>,
    ) -> Self {
        Self {
            linknode,
            subentries,
        }
    }
    pub fn into_sharded_subentries<'a>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a impl KeyedBlobstore,
    ) -> BoxStream<'a, Result<ShardedTraversalOutput<'a, DeletedManifestV2Id>>> {
        self.subentries.into_sharded_entries(ctx, blobstore).boxed()
    }
}

impl BlobstoreValue for DeletedManifestV2 {
    type Key = DeletedManifestV2Id;

    fn into_blob(self) -> DeletedManifestV2Blob {
        let data = self.into_bytes();
        let id = DeletedManifestV2Context::id_from_data(&data);
        Blob::new(id, data)
    }

    fn from_blob(blob: Blob<Self::Key>) -> Result<Self> {
        Self::from_bytes(blob.data())
    }
}

#[async_trait::async_trait]
impl Storable for DeletedManifestV2 {
    type Key = DeletedManifestV2Id;

    async fn store<'a, B: KeyedBlobstore>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Key> {
        let blob = self.into_blob();
        let id = blob.id().clone();
        blobstore.put(ctx, id.blobstore_key(), blob.into()).await?;
        Ok(id)
    }
}
