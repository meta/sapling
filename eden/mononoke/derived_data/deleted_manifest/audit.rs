/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::Result;
use async_stream::try_stream;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use context::CoreContext;
use derived_data_manager::BonsaiDerivable;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream::FuturesOrdered;
use mononoke_types::DeletedManifestV2Id;
use mononoke_types::DerivableType;
use mononoke_types::MPathElement;
use mononoke_types::audit::CommitInfo;
use mononoke_types::audit::LoadContext;
use mononoke_types::audit::LoadError;
use mononoke_types::audit::ObjectId;
use mononoke_types::audit::StoredObject;
use mononoke_types::audit::Validatable;
use mononoke_types::audit::WalkError;
use mononoke_types::audit::WalkStream;
use mononoke_types::audit::Walkable;
use mononoke_types::deleted_manifest_v2::DeletedManifestV2;
use mononoke_types::deleted_manifest_v2::ShardedMapNodeDMv2Id;
use mononoke_types::sharded_map::ShardedMapNode;
use mononoke_types::sharded_map::ShardedTraversalOutput;
use repo_derived_data::RepoDerivedDataRef;
use repo_derived_data::audit::Mapping;

use crate::RootDeletedManifestIdCommon;
use crate::RootDeletedManifestV2Id;

/// The deleted-manifest family: the commit's mapping, then every manifest node
/// that differs from all parents' nodes at the same path, and every separately
/// stored sharded-map node that no parent holds.
impl<R> Walkable<R> for RootDeletedManifestV2Id
where
    R: RepoDerivedDataRef + Send + Sync + 'static,
{
    const NAME: &'static str = "deleted-manifest";
    const DERIVED_TYPE: Option<DerivableType> = Some(Self::VARIANT);

    fn walk_new(
        ctx: LoadContext,
        repo: Arc<R>,
        commit: Arc<CommitInfo>,
        max_loads: NonZeroUsize,
    ) -> WalkStream<R> {
        try_stream! {
            let root = Mapping::<Self>::load_own(&ctx, &*repo, &commit).await?;
            let root_id = *root.id();
            yield StoredObject::preloaded(Mapping::<Self>::new(commit.changeset_id), root);

            let parent_roots = Mapping::<Self>::load_parents(&ctx, &*repo, &commit)
                .await?
                .iter()
                .map(|parent| *parent.id())
                .collect();

            // Nodes and shards have no path, so one commit's tree can hold the
            // same one at many paths: every file deleted by the commit shares one
            // leaf, and shards key entries relative to their prefix. Queueing
            // each once still emits everything it adds: equal ids have equal
            // contents, and pruning only skips what a parent holds.
            //
            // The first path to reach a node or shard decides the parents it is
            // compared with, so results are taken in the order their expansions
            // started, not as loads finish: the output then does not depend on
            // load timing. Shards are started before other queued nodes, which
            // with one load at a time is the order of a serial walk.
            let mut pending = Vec::new();
            let mut queued = HashSet::new();
            queue(&mut pending, &mut queued, root_id, parent_roots);
            let mut pending_shards = Vec::new();
            let mut expanded_shards = HashSet::new();
            let mut expanding = FuturesOrdered::new();
            loop {
                while expanding.len() < max_loads.get() {
                    let work = if let Some((prefix, shard, parents)) = pending_shards.pop() {
                        if !expanded_shards.insert(shard) {
                            continue;
                        }
                        Work::Shard(prefix, shard, parents)
                    } else if let Some((id, parents)) = pending.pop() {
                        Work::Node(id, parents)
                    } else {
                        break;
                    };
                    expanding.push_back(expand(&ctx, &*repo, work));
                }
                let Some(expanded) = expanding.next().await else {
                    break;
                };
                let (object, additions) = expanded?;
                yield object;
                let (children, shards) = additions?;
                for (child, parents) in children {
                    queue(&mut pending, &mut queued, child, parents);
                }
                // A shard that a parent also holds adds nothing, because its
                // entries are in that parent's tree. Only a node's map refers
                // to stored shards: sharded maps store only terminal nodes,
                // which hold values, so a stored shard holds no other.
                for (prefix, shard, parents) in shards {
                    if !parents.shards.contains(&shard) {
                        pending_shards.push((prefix, shard, parents));
                    }
                }
            }
        }
        .boxed()
    }
}

/// A manifest node's parents' nodes at its path, as its stored shards are
/// compared with them: each parent's id and map, and the stored shards their
/// maps refer to.
struct Parents {
    maps: Vec<(ObjectId, ShardedMapNode<DeletedManifestV2Id>)>,
    shards: HashSet<ShardedMapNodeDMv2Id>,
}

/// A manifest node or stored shard to load and compare with its parents.
enum Work {
    /// A node, with the parents' nodes at its path.
    Node(DeletedManifestV2Id, Vec<DeletedManifestV2Id>),
    /// A stored shard, with the key prefix it holds and its node's parents.
    Shard(Vec<u8>, ShardedMapNodeDMv2Id, Arc<Parents>),
}

/// What an expanded node or shard holds that its parents may not: child nodes,
/// each with the parents' nodes at its path, and stored shards, each with its
/// key prefix and its node's parents.
type Additions = (
    Vec<(DeletedManifestV2Id, Vec<DeletedManifestV2Id>)>,
    Vec<(Vec<u8>, ShardedMapNodeDMv2Id, Arc<Parents>)>,
);

