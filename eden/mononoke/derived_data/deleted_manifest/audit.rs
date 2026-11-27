/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use context::CoreContext;
use futures::TryStreamExt;
use mononoke_types::DeletedManifestV2Id;
use mononoke_types::audit::LoadContext;
use mononoke_types::audit::ObjectId;
use mononoke_types::deleted_manifest_v2::ShardedMapNodeDMv2Id;
use mononoke_types::sharded_map::ShardedMapNode;
use mononoke_types::sharded_map::ShardedTraversalOutput;

#[derive(Default)]
#[allow(dead_code)]
struct Entries {
    /// Entries the map holds itself, by full name.
    values: Vec<(Vec<u8>, DeletedManifestV2Id)>,
    /// Separately stored sharded-map nodes, by the key prefix they hold.
    shards: Vec<(Vec<u8>, ShardedMapNodeDMv2Id)>,
}

/// Lists the entries `map` holds itself and the separately stored sharded-map
/// nodes it refers to, without loading them. `prefix` is prepended to each key.
#[allow(dead_code)]
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
/// shard under its own tag, once when `names` are sorted.
#[allow(dead_code)]
async fn lookup_sorted<R>(
    ctx: &LoadContext,
    map: &ShardedMapNode<DeletedManifestV2Id>,
    names: &[&[u8]],
) -> Result<Vec<Option<DeletedManifestV2Id>>>
where
    R: Send + Sync + 'static,
{
    map.lookup_sorted(names, &|shard: ShardedMapNodeDMv2Id| async move {
        let ctx = ctx.tag(ObjectId::of::<R, _>(&shard));
        Loadable::load(&shard, &ctx, ctx.blobstore())
            .await
            .map_err(Into::into)
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use anyhow::Result;
    use blobstore::Loadable;
    use context::CoreContext;
    use derivation_queue_thrift::DerivationPriority;
    use fbinit::FacebookInit;
    use memblob::Memblob;
    use mononoke_macros::mononoke;
    use mononoke_types::ChangesetId;
    use mononoke_types::DeletedManifestV2Id;
    use mononoke_types::MPathElement;
    use mononoke_types::audit::LoadContext;
    use mononoke_types::audit::LoadTags;
    use mononoke_types::deleted_manifest_common::DeletedManifestCommon;
    use mononoke_types::deleted_manifest_v2::DeletedManifestV2;
    use repo_blobstore::RepoBlobstoreRef;
    use repo_derived_data::RepoDerivedDataRef;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::BasicTestRepo;
    use tests_utils::CreateCommitContext;

    use super::entries;
    use super::lookup_sorted;
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

    /// A load context over the repo's blobstore whose tags nothing reads.
    fn load_ctx(ctx: &CoreContext, repo: &impl RepoBlobstoreRef) -> LoadContext {
        LoadContext::new(
            ctx.clone(),
            Arc::new(repo.repo_blobstore().clone()),
            Arc::new(LoadTags::default()),
        )
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
}
