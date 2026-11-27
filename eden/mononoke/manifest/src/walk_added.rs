/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::iter;
use std::num::NonZeroUsize;
use std::sync::Arc;

use blobstore::KeyedBlobstore;
use bounded_traversal::bounded_traversal_stream;
use cloned::cloned;
use futures::FutureExt;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use futures::stream::BoxStream;
use mononoke_types::MPath;
use mononoke_types::SortedVectorTrieMap;
use mononoke_types::audit::LoadContext;
use mononoke_types::audit::LoadError;
use mononoke_types::audit::ObjectId;
use mononoke_types::audit::StoredObject;
use mononoke_types::audit::Validatable;
use mononoke_types::audit::WalkError;

use crate::Entry;
use crate::Manifest;
use crate::ManifestComparison;
use crate::Span;
use crate::TrieMapOps;
use crate::compare_manifest;

/// A directory the commit added, with its path and the parents' directories
/// at that path.
type Directory<TreeId> = (MPath, TreeId, Vec<TreeId>);

/// Streams every directory and leaf in the tree at `root` that differs from
/// every parent tree's entry at the same path. Directories are loaded, at most
/// `max_loads` at once, and yielded with their values; leaves are yielded as
/// listed, with their paths, without loading.
///
/// The walk is depth-first, so its work list holds the unvisited
/// subdirectories along the current paths rather than a whole level of the
/// tree.
///
/// A directory the commit added fails at `load` and a parent's directory at
/// `enumerate`, each with its key, so damage is reported against the commit
/// that added it. An error does not end the stream, so stop reading at the
/// first one.
///
/// Only manifests that store each directory in one blob are accepted: for a
/// sharded manifest the comparison would also load the directory's shards
/// without reporting them, so they would go unvalidated.
///
/// Pass `ctx.blobstore()` as `blobstore`. It is a type parameter, as in
/// `compare_manifest`, because bounds naming `Arc<dyn KeyedBlobstore>` hit a
/// rustc limitation when proving the stream `Send`.
pub fn walk_added<R, Store, TreeId, M, Leaf>(
    ctx: LoadContext,
    repo: Arc<R>,
    blobstore: Store,
    root: TreeId,
    parent_roots: Vec<TreeId>,
    max_loads: NonZeroUsize,
) -> BoxStream<'static, Result<Entry<StoredObject<R>, (MPath, Leaf)>, WalkError>>
where
    R: Send + Sync + 'static,
    Store: KeyedBlobstore + 'static,
    TreeId: Validatable<R, Loaded = M>,
    M: Manifest<
            Store,
            TreeId = TreeId,
            Leaf = Leaf,
            TrieMapType = SortedVectorTrieMap<Entry<TreeId, Leaf>>,
        > + Clone
        + Send,
    Leaf: Clone + Eq + Send + Sync + 'static,
{
    let blobstore = Arc::new(blobstore);
    let init = (!parent_roots.contains(&root)).then_some((MPath::ROOT, root, parent_roots));
    bounded_traversal_stream(max_loads.get(), init, move |(path, id, parents)| {
        cloned!(ctx, repo, blobstore);
        async move {
            let (directory, leaves, directories) =
                compare_directory(&ctx, &*repo, &*blobstore, path, id, parents).await?;
            let entries =
                iter::once(Entry::Tree(directory)).chain(leaves.into_iter().map(Entry::Leaf));
            Ok::<_, WalkError>((stream::iter(entries.map(Ok)), directories))
        }
        .boxed()
    })
    .try_flatten()
    .boxed()
}

/// Loads directory `id` at `path`, which the commit added, and its parents'
/// directories at the same path, and returns it, preloaded, with the leaves
/// and directories in it that differ from every parent's entry of the same
/// name.
async fn compare_directory<R, Store, TreeId, M, Leaf>(
    ctx: &LoadContext,
    repo: &R,
    blobstore: &Store,
    path: MPath,
    id: TreeId,
    parents: Vec<TreeId>,
) -> Result<(StoredObject<R>, Vec<(MPath, Leaf)>, Vec<Directory<TreeId>>), WalkError>
where
    R: Send + Sync + 'static,
    Store: KeyedBlobstore + 'static,
    TreeId: Validatable<R, Loaded = M>,
    M: Manifest<
            Store,
            TreeId = TreeId,
            Leaf = Leaf,
            TrieMapType = SortedVectorTrieMap<Entry<TreeId, Leaf>>,
        > + Clone
        + Send,
    Leaf: Clone + Eq + Send + Sync + 'static,
{
    let object = ObjectId::of::<R, _>(&id);
    // The comparison below runs under the directory's tag, so any read it
    // makes is attributed to the directory.
    let directory_ctx = ctx.tag(object.clone());
    let manifest = Validatable::load(&id, &directory_ctx, repo)
        .await
        .map_err(|error| WalkError::load(object.clone(), error))?;
    let mut parent_manifests = Vec::new();
    for parent in parents {
        let parent_id = ObjectId::of::<R, _>(&parent);
        let parent_manifest = Validatable::load(&parent, &ctx.tag(parent_id.clone()), repo)
            .await
            .map_err(|error| WalkError::enumerate(Some(parent_id), error))?;
        parent_manifests.push(Some(parent_manifest));
    }

    // Directories in one blob are compared in memory, so these fail only on a
    // malformed manifest.
    let unreadable =
        |error| WalkError::enumerate(Some(object.clone()), LoadError::Unreadable(error));
    let mut leaves = Vec::new();
    let mut directories = Vec::new();
    let mut add = |path: MPath, entry: Entry<TreeId, Leaf>, parents: Vec<TreeId>| match entry {
        Entry::Leaf(leaf) => leaves.push((path, leaf)),
        Entry::Tree(directory) => directories.push((path, directory, parents)),
    };
    let mut comparison = compare_manifest(
        &directory_ctx,
        blobstore,
        manifest.clone(),
        parent_manifests,
    )
    .await
    .map_err(unreadable)?;
    while let Some(entry) = comparison.try_next().await.map_err(unreadable)? {
        match entry {
            ManifestComparison::Same(..) | ManifestComparison::Removed(..) => {}
            ManifestComparison::Changed(name, entry, parent_entries) => {
                let parents = parent_entries
                    .into_iter()
                    .flatten()
                    .filter_map(Entry::into_tree)
                    .collect();
                add(path.join(&name), entry, parents);
            }
            ManifestComparison::New(Span::Element(name, entry)) => {
                add(path.join(&name), entry, Vec::new())
            }
            // A range of names no parent has, such as a whole new directory, is
            // not expanded by the comparison.
            ManifestComparison::New(Span::Prefix(prefix, entries)) => {
                let mut entries = entries
                    .into_stream(&directory_ctx, blobstore)
                    .await
                    .map_err(unreadable)?;
                while let Some((suffix, entry)) = entries.try_next().await.map_err(unreadable)? {
                    let name = prefix
                        .clone()
                        .join_into_element(suffix)
                        .map_err(unreadable)?;
                    add(path.join(&name), entry, Vec::new());
                }
            }
        }
    }
    Ok((StoredObject::preloaded(id, manifest), leaves, directories))
}