/// Loads `work` and returns it, preloaded, with what it holds that no parent
/// holds at the same path. A failure comparing it with its parents is returned
/// beside the object, so the object is still emitted.
async fn expand<R>(
    ctx: &LoadContext,
    repo: &R,
    work: Work,
) -> Result<(StoredObject<R>, Result<Additions, WalkError>), WalkError>
where
    R: Send + Sync + 'static,
{
    let blobstore = ctx.blobstore();
    match work {
        Work::Node(id, parents) => {
            let object = ObjectId::of::<R, _>(&id);
            let node_ctx = ctx.tag(object.clone());
            let node = <DeletedManifestV2Id as Validatable<R>>::load(&id, &node_ctx, repo)
                .await
                .map_err(|error| WalkError::load(object.clone(), error))?;
            let own = own_entries(&node_ctx, blobstore, node.subentries.clone(), &[])
                .await
                .map_err(|error| WalkError::enumerate(Some(object), error.into()))?;
            drop(node_ctx);
            let object = StoredObject::preloaded(id, node);

            let additions = async {
                let mut parent_maps = Vec::new();
                let mut parent_shards = HashSet::new();
                for parent in parents {
                    let parent_id = ObjectId::of::<R, _>(&parent);
                    let parent_ctx = ctx.tag(parent_id.clone());
                    let parent_node: DeletedManifestV2 =
                        Loadable::load(&parent, &parent_ctx, blobstore)
                            .await
                            .map_err(|error| {
                                WalkError::enumerate(Some(parent_id.clone()), error.into())
                            })?;
                    let listing =
                        entries(&parent_ctx, blobstore, parent_node.subentries.clone(), &[])
                            .await
                            .map_err(|error| {
                                WalkError::enumerate(Some(parent_id.clone()), error.into())
                            })?;
                    parent_shards.extend(listing.shards.iter().map(|(_prefix, shard)| *shard));
                    parent_maps.push((parent_id, parent_node.subentries));
                }
                let parents = Arc::new(Parents {
                    maps: parent_maps,
                    shards: parent_shards,
                });
                compare::<R>(ctx, own, parents).await
            }
            .await;
            Ok((object, additions))
        }
        Work::Shard(prefix, shard, parents) => {
            let shard_id = ObjectId::of::<R, _>(&shard);
            let shard_ctx = ctx.tag(shard_id.clone());
            let map = <ShardedMapNodeDMv2Id as Validatable<R>>::load(&shard, &shard_ctx, repo)
                .await
                .map_err(|error| WalkError::load(shard_id.clone(), error))?;
            let shard_entries = own_entries(&shard_ctx, blobstore, map.clone(), &prefix)
                .await
                .map_err(|error| WalkError::enumerate(Some(shard_id), error.into()))?;
            drop(shard_ctx);
            let object = StoredObject::preloaded(shard, map);

            let additions = compare::<R>(ctx, shard_entries, parents).await;
            Ok((object, additions))
        }
    }
}

/// What `own`, a node's or stored shard's entries, holds that the parents may
/// not: each child with the parents' children of the same name, and each stored
/// shard with `parents`. The names are in key order, so each parent shard
/// covering them is loaded once, one at a time.
async fn compare<R>(
    ctx: &LoadContext,
    own: Entries,
    parents: Arc<Parents>,
) -> Result<Additions, WalkError>
where
    R: Send + Sync + 'static,
{
    let mut parent_children = vec![Vec::new(); own.values.len()];
    let names = own
        .values
        .iter()
        .map(|(name, _)| name.as_slice())
        .collect::<Vec<_>>();
    for (parent_id, parent_map) in &parents.maps {
        let found = lookup_sorted::<R>(ctx, parent_map, &names)
            .await
            .map_err(|error| match error.downcast::<WalkError>() {
                Ok(error) => error,
                Err(error) => {
                    WalkError::enumerate(Some(parent_id.clone()), LoadError::Unreadable(error))
                }
            })?;
        for (children, child) in parent_children.iter_mut().zip(found) {
            children.extend(child);
        }
    }
    let children = own
        .values
        .into_iter()
        .zip(parent_children)
        .map(|((_name, child), parents)| (child, parents))
        .collect::<Vec<_>>();
    let shards = own
        .shards
        .into_iter()
        .map(|(prefix, shard)| (prefix, shard, parents.clone()))
        .collect::<Vec<_>>();
    Ok((children, shards))
}

/// Adds `child` to the work list unless a parent holds it at the same path or
/// it is already queued. A child a parent holds is not recorded as queued,
/// because it can be new at another path. Parents holding the same node, as
/// a merge's often do, are compared with it once.
fn queue(
    pending: &mut Vec<(DeletedManifestV2Id, Vec<DeletedManifestV2Id>)>,
    queued: &mut HashSet<DeletedManifestV2Id>,
    child: DeletedManifestV2Id,
    mut parents: Vec<DeletedManifestV2Id>,
) {
    if !parents.contains(&child) && queued.insert(child) {
        let mut seen = HashSet::new();
        parents.retain(|parent| seen.insert(*parent));
        pending.push((child, parents));
    }
}

#[derive(Default)]
struct Entries {
    /// Entries the map holds itself, by full name.
    values: Vec<(Vec<u8>, DeletedManifestV2Id)>,
    /// Separately stored sharded-map nodes, by the key prefix they hold.
    shards: Vec<(Vec<u8>, ShardedMapNodeDMv2Id)>,
}

/// The entries of a node or stored shard the commit added, as `entries` lists
/// them. An entry whose full name is not a path element is an error, as when
/// the manifest's children are listed by name. Parents' entries are not
/// checked: the commit that added them checks them.
async fn own_entries(
    ctx: &CoreContext,
    blobstore: &impl KeyedBlobstore,
    map: ShardedMapNode<DeletedManifestV2Id>,
    prefix: &[u8],
) -> Result<Entries> {
    let own = entries(ctx, blobstore, map, prefix).await?;
    for (name, _child) in &own.values {
        MPathElement::new_from_slice(name)?;
    }
    Ok(own)
}

/// Lists the entries `map` holds itself and the separately stored sharded-map
/// nodes it refers to, without loading them. `prefix` is prepended to each key.
async fn entries(
    ctx: &CoreContext,
    blobstore: &impl KeyedBlobstore,
    map: ShardedMapNode<DeletedManifestV2Id>,
    prefix: &[u8],
) -> Result<Entries> {
    map.into_sharded_entries(ctx, blobstore)
        .try_fold(Entries::default(), |mut entries, output| async move {
            match output {
                ShardedTraversalOutput::Value(key, child) => {
                    entries
                        .values
                        .push(([prefix, key.as_slice()].concat(), child));
                }
                ShardedTraversalOutput::Shard(key, _remainder, shard) => {
                    entries
                        .shards
                        .push(([prefix, key.as_slice()].concat(), shard));
                }
            }
            Ok(entries)
        })
        .await
}

