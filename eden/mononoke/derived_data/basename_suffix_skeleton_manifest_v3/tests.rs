/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Result;
use async_recursion::async_recursion;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::Bookmarks;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphRef;
use commit_graph::CommitGraphWriter;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use fbinit::FacebookInit;
use filestore::FilestoreConfig;
use fixtures::TestRepoFixture;
use futures::TryStreamExt;
use itertools::Itertools;
use manifest::ManifestOps;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetIdPrefix;
use mononoke_types::ChangesetIdsResolvedFromPrefix;
use mononoke_types::MPathElement;
use mononoke_types::basename_suffix_skeleton_manifest_v3::BssmV3Directory;
use mononoke_types::basename_suffix_skeleton_manifest_v3::BssmV3Entry;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentity;
use skeleton_manifest::RootSkeletonManifestId;

use crate::RootBssmV3DirectoryId;
use crate::derive_from_predecessor::inner_derive_from_predecessor;
use crate::path::BssmPath;

#[mononoke::fbinit_test]
async fn sharded_partitions_decode_only_when_consumed(fb: FacebookInit) -> Result<()> {
    use derived_data_manager::manifest_partitions::PartitionMap;
    use mononoke_types::BlobstoreBytes;
    use mononoke_types::ThriftConvert;
    use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;

    type Map = LoadableShardedMapV2Node<BssmV3Entry>;
    let ctx = &CoreContext::test_mock(fb);
    let repo: TestRepo = fixtures::Linear::get_repo(fb).await;
    let derivation = repo.repo_derived_data().manager().derivation_context(None);
    for bytes in [
        BlobstoreBytes::from_bytes(Map::default().into_bytes()),
        BlobstoreBytes::empty(),
    ] {
        let output =
            <Map as PartitionMap<RootBssmV3DirectoryId>>::decode(ctx, &derivation, bytes.clone())?;
        assert_eq!(
            <Map as PartitionMap<RootBssmV3DirectoryId>>::encode(ctx, &derivation, output.clone())
                .await?,
            bytes,
        );
        let loaded = output.load().await;
        if bytes.is_empty() {
            assert!(loaded.is_err());
        } else {
            assert_eq!(loaded?, Map::default());
        }
    }
    Ok(())
}

#[facet::container]
struct TestRepo(
    dyn BonsaiHgMapping,
    dyn Bookmarks,
    CommitGraph,
    dyn CommitGraphWriter,
    RepoDerivedData,
    RepoBlobstore,
    FilestoreConfig,
    RepoIdentity,
);

#[async_recursion]
async fn validate(
    visited: &RwLock<HashSet<BssmV3Directory>>,
    ctx: &CoreContext,
    blobstore: &impl KeyedBlobstore,
    dir: BssmV3Directory,
    rev_basename: Option<&'async_recursion MPathElement>,
) -> Result<()> {
    if visited.read().unwrap().contains(&dir) {
        return Ok(());
    }
    visited.write().unwrap().insert(dir.clone());

    let mf = dir.load(ctx, blobstore).await?;
    let calculated_rollup_count = AtomicU64::new(1);
    let calculated_rollup_count = &calculated_rollup_count;
    mf.into_subentries(ctx, blobstore)
        .try_for_each_concurrent(None, |(mut path, entry)| async move {
            calculated_rollup_count.fetch_add(entry.rollup_count().0, Ordering::Relaxed);
            match entry {
                BssmV3Entry::File => {
                    path.reverse();
                    assert_eq!(Some(&path), rev_basename)
                }
                BssmV3Entry::Directory(dir) => {
                    validate(visited, ctx, blobstore, dir, rev_basename.or(Some(&path))).await?
                }
            }
            Ok(())
        })
        .await?;
    let count = calculated_rollup_count.load(Ordering::Relaxed);
    assert_eq!(dir.rollup_count().0, count);

    Ok(())
}

async fn test_for_fixture<F: TestRepoFixture + Send>(fb: FacebookInit) -> Result<()> {
    let ctx = &CoreContext::test_mock(fb);
    let repo: TestRepo = F::get_repo(fb).await;
    let derived_data = repo.repo_derived_data();
    let blobstore = repo.repo_blobstore();
    let all_commits = repo
        .commit_graph()
        .find_by_prefix(ctx, ChangesetIdPrefix::from_bytes("").unwrap(), 1000)
        .await?;
    let all_commits = match all_commits {
        ChangesetIdsResolvedFromPrefix::Multiple(all_commits) => all_commits,
        other => anyhow::bail!("Unexpected number of commits: {other:?}"),
    };
    let visited = &RwLock::new(HashSet::new());
    repo.commit_graph()
        .process_topologically(ctx, all_commits, |cs_id| async move {
            let bssm_v3_dir_id = derived_data
                .derive::<RootBssmV3DirectoryId>(ctx, cs_id, DerivationPriority::LOW)
                .await?;
            let bssm_v3_dir = bssm_v3_dir_id.into_inner_id().load(ctx, blobstore).await?;
            validate(visited, ctx, blobstore, bssm_v3_dir.clone(), None).await?;

            let skeleton_manifest = derived_data
                .derive::<RootSkeletonManifestId>(ctx, cs_id, DerivationPriority::LOW)
                .await?
                .into_skeleton_manifest_id();

            let bssm_v3_leaf_entries = bssm_v3_dir
                .list_leaf_entries(ctx.clone(), blobstore.clone())
                .try_collect::<Vec<_>>()
                .await?;
            let skeleton_manifest_leaf_entries = skeleton_manifest
                .list_leaf_entries(ctx.clone(), blobstore.clone())
                .try_collect::<Vec<_>>()
                .await?;

            assert_eq!(
                bssm_v3_leaf_entries
                    .into_iter()
                    .sorted()
                    .collect::<Vec<_>>(),
                skeleton_manifest_leaf_entries
                    .into_iter()
                    .map(|(path, ())| (BssmPath::transform(path).into_raw(), ()))
                    .sorted()
                    .collect::<Vec<_>>(),
            );

            let bssm_v3_dir_from_skeleton_manifest =
                inner_derive_from_predecessor(ctx, &blobstore.boxed(), skeleton_manifest, 3)
                    .await?;

            assert_eq!(bssm_v3_dir, bssm_v3_dir_from_skeleton_manifest);

            Ok(())
        })
        .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_bssm_v3_repo_fixtures(fb: FacebookInit) {
    futures::try_join!(
        test_for_fixture::<fixtures::Linear>(fb),
        test_for_fixture::<fixtures::BranchEven>(fb),
        test_for_fixture::<fixtures::BranchUneven>(fb),
        test_for_fixture::<fixtures::BranchWide>(fb),
        test_for_fixture::<fixtures::MergeEven>(fb),
        test_for_fixture::<fixtures::ManyFilesDirs>(fb),
        test_for_fixture::<fixtures::MergeUneven>(fb),
        test_for_fixture::<fixtures::UnsharedMergeEven>(fb),
        test_for_fixture::<fixtures::UnsharedMergeUneven>(fb),
        test_for_fixture::<fixtures::ManyDiamonds>(fb),
    )
    .unwrap();
}