/// The child each of `names` has in `map`, or `None`, loading each stored
/// shard under its own tag, once when `names` are sorted. A shard that fails
/// to load fails as a `WalkError` naming that shard.
async fn lookup_sorted<R>(
    ctx: &LoadContext,
    map: &ShardedMapNode<DeletedManifestV2Id>,
    names: &[&[u8]],
) -> Result<Vec<Option<DeletedManifestV2Id>>>
where
    R: Send + Sync + 'static,
{
    map.lookup_sorted(names, &|shard: ShardedMapNodeDMv2Id| async move {
        let shard_id = ObjectId::of::<R, _>(&shard);
        let ctx = ctx.tag(shard_id.clone());
        Loadable::load(&shard, &ctx, ctx.blobstore())
            .await
            .map_err(|error| WalkError::enumerate(Some(shard_id), error.into()).into())
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::error::Error as _;
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    use anyhow::Result;
    use blobstore::Blobstore;
    use blobstore::BlobstoreGetData;
    use blobstore::Loadable;
    use blobstore::PutBehaviour;
    use context::CoreContext;
    use derivation_queue_thrift::DerivationPriority;
    use fbinit::FacebookInit;
    use futures::StreamExt;
    use futures::TryStreamExt;
    use memblob::Memblob;
    use mononoke_macros::mononoke;
    use mononoke_types::BlobstoreBytes;
    use mononoke_types::BlobstoreKey;
    use mononoke_types::BlobstoreValue;
    use mononoke_types::ChangesetId;
    use mononoke_types::DeletedManifestV2Id;
    use mononoke_types::MPathElement;
    use mononoke_types::audit::ObjectId;
    use mononoke_types::audit::StoredObject;
    use mononoke_types::audit::WalkError;
    use mononoke_types::audit::WalkStage;
    use mononoke_types::audit::Walkable;
    use mononoke_types::deleted_manifest_common::DeletedManifestCommon;
    use mononoke_types::deleted_manifest_v2::DeletedManifestV2;
    use mononoke_types::deleted_manifest_v2::ShardedMapNodeDMv2Id;
    use mononoke_types::sharded_map::ShardedMapNode;
    use mononoke_types::sharded_map::ShardedTraversalOutput;
    use repo_blobstore::RepoBlobstoreRef;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_derived_data::audit::Mapping;
    use repo_identity::RepoIdentityRef;
    use samplingblob::SamplingBlobstore;
    use samplingblob::SamplingHandler;
    use sorted_vector_map::SortedVectorMap;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::BasicTestRepo;
    use tests_utils::CreateCommitContext;
    use tests_utils::audit;
    use tests_utils::audit::OverlappingReadsBlobstore;
    use tests_utils::audit::failed;
    use tests_utils::audit::load_ctx;
    use tests_utils::audit::walk;

    use super::entries;
    use super::lookup_sorted;
    use super::queue;
    use crate::RootDeletedManifestIdCommon;
    use crate::RootDeletedManifestV2Id;

    type Repo = BasicTestRepo;

    async fn test_repo(fb: FacebookInit) -> Result<(Arc<BasicTestRepo>, Memblob)> {
        let blobstore = Memblob::default();
        let repo: BasicTestRepo = TestRepoFactory::new(fb)?
            .with_blobstore(Arc::new(blobstore.clone()))
            .build()
            .await?;
        Ok((Arc::new(repo), blobstore))
    }

    /// Everything the family yields for the commit: its objects, then any error.
    async fn walk_items<W: Walkable<BasicTestRepo>>(
        ctx: &CoreContext,
        repo: &Arc<BasicTestRepo>,
        changeset_id: ChangesetId,
    ) -> Result<Vec<Result<StoredObject<BasicTestRepo>, WalkError>>> {
        Ok(W::walk_new(
            load_ctx(ctx, &**repo),
            repo.clone(),
            audit::commit(ctx, &**repo, changeset_id).await?,
            NonZeroUsize::new(4).unwrap(),
        )
        .collect()
        .await)
    }

    /// Counts reads of each key.
    #[derive(Debug, Default)]
    struct ReadCounter(Mutex<HashMap<String, usize>>);

    impl SamplingHandler for ReadCounter {
        fn sample_get(
            &self,
            _ctx: &CoreContext,
            key: &str,
            _value: Option<&BlobstoreGetData>,
        ) -> Result<()> {
            *self
                .0
                .lock()
                .expect("lock poisoned")
                .entry(key.to_owned())
                .or_default() += 1;
            Ok(())
        }
    }

    async fn derive(
        ctx: &CoreContext,
        repo: &Repo,
        changeset_id: ChangesetId,
    ) -> Result<DeletedManifestV2Id> {
        let root = repo
            .repo_derived_data()
            .derive::<RootDeletedManifestV2Id>(ctx, changeset_id, DerivationPriority::LOW)
            .await?;
        Ok(*root.id())
    }

    /// Every manifest node and separately stored sharded-map node in a tree.
    /// Children are listed with `into_subentries`, which loads stored shards as
    /// Walker does.
    async fn tree(
        ctx: &CoreContext,
        repo: &Repo,
        root: DeletedManifestV2Id,
    ) -> Result<HashSet<ObjectId>> {
        let mut objects = HashSet::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if !objects.insert(ObjectId::of::<Repo, _>(&id)) {
                continue;
            }
            let node = Loadable::load(&id, ctx, repo.repo_blobstore()).await?;
            let outputs = node
                .clone()
                .into_sharded_subentries(ctx, repo.repo_blobstore())
                .try_collect::<Vec<_>>()
                .await?;
            for output in outputs {
                if let ShardedTraversalOutput::Shard(_prefix, _remainder, shard) = output {
                    objects.insert(ObjectId::of::<Repo, ShardedMapNodeDMv2Id>(&shard));
                }
            }
            let children = node
                .into_subentries(ctx, repo.repo_blobstore())
                .try_collect::<Vec<_>>()
                .await?;
            pending.extend(children.into_iter().map(|(_name, child)| child));
        }
        Ok(objects)
    }

    /// A adds more than `MAP_SHARD_SIZE` (2000) files under `many`; B deletes
    /// them all, so B's `many` node stores its entries in sharded-map nodes.
    async fn delete_sharded_directory(
        ctx: &CoreContext,
        repo: &Repo,
    ) -> Result<(ChangesetId, ChangesetId)> {
        let names = (0..2100)
            .map(|index| format!("many/f{index:04}"))
            .collect::<Vec<_>>();
        let a = CreateCommitContext::new_root(ctx, repo)
            .add_files(names.iter().map(|name| (name.as_str(), name.clone())))
            .commit()
            .await?;
        let b = names
            .iter()
            .fold(
                CreateCommitContext::new(ctx, repo, vec![a]),
                |commit, name| commit.delete_file(name.as_str()),
            )
            .commit()
            .await?;
        Ok((a, b))
    }

    /// The child named `name` of a manifest node, looked up through any stored
    /// sharded-map nodes.
    async fn child(
        ctx: &CoreContext,
        repo: &Repo,
        id: DeletedManifestV2Id,
        name: &str,
    ) -> Result<DeletedManifestV2Id> {
        let node = Loadable::load(&id, ctx, repo.repo_blobstore()).await?;
        node.lookup(
            ctx,
            repo.repo_blobstore(),
            &MPathElement::new_from_slice(name.as_bytes())?,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("{id} has no child {name}"))
    }

    /// Objects in the commit's tree that are in none of its parents' trees.
    async fn added(
        ctx: &CoreContext,
        repo: &Repo,
        root: DeletedManifestV2Id,
        parent_roots: &[DeletedManifestV2Id],
    ) -> Result<HashSet<ObjectId>> {
        let mut objects = tree(ctx, repo, root).await?;
        for parent in parent_roots {
            for object in tree(ctx, repo, *parent).await? {
                objects.remove(&object);
            }
        }
        Ok(objects)
    }

    /// Walks the family, checks that the mapping comes first and that every
    /// object validates, and returns the manifest objects after the mapping.
    async fn walk_manifests(
        ctx: &CoreContext,
        repo: &Arc<Repo>,
        changeset_id: ChangesetId,
    ) -> Result<Vec<ObjectId>> {
        let objects = walk::<RootDeletedManifestV2Id, _>(ctx, repo, changeset_id, 4).await??;
        assert_eq!(
            objects.first().map(|object| object.id().clone()),
            Some(ObjectId::of::<Repo, _>(
                &Mapping::<RootDeletedManifestV2Id>::new(changeset_id)
            )),
        );
        let ids = objects
            .iter()
            .skip(1)
            .map(|object| object.id().clone())
            .collect::<Vec<_>>();
        for object in objects {
            assert_eq!(
                object
                    .validate(&load_ctx(ctx, &**repo), &**repo, true)
                    .await?,
                vec![]
            );
        }
        Ok(ids)
    }

    fn as_set(ids: &[ObjectId]) -> HashSet<ObjectId> {
        let set = ids.iter().cloned().collect::<HashSet<_>>();
        assert_eq!(set.len(), ids.len(), "duplicate objects in {ids:?}");
        set
    }

    #[mononoke::fbinit_test]
    async fn linear_history_emits_only_each_commits_additions(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("dir/a", "a")
            .add_file("dir/b", "b")
            .add_file("live", "live")
            .commit()
            .await?;
        let b = CreateCommitContext::new(&ctx, &*repo, vec![a])
            .delete_file("dir/a")
            .commit()
            .await?;
        let c = CreateCommitContext::new(&ctx, &*repo, vec![b])
            .delete_file("live")
            .commit()
            .await?;
        let (root_a, root_b, root_c) = (
            derive(&ctx, &repo, a).await?,
            derive(&ctx, &repo, b).await?,
            derive(&ctx, &repo, c).await?,
        );

        assert_eq!(
            as_set(&walk_manifests(&ctx, &repo, a).await?),
            tree(&ctx, &repo, root_a).await?
        );
        let expected_b = added(&ctx, &repo, root_b, &[root_a]).await?;
        assert_eq!(as_set(&walk_manifests(&ctx, &repo, b).await?), expected_b);
        // B adds the root, `dir` and `dir/a`.
        assert_eq!(expected_b.len(), 3);
        let expected_c = added(&ctx, &repo, root_c, &[root_b]).await?;
        assert_eq!(as_set(&walk_manifests(&ctx, &repo, c).await?), expected_c);
        // C reuses B's `dir` subtree and adds only the root and `live`.
        assert_eq!(expected_c.len(), 2);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn merge_emits_only_objects_new_relative_to_both_parents(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let base = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("left/a", "a")
            .add_file("right/b", "b")
            .add_file("both/c", "c")
            .commit()
            .await?;
        let left = CreateCommitContext::new(&ctx, &*repo, vec![base])
            .delete_file("left/a")
            .commit()
            .await?;
        let right = CreateCommitContext::new(&ctx, &*repo, vec![base])
            .delete_file("right/b")
            .commit()
            .await?;
        let merge = CreateCommitContext::new(&ctx, &*repo, vec![left, right])
            .delete_file("right/b")
            .delete_file("left/a")
            .delete_file("both/c")
            .commit()
            .await?;
        let root_left = derive(&ctx, &repo, left).await?;
        let root_right = derive(&ctx, &repo, right).await?;
        let root_merge = derive(&ctx, &repo, merge).await?;

        let expected = added(&ctx, &repo, root_merge, &[root_left, root_right]).await?;

        assert_eq!(as_set(&walk_manifests(&ctx, &repo, merge).await?), expected);
        assert!(!expected.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn emits_only_changed_sharded_map_nodes(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let (a, b) = delete_sharded_directory(&ctx, &repo).await?;
        let c = CreateCommitContext::new(&ctx, &*repo, vec![b])
            .add_file("many/f0000", "back")
            .commit()
            .await?;
        let root_a = derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        let root_c = derive(&ctx, &repo, c).await?;
        let is_shard = |id: &ObjectId| id.name() == "DeletedManifestV2MapNode";

        let walked_b = as_set(&walk_manifests(&ctx, &repo, b).await?);
        let walked_c = as_set(&walk_manifests(&ctx, &repo, c).await?);

        assert_eq!(walked_b, added(&ctx, &repo, root_b, &[root_a]).await?);
        assert_eq!(walked_c, added(&ctx, &repo, root_c, &[root_b]).await?);
        let shards_b = walked_b.iter().filter(|id| is_shard(id)).count();
        let shards_c = walked_c.iter().filter(|id| is_shard(id)).count();
        assert!(shards_b > 1, "expected several shards, got {shards_b}");
        assert!(
            (1..shards_b).contains(&shards_c),
            "C changes one shard of {shards_b}, emitted {shards_c}"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn emits_children_stored_in_sharded_map_nodes(fb: FacebookInit) -> Result<()> {
        // Given B's `many` node stores its entries in sharded-map nodes
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let (a, b) = delete_sharded_directory(&ctx, &repo).await?;
        derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        let many = child(&ctx, &repo, root_b, "many").await?;
        // Every file B deleted shares this leaf, stored only in `many`'s shards.
        let leaf = child(&ctx, &repo, many, "f0000").await?;

        // When B's additions are walked
        let walked = walk_manifests(&ctx, &repo, b)
            .await?
            .into_iter()
            .filter(|id| id.name() == "DeletedManifestV2")
            .collect::<HashSet<_>>();

        // Then the manifest nodes are exactly the root, `many` and the leaf
        assert_eq!(
            walked,
            HashSet::from([root_b, many, leaf].map(|id| ObjectId::of::<Repo, _>(&id)))
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn directory_leaving_sharded_map_emits_only_its_additions(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given C restores 200 of the files B deleted, so C's `many` node holds
        // its remaining 1,900 entries itself while B's are in sharded-map nodes
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let (a, b) = delete_sharded_directory(&ctx, &repo).await?;
        let c = (0..200)
            .fold(
                CreateCommitContext::new(&ctx, &*repo, vec![b]),
                |commit, index| commit.add_file(format!("many/f{index:04}").as_str(), "back"),
            )
            .commit()
            .await?;
        derive(&ctx, &repo, a).await?;
        derive(&ctx, &repo, b).await?;
        let root_c = derive(&ctx, &repo, c).await?;
        let many = child(&ctx, &repo, root_c, "many").await?;

        // When C's additions are walked
        let walked = as_set(&walk_manifests(&ctx, &repo, c).await?);

        // Then C adds only its root and `many`: the leaf its entries share is B's
        assert_eq!(
            walked,
            HashSet::from([root_c, many].map(|id| ObjectId::of::<Repo, _>(&id)))
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn sorted_lookup_matches_lookup_for_present_and_absent_names(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given B's `many` map holds `a` in an inlined node and its other
        // entries in sharded-map nodes one and two levels down, and each entry
        // is a distinct deleted directory
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let present = std::iter::once("a".to_owned())
            .chain((0..2100).map(|index| format!("a{index:04}")))
            .chain((0..10).map(|index| format!("b{index}")))
            .collect::<Vec<_>>();
        let paths = present
            .iter()
            .map(|name| format!("many/{name}/{name}"))
            .collect::<Vec<_>>();
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_files(paths.iter().map(|path| (path.as_str(), path.clone())))
            .commit()
            .await?;
        let b = paths
            .iter()
            .fold(
                CreateCommitContext::new(&ctx, &*repo, vec![a]),
                |commit, path| commit.delete_file(path.as_str()),
            )
            .commit()
            .await?;
        derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        let many: DeletedManifestV2 = Loadable::load(
            &child(&ctx, &repo, root_b, "many").await?,
            &ctx,
            repo.repo_blobstore(),
        )
        .await?;
        let map = many.subentries;
        let listing = entries(&ctx, repo.repo_blobstore(), map.clone(), &[]).await?;
        assert_eq!(
            (
                listing
                    .values
                    .iter()
                    .map(|(key, _)| key.as_slice())
                    .collect::<Vec<_>>(),
                listing
                    .shards
                    .iter()
                    .map(|(prefix, _)| prefix.as_slice())
                    .collect::<Vec<_>>(),
            ),
            (
                vec![b"a".as_slice()],
                vec![b"a0".as_slice(), b"a1", b"a2", b"b"]
            ),
        );
        let names = present
            .iter()
            .map(String::as_str)
            .chain(["0", "a0", "a1500x", "a3000", "aa", "b", "b5x", "c"])
            .map(str::as_bytes)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut expected = Vec::new();
        for name in &names {
            expected.push(map.lookup(&ctx, repo.repo_blobstore(), name).await?);
        }

        // When the sorted pass looks up every name
        let found = lookup_sorted::<Repo>(&load_ctx(&ctx, &*repo), &map, &names).await?;

        // Then each result equals `lookup`'s
        assert_eq!(found, expected);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn directory_leaving_sharded_map_loads_each_parent_shard_once(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given C's `many` node holds its 1,900 entries itself, all under B's
        // sharded-map nodes, and a blobstore that counts reads of each key
        let ctx = CoreContext::test_mock(fb);
        let reads = Arc::new(ReadCounter::default());
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_blobstore(Arc::new(SamplingBlobstore::new(
                Memblob::default(),
                reads.clone(),
            )))
            .build()
            .await?;
        let repo = Arc::new(repo);
        let (a, b) = delete_sharded_directory(&ctx, &repo).await?;
        let c = (0..200)
            .fold(
                CreateCommitContext::new(&ctx, &*repo, vec![b]),
                |commit, index| commit.add_file(format!("many/f{index:04}").as_str(), "back"),
            )
            .commit()
            .await?;
        derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        derive(&ctx, &repo, c).await?;
        let many_b = child(&ctx, &repo, root_b, "many").await?;
        // Equal shards share a blob: here `f0` and `f1` hold the same relative
        // keys and leaf. Each position is listed with its own prefix.
        let positions = Loadable::load(&many_b, &ctx, repo.repo_blobstore())
            .await?
            .into_sharded_subentries(&ctx, repo.repo_blobstore())
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .filter_map(|output| match output {
                ShardedTraversalOutput::Shard(_prefix, _remainder, shard) => Some(format!(
                    "{}{}",
                    repo.repo_identity().id().prefix(),
                    shard.blobstore_key()
                )),
                ShardedTraversalOutput::Value(..) => None,
            })
            .fold(HashMap::<String, usize>::new(), |mut positions, key| {
                *positions.entry(key).or_default() += 1;
                positions
            });
        assert!(!positions.is_empty(), "B's `many` stores no shards");
        reads.0.lock().expect("lock poisoned").clear();

        // When C's additions are walked
        walk::<RootDeletedManifestV2Id, _>(&ctx, &repo, c, 4).await??;

        // Then each of B's sharded-map nodes is read once per position
        let reads = reads.0.lock().expect("lock poisoned");
        assert_eq!(
            positions
                .keys()
                .map(|key| (key.clone(), reads.get(key).copied().unwrap_or(0)))
                .collect::<HashMap<_, _>>(),
            positions
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn one_commit_reads_up_to_its_limit_of_nodes_at_once(fb: FacebookInit) -> Result<()> {
        // Given B deletes one file from each of eight directories, in a
        // repository that records overlapping manifest-node reads
        let ctx = CoreContext::test_mock(fb);
        let (blobstore, peak) =
            OverlappingReadsBlobstore::new(Memblob::default(), "deletedmanifest2.blake2.");
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_blobstore(Arc::new(blobstore))
            .build()
            .await?;
        let repo = Arc::new(repo);
        let names = (0..8).map(|n| format!("dir{n}/f{n}")).collect::<Vec<_>>();
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_files(names.iter().map(|name| (name.as_str(), name.clone())))
            .commit()
            .await?;
        let b = names
            .iter()
            .fold(
                CreateCommitContext::new(&ctx, &*repo, vec![a]),
                |commit, name| commit.delete_file(name.as_str()),
            )
            .commit()
            .await?;
        let root_a = derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        let expected = added(&ctx, &repo, root_b, &[root_a]).await?;
        peak.store(0, Ordering::SeqCst);

        // When its additions are walked with a limit of four loads
        let objects = walk_manifests(&ctx, &repo, b).await?;

        // Then it emits the root, the eight directories and their shared
        // leaf, and reads four nodes at once
        assert_eq!(as_set(&objects), expected);
        assert_eq!(expected.len(), 10);
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        Ok(())
    }

    #[mononoke::test]
    fn child_a_parent_holds_is_still_queued_where_it_is_new() -> Result<()> {
        // Given a child that a parent holds at the first path it appears at
        let child = DeletedManifestV2Id::from_bytes([1; 32])?;
        let other = DeletedManifestV2Id::from_bytes([2; 32])?;
        let (mut pending, mut queued) = (Vec::new(), HashSet::new());
        queue(&mut pending, &mut queued, child, vec![child]);

        // When it appears at a path where it is new, and then at a third
        queue(&mut pending, &mut queued, child, vec![other]);
        queue(&mut pending, &mut queued, child, vec![]);

        // Then it is queued once, with the parents of the path where it is new
        assert_eq!(pending, vec![(child, vec![other])]);
        Ok(())
    }

    #[mononoke::test]
    fn parents_holding_the_same_node_are_compared_once() -> Result<()> {
        // Given a merge whose two parents hold the same node at a path
        let child = DeletedManifestV2Id::from_bytes([1; 32])?;
        let parent = DeletedManifestV2Id::from_bytes([2; 32])?;
        let (mut pending, mut queued) = (Vec::new(), HashSet::new());

        // When the merge's node at that path is queued
        queue(&mut pending, &mut queued, child, vec![parent, parent]);

        // Then it is compared with that node once
        assert_eq!(pending, vec![(child, vec![parent])]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn missing_added_shard_fails_with_its_key(fb: FacebookInit) -> Result<()> {
        // Given one of the sharded-map nodes B's `many` node added is removed
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let (a, b) = delete_sharded_directory(&ctx, &repo).await?;
        derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        let many = child(&ctx, &repo, root_b, "many").await?;
        let shard = Loadable::load(&many, &ctx, repo.repo_blobstore())
            .await?
            .into_sharded_subentries(&ctx, repo.repo_blobstore())
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .find_map(|output| match output {
                ShardedTraversalOutput::Shard(_prefix, _remainder, shard) => Some(shard),
                ShardedTraversalOutput::Value(..) => None,
            })
            .expect("`many` stores its entries in sharded-map nodes");
        let key = format!(
            "{}{}",
            repo.repo_identity().id().prefix(),
            shard.blobstore_key()
        );
        assert_eq!(blobstore.unlink(key).await?, Some(()));

        // When B's additions are walked
        let error = walk::<RootDeletedManifestV2Id, _>(&ctx, &repo, b, 4)
            .await?
            .expect_err("a sharded-map node B added was removed");

        // Then the failure names the shard at `load`
        assert_eq!(
            failed(error),
            (
                "missing",
                WalkStage::Load,
                Some(ObjectId::of::<Repo, _>(&shard))
            )
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn underived_commit_is_not_derived(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("a", "a")
            .commit()
            .await?;
        derive(&ctx, &repo, a).await?;
        let b = CreateCommitContext::new(&ctx, &*repo, vec![a])
            .delete_file("a")
            .commit()
            .await?;

        let error = walk::<RootDeletedManifestV2Id, _>(&ctx, &repo, b, 4)
            .await?
            .expect_err("B's deleted manifest is not derived");

        assert_eq!(
            failed(error),
            (
                "not_derived",
                WalkStage::Load,
                Some(ObjectId::of::<Repo, _>(
                    &Mapping::<RootDeletedManifestV2Id>::new(b)
                ))
            )
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn missing_added_node_fails_with_its_key(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("dir/a", "a")
            .commit()
            .await?;
        let b = CreateCommitContext::new(&ctx, &*repo, vec![a])
            .delete_file("dir/a")
            .commit()
            .await?;
        derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        let key = format!(
            "{}{}",
            repo.repo_identity().id().prefix(),
            root_b.blobstore_key()
        );
        assert_eq!(blobstore.unlink(key).await?, Some(()));

        let error = walk::<RootDeletedManifestV2Id, _>(&ctx, &repo, b, 4)
            .await?
            .expect_err("B's root manifest node was removed");

        assert_eq!(
            failed(error),
            (
                "missing",
                WalkStage::Load,
                Some(ObjectId::of::<Repo, _>(&root_b))
            )
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn lost_parent_mapping_is_missing(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("dir/a", "a")
            .commit()
            .await?;
        let b = CreateCommitContext::new(&ctx, &*repo, vec![a])
            .delete_file("dir/a")
            .commit()
            .await?;
        derive(&ctx, &repo, a).await?;
        derive(&ctx, &repo, b).await?;
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
        let key = format!(
            "{}{}",
            repo.repo_identity().id().prefix(),
            crate::format_key(&derivation_ctx, a)
        );
        assert_eq!(blobstore.unlink(key).await?, Some(()));

        let error = walk::<RootDeletedManifestV2Id, _>(&ctx, &repo, b, 4)
            .await?
            .expect_err("B is derived but its parent's mapping was removed");

        assert_eq!(
            failed(error),
            (
                "missing",
                WalkStage::Enumerate,
                Some(ObjectId::of::<Repo, _>(
                    &Mapping::<RootDeletedManifestV2Id>::new(a)
                ))
            )
        );
        Ok(())
    }

    /// The error's cause, as printed after it.
    fn cause(error: &WalkError) -> String {
        error.source().map(ToString::to_string).unwrap_or_default()
    }

    /// The sharded-map node that `node` stores at key prefix `prefix`.
    async fn shard_at(
        ctx: &CoreContext,
        repo: &Repo,
        node: DeletedManifestV2Id,
        prefix: &[u8],
    ) -> Result<ShardedMapNodeDMv2Id> {
        Loadable::load(&node, ctx, repo.repo_blobstore())
            .await?
            .into_sharded_subentries(ctx, repo.repo_blobstore())
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .find_map(|output| match output {
                ShardedTraversalOutput::Shard(key, _remainder, shard)
                    if key.as_slice() == prefix =>
                {
                    Some(shard)
                }
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("{node} stores no shard at {prefix:?}"))
    }

    #[mononoke::fbinit_test]
    async fn lost_parent_shard_a_node_is_compared_with_is_missing(fb: FacebookInit) -> Result<()> {
        // Given C's `many` node holds its 1,900 entries itself, and the
        // sharded-map node of B's `many` that holds the names under `f0` is
        // removed
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let (a, b) = delete_sharded_directory(&ctx, &repo).await?;
        let c = (0..200)
            .fold(
                CreateCommitContext::new(&ctx, &*repo, vec![b]),
                |commit, index| commit.add_file(format!("many/f{index:04}").as_str(), "back"),
            )
            .commit()
            .await?;
        derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        derive(&ctx, &repo, c).await?;
        let many_b = child(&ctx, &repo, root_b, "many").await?;
        let shard = shard_at(&ctx, &repo, many_b, b"f0").await?;
        let key = format!(
            "{}{}",
            repo.repo_identity().id().prefix(),
            shard.blobstore_key()
        );
        assert_eq!(blobstore.unlink(key).await?, Some(()));

        // When C's additions are walked
        let error = walk::<RootDeletedManifestV2Id, _>(&ctx, &repo, c, 4)
            .await?
            .expect_err("a sharded-map node of B's `many` was removed");

        // Then the removed sharded-map node fails at `enumerate` as missing,
        // naming its key
        assert!(
            cause(&error).contains(&shard.blobstore_key()),
            "the cause should name the removed node's key: {error:?}"
        );
        assert_eq!(
            failed(error),
            (
                "missing",
                WalkStage::Enumerate,
                Some(ObjectId::of::<Repo, _>(&shard))
            )
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn lost_parent_shard_a_changed_shard_is_compared_with_is_missing(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given C restores `many/f0000`, changing the sharded-map node of its
        // `many` that holds the names under `f0`, and B's node at `f0` is
        // removed
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let (a, b) = delete_sharded_directory(&ctx, &repo).await?;
        let c = CreateCommitContext::new(&ctx, &*repo, vec![b])
            .add_file("many/f0000", "back")
            .commit()
            .await?;
        derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        derive(&ctx, &repo, c).await?;
        let many_b = child(&ctx, &repo, root_b, "many").await?;
        let shard = shard_at(&ctx, &repo, many_b, b"f0").await?;
        let key = format!(
            "{}{}",
            repo.repo_identity().id().prefix(),
            shard.blobstore_key()
        );
        assert_eq!(blobstore.unlink(key).await?, Some(()));

        // When C's additions are walked
        let error = walk::<RootDeletedManifestV2Id, _>(&ctx, &repo, c, 4)
            .await?
            .expect_err("a sharded-map node of B's `many` was removed");

        // Then the removed sharded-map node fails at `enumerate` as missing,
        // naming its key
        assert!(
            cause(&error).contains(&shard.blobstore_key()),
            "the cause should name the removed node's key: {error:?}"
        );
        assert_eq!(
            failed(error),
            (
                "missing",
                WalkStage::Enumerate,
                Some(ObjectId::of::<Repo, _>(&shard))
            )
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn undecodable_parent_shard_a_node_is_compared_with_is_unreadable(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given C's `many` node holds its 1,900 entries itself, and the
        // sharded-map node of B's `many` that holds the names under `f0` is
        // overwritten with bytes that do not decode
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let (a, b) = delete_sharded_directory(&ctx, &repo).await?;
        let c = (0..200)
            .fold(
                CreateCommitContext::new(&ctx, &*repo, vec![b]),
                |commit, index| commit.add_file(format!("many/f{index:04}").as_str(), "back"),
            )
            .commit()
            .await?;
        derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        derive(&ctx, &repo, c).await?;
        let many_b = child(&ctx, &repo, root_b, "many").await?;
        let shard = shard_at(&ctx, &repo, many_b, b"f0").await?;
        let key = format!(
            "{}{}",
            repo.repo_identity().id().prefix(),
            shard.blobstore_key()
        );
        blobstore
            .put_explicit(
                &ctx,
                key,
                BlobstoreBytes::from_bytes(&b"not a sharded-map node"[..]),
                PutBehaviour::Overwrite,
            )
            .await?;

        // When C's additions are walked
        let error = walk::<RootDeletedManifestV2Id, _>(&ctx, &repo, c, 4)
            .await?
            .expect_err("a sharded-map node of B's `many` does not decode");

        // Then that sharded-map node, not B's `many` node, fails at
        // `enumerate` as unreadable
        assert_eq!(
            failed(error),
            (
                "unreadable",
                WalkStage::Enumerate,
                Some(ObjectId::of::<Repo, _>(&shard))
            )
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn added_node_is_emitted_before_its_lost_parent_node_fails(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given B deletes `dir/a`, and A's root manifest node, which B's root
        // is compared with, is removed
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("dir/a", "a")
            .commit()
            .await?;
        let b = CreateCommitContext::new(&ctx, &*repo, vec![a])
            .delete_file("dir/a")
            .commit()
            .await?;
        let root_a = derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        let key = format!(
            "{}{}",
            repo.repo_identity().id().prefix(),
            root_a.blobstore_key()
        );
        assert_eq!(blobstore.unlink(key).await?, Some(()));

        // When B's additions are walked
        let items = walk_items::<RootDeletedManifestV2Id>(&ctx, &repo, b)
            .await?
            .into_iter()
            .map(|item| item.map(|object| object.id().clone()).map_err(failed))
            .collect::<Vec<_>>();

        // Then B's mapping and root are emitted, and then A's root fails at
        // `enumerate`
        assert_eq!(
            items,
            vec![
                Ok(ObjectId::of::<Repo, _>(
                    &Mapping::<RootDeletedManifestV2Id>::new(b)
                )),
                Ok(ObjectId::of::<Repo, _>(&root_b)),
                Err((
                    "missing",
                    WalkStage::Enumerate,
                    Some(ObjectId::of::<Repo, _>(&root_a))
                )),
            ]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn changed_shard_reads_only_the_parent_shards_holding_its_names(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given B deletes 10,000 files under `many/f0` and 2,100 under
        // `many/g`, so its `many` node stores the `f0` names in ten shards, and
        // C restores all but the last 1,500 `f0` files, which its `many` node
        // stores in one changed shard, in a repository that counts reads of
        // each key
        let ctx = CoreContext::test_mock(fb);
        let reads = Arc::new(ReadCounter::default());
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_blobstore(Arc::new(SamplingBlobstore::new(
                Memblob::default(),
                reads.clone(),
            )))
            .build()
            .await?;
        let repo = Arc::new(repo);
        let names = (0..10_000)
            .map(|index| format!("f{index:05}"))
            .chain((0..2_100).map(|index| format!("g{index:04}")))
            .collect::<Vec<_>>();
        let paths = names
            .iter()
            .map(|name| format!("many/{name}"))
            .collect::<Vec<_>>();
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_files(paths.iter().map(|path| (path.as_str(), path.clone())))
            .commit()
            .await?;
        let b = paths
            .iter()
            .fold(
                CreateCommitContext::new(&ctx, &*repo, vec![a]),
                |commit, path| commit.delete_file(path.as_str()),
            )
            .commit()
            .await?;
        let c = paths[..8_500]
            .iter()
            .fold(
                CreateCommitContext::new(&ctx, &*repo, vec![b]),
                |commit, path| commit.add_file(path.as_str(), "back"),
            )
            .commit()
            .await?;
        derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        let root_c = derive(&ctx, &repo, c).await?;
        let many_b = child(&ctx, &repo, root_b, "many").await?;
        let positions = Loadable::load(&many_b, &ctx, repo.repo_blobstore())
            .await?
            .into_sharded_subentries(&ctx, repo.repo_blobstore())
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .filter_map(|output| match output {
                ShardedTraversalOutput::Shard(prefix, _remainder, shard) => Some((
                    prefix.to_vec(),
                    format!(
                        "{}{}",
                        repo.repo_identity().id().prefix(),
                        shard.blobstore_key()
                    ),
                )),
                ShardedTraversalOutput::Value(..) => None,
            })
            .collect::<Vec<_>>();
        let still_deleted = &names[8_500..10_000];
        let expected_reads = positions
            .iter()
            .filter(|(prefix, _key)| {
                still_deleted
                    .iter()
                    .any(|name| name.as_bytes().starts_with(prefix))
            })
            .fold(
                HashMap::<String, usize>::new(),
                |mut reads, (_prefix, key)| {
                    *reads.entry(key.clone()).or_default() += 1;
                    reads
                },
            );
        assert!(
            positions.len() > expected_reads.values().sum::<usize>(),
            "every shard of B's `many` holds a name C leaves deleted: {positions:?}"
        );
        let expected = added(&ctx, &repo, root_c, &[root_b]).await?;
        reads.0.lock().expect("lock poisoned").clear();

        // When C's additions are walked
        let objects = walk_manifests(&ctx, &repo, c).await?;

        // Then it emits what C added, and of B's shards it reads only those
        // holding a name in C's changed shard, once per position
        assert_eq!(as_set(&objects), expected);
        let reads = reads.0.lock().expect("lock poisoned");
        assert_eq!(
            positions
                .iter()
                .filter_map(|(_prefix, key)| reads.get(key).map(|count| (key.clone(), *count)))
                .collect::<HashMap<_, _>>(),
            expected_reads
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn malformed_entry_name_in_a_parent_is_left_to_the_parent(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given A deletes `x/1` and B deletes `x/2`, and A's root node is
        // rewritten so that it also holds an entry named `bad/name`
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let root = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("x/1", "1")
            .add_file("x/2", "2")
            .commit()
            .await?;
        let a = CreateCommitContext::new(&ctx, &*repo, vec![root])
            .delete_file("x/1")
            .commit()
            .await?;
        let b = CreateCommitContext::new(&ctx, &*repo, vec![a])
            .delete_file("x/2")
            .commit()
            .await?;
        derive(&ctx, &repo, root).await?;
        let root_a = derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        let expected = added(&ctx, &repo, root_b, &[root_a]).await?;
        let x = child(&ctx, &repo, root_a, "x").await?;
        let mut node = Loadable::load(&root_a, &ctx, repo.repo_blobstore()).await?;
        node.subentries = ShardedMapNode::Terminal {
            values: SortedVectorMap::from_iter([(b"bad/name"[..].into(), x), (b"x"[..].into(), x)]),
        };
        let key = format!(
            "{}{}",
            repo.repo_identity().id().prefix(),
            root_a.blobstore_key()
        );
        blobstore
            .put_explicit(&ctx, key, node.into_blob().into(), PutBehaviour::Overwrite)
            .await?;

        // When B's additions are walked
        let objects = walk_manifests(&ctx, &repo, b).await?;

        // Then B's additions are emitted: the bad name is A's to report
        assert_eq!(as_set(&objects), expected);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn malformed_entry_name_is_unreadable(fb: FacebookInit) -> Result<()> {
        // Given B deletes `dir/file`, and B's root node is rewritten so that
        // its entry is named `bad/name`, which is not a path element
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("dir/file", "a")
            .commit()
            .await?;
        let b = CreateCommitContext::new(&ctx, &*repo, vec![a])
            .delete_file("dir/file")
            .commit()
            .await?;
        derive(&ctx, &repo, a).await?;
        let root_b = derive(&ctx, &repo, b).await?;
        let dir = child(&ctx, &repo, root_b, "dir").await?;
        let mut node = Loadable::load(&root_b, &ctx, repo.repo_blobstore()).await?;
        node.subentries = ShardedMapNode::Terminal {
            values: SortedVectorMap::from_iter([(b"bad/name"[..].into(), dir)]),
        };
        let key = format!(
            "{}{}",
            repo.repo_identity().id().prefix(),
            root_b.blobstore_key()
        );
        blobstore
            .put_explicit(&ctx, key, node.into_blob().into(), PutBehaviour::Overwrite)
            .await?;

        // When B's additions are walked
        let error = walk::<RootDeletedManifestV2Id, _>(&ctx, &repo, b, 4)
            .await?
            .expect_err("B's root node has a malformed entry name");

        // Then B's root node fails at `enumerate` as unreadable
        assert_eq!(
            failed(error),
            (
                "unreadable",
                WalkStage::Enumerate,
                Some(ObjectId::of::<Repo, _>(&root_b))
            )
        );
        Ok(())
    }
}
