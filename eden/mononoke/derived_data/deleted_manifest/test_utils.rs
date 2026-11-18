/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Error;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::Bookmarks;
use bounded_traversal::bounded_traversal_stream;
use changesets_creation::save_changesets;
use cloned::cloned;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use derived_data_test_utils::bonsai_changeset_from_hg;
use fbinit::FacebookInit;
use filestore::FilestoreConfig;
use fixtures::ManyFilesDirs;
use fixtures::MergedDeletionHistory;
use fixtures::TestRepoFixture;
use fixtures::store_files;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::pin_mut;
use futures::stream::iter;
use manifest::PathOrPrefix;
use maplit::btreemap;
use mononoke_types::BonsaiChangeset;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::ChangesetId;
use mononoke_types::DateTime;
use mononoke_types::DeletedManifestV2Id;
use mononoke_types::FileChange;
use mononoke_types::NonRootMPath;
use mononoke_types::deleted_manifest_common::DeletedManifestCommon;
use mononoke_types::deleted_manifest_v2::DeletedManifestV2;
use mononoke_types::path::MPath;
use pretty_assertions::assert_eq;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreArc;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentity;
use sorted_vector_map::SortedVectorMap;
use unodes::RootUnodeManifestId;

use crate::derive::DeletedManifestDeriver;
use crate::derive::get_unodes;
use crate::mapping::RootDeletedManifestIdCommon;
use crate::ops::DeletedManifestOps;

#[derive(Clone)]
#[facet::container]
pub(crate) struct TestRepo {
    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,
    #[facet]
    bookmarks: dyn Bookmarks,
    #[facet]
    repo_blobstore: RepoBlobstore,
    #[facet]
    repo_derived_data: RepoDerivedData,
    #[facet]
    filestore_config: FilestoreConfig,
    #[facet]
    commit_graph: CommitGraph,
    #[facet]
    commit_graph_writer: dyn CommitGraphWriter,
    #[facet]
    repo_identity: RepoIdentity,
}

/// Defines all common DM tests.
// Why a macro and not a function? So we get different tests that are parallelised
// and have separate signal, instead of a single catch-all test.
macro_rules! impl_deleted_manifest_tests {
    ($manifest:ty) => {
        mod tests {
            use ::anyhow::Result;
            use ::fbinit::FacebookInit;
            use ::mononoke_macros::mononoke;

            use super::*;

            #[mononoke::fbinit_test]
            async fn linear_test(fb: FacebookInit) -> Result<(), Error> {
                $crate::test_utils::linear_test::<$manifest>(fb).await
            }
            #[mononoke::fbinit_test]
            async fn many_file_dirs_test(fb: FacebookInit) -> Result<(), Error> {
                $crate::test_utils::many_file_dirs_test::<$manifest>(fb).await
            }
            #[mononoke::fbinit_test]
            async fn merged_history_test(fb: FacebookInit) -> Result<()> {
                $crate::test_utils::merged_history_test::<$manifest>(fb).await
            }
            #[mononoke::fbinit_test]
            async fn test_find_entries(fb: FacebookInit) -> Result<(), Error> {
                $crate::test_utils::test_find_entries::<$manifest>(fb).await
            }
            #[mononoke::fbinit_test]
            async fn test_list_all_entries(fb: FacebookInit) -> Result<(), Error> {
                $crate::test_utils::test_list_all_entries::<$manifest>(fb).await
            }
        }
    };
}
pub(crate) use impl_deleted_manifest_tests;

pub(crate) async fn build_repo(fb: FacebookInit) -> Result<TestRepo, Error> {
    Ok(test_repo_factory::TestRepoFactory::new(fb)?.build().await?)
}

pub(crate) async fn linear_test<
    Root: RootDeletedManifestIdCommon<Manifest = DeletedManifestV2, Id = DeletedManifestV2Id>,
>(
    fb: FacebookInit,
) -> Result<(), Error> {
    // Test simple separate files and whole dir deletions
    let repo: TestRepo = build_repo(fb).await.unwrap();
    let ctx = CoreContext::test_mock(fb);

    // create parent deleted manifest
    let (bcs_id_1, mf_id_1) = {
        let file_changes = btreemap! {
            "file.txt" => Some("1\n"),
            "file-2.txt" => Some("2\n"),
            "dir/sub/f-1" => Some("3\n"),
            "dir/f-2" => Some("4\n"),
            "dir-2/sub/f-3" => Some("5\n"),
            "dir-2/f-4" => Some("6\n"),
        };
        let (bcs_id, mf_id, deleted_nodes) =
            create_cs_and_derive_manifest::<Root>(ctx.clone(), repo.clone(), file_changes, vec![])
                .await?;

        // nothing was deleted yet
        let expected_nodes = vec![(MPath::ROOT, Status::Live)];
        assert_eq!(deleted_nodes, expected_nodes);

        (bcs_id, mf_id)
    };

    // delete some files and dirs
    let (bcs_id_2, mf_id_2) = {
        let file_changes = btreemap! {
            "file.txt" => None,
            "file-2.txt" => Some("2\n2\n"),
            "file-3.txt" => Some("3\n3\n"),
            "dir/sub/f-1" => None,
            "dir/f-2" => None,
            "dir-2/sub/f-3" => None,
        };
        let (bcs_id, mf_id, deleted_nodes) = create_cs_and_derive_manifest::<Root>(
            ctx.clone(),
            repo.clone(),
            file_changes,
            vec![(bcs_id_1, mf_id_1)],
        )
        .await?;

        let expected_nodes = vec![
            (MPath::ROOT, Status::Live),
            (path("dir"), Status::Deleted(bcs_id)),
            (path("dir/f-2"), Status::Deleted(bcs_id)),
            (path("dir/sub"), Status::Deleted(bcs_id)),
            (path("dir/sub/f-1"), Status::Deleted(bcs_id)),
            (path("dir-2"), Status::Live),
            (path("dir-2/sub"), Status::Deleted(bcs_id)),
            (path("dir-2/sub/f-3"), Status::Deleted(bcs_id)),
            (path("file.txt"), Status::Deleted(bcs_id)),
        ];
        assert_eq!(deleted_nodes, expected_nodes);

        (bcs_id, mf_id)
    };

    // reincarnate file and directory
    let (bcs_id_3, mf_id_3) = {
        let file_changes = btreemap! {
            "file.txt" => Some("1\n1\n1\n"),
            "file-2.txt" => None,
            "dir/sub/f-4" => Some("4\n4\n4\n"),
        };
        let (bcs_id, mf_id, deleted_nodes) = create_cs_and_derive_manifest::<Root>(
            ctx.clone(),
            repo.clone(),
            file_changes,
            vec![(bcs_id_2, mf_id_2)],
        )
        .await?;

        let expected_nodes = vec![
            (MPath::ROOT, Status::Live),
            (path("dir"), Status::Live),
            (path("dir/f-2"), Status::Deleted(bcs_id_2)),
            (path("dir/sub"), Status::Live),
            (path("dir/sub/f-1"), Status::Deleted(bcs_id_2)),
            (path("dir-2"), Status::Live),
            (path("dir-2/sub"), Status::Deleted(bcs_id_2)),
            (path("dir-2/sub/f-3"), Status::Deleted(bcs_id_2)),
            (path("file-2.txt"), Status::Deleted(bcs_id)),
        ];
        assert_eq!(deleted_nodes, expected_nodes);

        (bcs_id, mf_id)
    };

    // reincarnate file as dir and dir as file
    let (bcs_id_4, mf_id_4) = {
        let file_changes = btreemap! {
            // file as dir
            "file-2.txt/subfile.txt" => Some("2\n2\n1\n"),
            // dir as file
            "dir-2/sub" => Some("file now!\n"),
        };
        let (bcs_id, mf_id, deleted_nodes) = create_cs_and_derive_manifest::<Root>(
            ctx.clone(),
            repo.clone(),
            file_changes,
            vec![(bcs_id_3, mf_id_3)],
        )
        .await?;

        let expected_nodes = vec![
            (MPath::ROOT, Status::Live),
            (path("dir"), Status::Live),
            (path("dir/f-2"), Status::Deleted(bcs_id_2)),
            (path("dir/sub"), Status::Live),
            (path("dir/sub/f-1"), Status::Deleted(bcs_id_2)),
            (path("dir-2"), Status::Live),
            (path("dir-2/sub"), Status::Live),
            (path("dir-2/sub/f-3"), Status::Deleted(bcs_id_2)),
        ];
        assert_eq!(deleted_nodes, expected_nodes);

        (bcs_id, mf_id)
    };

    // delete everything
    {
        let file_changes = btreemap! {
            "file.txt" => None,
            "file-2.txt/subfile.txt" => None,
            "file-3.txt" => None,
            "dir-2/f-4" => None,
            "dir-2/sub" => None,
            "dir/sub/f-4" => None,
        };
        let (bcs_id, mf_id, deleted_nodes) = create_cs_and_derive_manifest::<Root>(
            ctx.clone(),
            repo.clone(),
            file_changes,
            vec![(bcs_id_4, mf_id_4)],
        )
        .await?;

        let expected_nodes = vec![
            (MPath::ROOT, Status::Live),
            (path("dir"), Status::Deleted(bcs_id)),
            (path("dir/f-2"), Status::Deleted(bcs_id_2)),
            (path("dir/sub"), Status::Deleted(bcs_id)),
            (path("dir/sub/f-1"), Status::Deleted(bcs_id_2)),
            (path("dir/sub/f-4"), Status::Deleted(bcs_id)),
            (path("dir-2"), Status::Deleted(bcs_id)),
            (path("dir-2/f-4"), Status::Deleted(bcs_id)),
            (path("dir-2/sub"), Status::Deleted(bcs_id)),
            (path("dir-2/sub/f-3"), Status::Deleted(bcs_id_2)),
            (path("file-2.txt"), Status::Deleted(bcs_id)),
            (path("file-2.txt/subfile.txt"), Status::Deleted(bcs_id)),
            (path("file-3.txt"), Status::Deleted(bcs_id)),
            (path("file.txt"), Status::Deleted(bcs_id)),
        ];
        assert_eq!(deleted_nodes, expected_nodes);

        (bcs_id, mf_id)
    };
    Ok(())
}

pub(crate) async fn many_file_dirs_test<
    Root: RootDeletedManifestIdCommon<Manifest = DeletedManifestV2, Id = DeletedManifestV2Id>,
>(
    fb: FacebookInit,
) -> Result<(), Error> {
    let repo: TestRepo = build_repo(fb).await.unwrap();
    ManyFilesDirs::init_repo(fb, &repo).await?;
    let ctx = CoreContext::test_mock(fb);

    let mf_id_1 = {
        let hg_cs = "5a28e25f924a5d209b82ce0713d8d83e68982bc8";
        let (_, bcs) = bonsai_changeset_from_hg(&ctx, &repo, hg_cs).await.unwrap();

        let (_, mf_id, deleted_nodes) = derive_manifest::<Root>(&ctx, &repo, bcs, vec![]).await?;

        // nothing was deleted yet
        let expected_nodes = vec![(MPath::ROOT, Status::Live)];
        assert_eq!(deleted_nodes, expected_nodes);
        mf_id
    };

    let mf_id_2 = {
        let hg_cs = "2f866e7e549760934e31bf0420a873f65100ad63";
        let (_, bcs) = bonsai_changeset_from_hg(&ctx, &repo, hg_cs).await.unwrap();

        let (_, mf_id, deleted_nodes) =
            derive_manifest::<Root>(&ctx, &repo, bcs, vec![mf_id_1]).await?;

        // nothing was deleted yet
        let expected_nodes = vec![(MPath::ROOT, Status::Live)];
        assert_eq!(deleted_nodes, expected_nodes);
        mf_id
    };

    let mf_id_3 = {
        let hg_cs = "d261bc7900818dea7c86935b3fb17a33b2e3a6b4";
        let (_, bcs) = bonsai_changeset_from_hg(&ctx, &repo, hg_cs).await.unwrap();

        let (_, mf_id, deleted_nodes) =
            derive_manifest::<Root>(&ctx, &repo, bcs, vec![mf_id_2]).await?;

        // nothing was deleted yet
        let expected_nodes = vec![(MPath::ROOT, Status::Live)];
        assert_eq!(deleted_nodes, expected_nodes);
        mf_id
    };

    {
        let hg_cs = "051946ed218061e925fb120dac02634f9ad40ae2";
        let (bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, hg_cs).await.unwrap();

        let (_, mf_id, deleted_nodes) =
            derive_manifest::<Root>(&ctx, &repo, bcs, vec![mf_id_3]).await?;

        let expected_nodes = vec![
            (MPath::ROOT, Status::Live),
            (path("dir1"), Status::Live),
            (path("dir1/file_1_in_dir1"), Status::Deleted(bcs_id)),
            (path("dir1/file_2_in_dir1"), Status::Deleted(bcs_id)),
            (path("dir1/subdir1"), Status::Deleted(bcs_id)),
            (path("dir1/subdir1/file_1"), Status::Deleted(bcs_id)),
            (path("dir1/subdir1/subsubdir1"), Status::Deleted(bcs_id)),
            (
                path("dir1/subdir1/subsubdir1/file_1"),
                Status::Deleted(bcs_id),
            ),
            (path("dir1/subdir1/subsubdir2"), Status::Deleted(bcs_id)),
            (
                path("dir1/subdir1/subsubdir2/file_1"),
                Status::Deleted(bcs_id),
            ),
            (
                path("dir1/subdir1/subsubdir2/file_2"),
                Status::Deleted(bcs_id),
            ),
        ];
        assert_eq!(deleted_nodes, expected_nodes);
        mf_id
    };
    Ok(())
}

pub(crate) async fn merged_history_test<
    Root: RootDeletedManifestIdCommon<Manifest = DeletedManifestV2, Id = DeletedManifestV2Id>,
>(
    fb: FacebookInit,
) -> Result<(), Error> {
    let (repo, commits, _) = MergedDeletionHistory::get_repo_and_dag::<TestRepo>(fb).await;
    let ctx = CoreContext::test_mock(fb);
    for &(label, entries) in MergedDeletionHistory::DELETED_MANIFESTS {
        let actual = gen_deleted_manifest_nodes::<Root>(&ctx, &repo, commits[label]).await?;
        let expected = entries
            .iter()
            .map(|(path, deleted)| {
                Ok((
                    MPath::try_from(*path)?,
                    deleted.map(|label| commits[label]).into(),
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        assert_eq!(actual, expected, "commit {label}");
    }
    Ok(())
}

pub(crate) async fn test_find_entries<
    Root: RootDeletedManifestIdCommon<Manifest = DeletedManifestV2, Id = DeletedManifestV2Id>,
>(
    fb: FacebookInit,
) -> Result<(), Error> {
    // Test simple separate files and whole dir deletions
    let repo: TestRepo = build_repo(fb).await.unwrap();
    let ctx = CoreContext::test_mock(fb);

    // create parent deleted manifest
    let (bcs_id_1, mf_id_1) = {
        let file_changes = btreemap! {
            "file.txt" => Some("1\n"),
            "file-2.txt" => Some("2\n"),
            "dir/sub/f-1" => Some("3\n"),
            "dir/sub/f-6" => Some("3\n"),
            "dir/f-2" => Some("4\n"),
            "dir-2/sub/f-3" => Some("5\n"),
            "dir-2/f-4" => Some("6\n"),
            "dir-2/f-5" => Some("7\n"),
        };
        let files = store_files(&ctx, file_changes, &repo).await;
        let bcs = create_bonsai_changeset(ctx.fb, repo.clone(), files, vec![]).await;

        let bcs_id = bcs.get_changeset_id();
        let mf_id = derive_manifest::<Root>(&ctx, &repo, bcs, vec![]).await?.1;

        (bcs_id, mf_id)
    };

    // delete some files and dirs
    {
        let file_changes = btreemap! {
            "dir/sub/f-1" => None,
            "dir/sub/f-6" => None,
            "dir/f-2" => None,
            "dir-2/sub/f-3" => None,
            "dir-2/f-4" => None,
        };
        let files = store_files(&ctx, file_changes, &repo).await;
        let bcs = create_bonsai_changeset(ctx.fb, repo.clone(), files, vec![bcs_id_1]).await;

        let _bcs_id = bcs.get_changeset_id();
        let mf_id = derive_manifest::<Root>(&ctx, &repo, bcs, vec![mf_id_1])
            .await?
            .1;

        {
            // check that it will yield only two deleted paths
            let mut entries = Root::new(mf_id)
                .find_entries(
                    &ctx,
                    repo.repo_blobstore(),
                    vec![
                        PathOrPrefix::Path(path("file.txt")),
                        PathOrPrefix::Path(path("dir/f-2")),
                        PathOrPrefix::Path(path("dir/sub/f-1")),
                    ],
                )
                .map_ok(|(path, _)| path)
                .try_collect::<Vec<_>>()
                .await
                .unwrap();

            entries.sort();
            let expected_entries = [path("dir/f-2"), path("dir/sub/f-1")];
            assert_eq!(entries, expected_entries);
        }

        {
            // check that it will yield recursively all deleted paths including dirs
            let mut entries = Root::new(mf_id)
                .find_entries(
                    &ctx,
                    repo.repo_blobstore(),
                    vec![PathOrPrefix::Prefix(path("dir-2"))],
                )
                .map_ok(|(path, _)| path)
                .try_collect::<Vec<_>>()
                .await
                .unwrap();

            entries.sort();
            let expected_entries =
                vec![path("dir-2/f-4"), path("dir-2/sub"), path("dir-2/sub/f-3")];
            assert_eq!(entries, expected_entries);
        }

        {
            // check that it will yield recursively even having a path patterns
            let mut entries = Root::new(mf_id)
                .find_entries(
                    &ctx,
                    repo.repo_blobstore(),
                    vec![
                        PathOrPrefix::Prefix(path("dir/sub")),
                        PathOrPrefix::Path(path("dir/sub/f-1")),
                    ],
                )
                .map_ok(|(path, _)| path)
                .try_collect::<Vec<_>>()
                .await
                .unwrap();

            entries.sort();
            let expected_entries = vec![path("dir/sub"), path("dir/sub/f-1"), path("dir/sub/f-6")];
            assert_eq!(entries, expected_entries);
        }
    }
    Ok(())
}

pub(crate) async fn test_list_all_entries<
    Root: RootDeletedManifestIdCommon<Manifest = DeletedManifestV2, Id = DeletedManifestV2Id>,
>(
    fb: FacebookInit,
) -> Result<(), Error> {
    // Test simple separate files and whole dir deletions
    let repo: TestRepo = build_repo(fb).await.unwrap();
    let ctx = CoreContext::test_mock(fb);

    // create parent deleted manifest
    let (bcs_id_1, mf_id_1) = {
        let file_changes = btreemap! {
            "file.txt" => Some("1\n"),
            "dir/sub/f-1" => Some("3\n"),
            "dir/sub/f-3" => Some("3\n"),
            "dir/f-2" => Some("4\n"),
        };
        let files = store_files(&ctx, file_changes, &repo).await;
        let bcs = create_bonsai_changeset(ctx.fb, repo.clone(), files, vec![]).await;

        let bcs_id = bcs.get_changeset_id();
        let mf_id = derive_manifest::<Root>(&ctx, &repo, bcs, vec![]).await?.1;

        (bcs_id, mf_id)
    };

    {
        let file_changes = btreemap! {
            "dir/sub/f-1" => None,
            "dir/sub/f-3" => None,
        };
        let files = store_files(&ctx, file_changes, &repo).await;
        let bcs = create_bonsai_changeset(ctx.fb, repo.clone(), files, vec![bcs_id_1]).await;

        let _bcs_id = bcs.get_changeset_id();
        let mf_id = derive_manifest::<Root>(&ctx, &repo, bcs, vec![mf_id_1])
            .await?
            .1;

        {
            // check that it will yield only two deleted paths
            let entries = Root::new(mf_id)
                .list_all_entries(&ctx, repo.repo_blobstore())
                .try_collect::<Vec<_>>()
                .await
                .unwrap();

            let mut entries = entries
                .into_iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>();
            entries.sort();
            let expected_entries = vec![path("dir/sub"), path("dir/sub/f-1"), path("dir/sub/f-3")];
            assert_eq!(entries, expected_entries);
        }
    }
    Ok(())
}

async fn gen_deleted_manifest_nodes<
    Root: RootDeletedManifestIdCommon<Manifest = DeletedManifestV2, Id = DeletedManifestV2Id>,
>(
    ctx: &CoreContext,
    repo: &TestRepo,
    bonsai: ChangesetId,
) -> Result<Vec<(MPath, Status)>, Error> {
    let manifest = repo
        .repo_derived_data()
        .manager()
        .derive::<Root>(ctx, bonsai, None, DerivationPriority::LOW)
        .await?;
    let mut deleted_nodes = iterate_all_entries::<Root>(ctx.clone(), repo.clone(), *manifest.id())
        .map_ok(|(path, st, ..)| (path, st))
        .try_collect::<Vec<_>>()
        .await?;
    deleted_nodes.sort_by_key(|(path, ..)| path.clone());
    Ok(deleted_nodes)
}

async fn create_cs_and_derive_manifest<
    Root: RootDeletedManifestIdCommon<Manifest = DeletedManifestV2, Id = DeletedManifestV2Id>,
>(
    ctx: CoreContext,
    repo: TestRepo,
    file_changes: BTreeMap<&str, Option<&str>>,
    parent_ids: Vec<(ChangesetId, Root::Id)>,
) -> Result<(ChangesetId, Root::Id, Vec<(MPath, Status)>), Error> {
    let parent_bcs_ids = parent_ids
        .iter()
        .map(|(bs, _)| bs.clone())
        .collect::<Vec<_>>();
    let parent_mf_ids = parent_ids.into_iter().map(|(_, mf)| mf).collect::<Vec<_>>();

    let files = store_files(&ctx, file_changes, &repo).await;

    let bcs = create_bonsai_changeset(ctx.fb, repo.clone(), files, parent_bcs_ids).await;

    derive_manifest::<Root>(&ctx, &repo, bcs, parent_mf_ids).await
}

async fn derive_manifest<
    Root: RootDeletedManifestIdCommon<Manifest = DeletedManifestV2, Id = DeletedManifestV2Id>,
>(
    ctx: &CoreContext,
    repo: &TestRepo,
    bcs: BonsaiChangeset,
    parent_mf_ids: Vec<Root::Id>,
) -> Result<(ChangesetId, Root::Id, Vec<(MPath, Status)>), Error> {
    let blobstore = repo.repo_blobstore_arc() as Arc<dyn KeyedBlobstore>;
    let bcs_id = bcs.get_changeset_id();

    repo.repo_derived_data()
        .manager()
        .derive::<RootUnodeManifestId>(ctx, bcs.get_changeset_id(), None, DerivationPriority::LOW)
        .await?;

    let (current_unode, parent_unodes) = get_unodes(
        ctx,
        &repo.repo_derived_data().manager().derivation_context(None),
        &bcs,
    )
    .await?;
    let parents = bcs
        .parents()
        .zip(parent_mf_ids)
        .zip(parent_unodes)
        .map(|((parent, parent_dm), parent_unode)| (parent, parent_dm, parent_unode))
        .collect();
    let f = DeletedManifestDeriver::derive(ctx, &blobstore, bcs, parents, current_unode);

    let dfm_id = f.await.unwrap();
    // Make sure it's saved in the blobstore
    dfm_id.load(ctx, &blobstore).await.unwrap();

    let mut deleted_nodes = iterate_all_entries::<Root>(ctx.clone(), repo.clone(), dfm_id.clone())
        .map_ok(|(path, st, ..)| (path, st))
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    deleted_nodes.sort_by_key(|(path, ..)| path.clone());

    Ok((bcs_id, dfm_id, deleted_nodes))
}

async fn create_bonsai_changeset(
    fb: FacebookInit,
    repo: TestRepo,
    file_changes: SortedVectorMap<NonRootMPath, FileChange>,
    parents: Vec<ChangesetId>,
) -> BonsaiChangeset {
    let bcs = BonsaiChangesetMut {
        parents,
        author: "author".to_string(),
        author_date: DateTime::now(),
        message: "message".to_string(),
        file_changes,
        ..Default::default()
    }
    .freeze()
    .unwrap();

    save_changesets(&CoreContext::test_mock(fb), &repo, vec![bcs.clone()])
        .await
        .unwrap();
    bcs
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum Status {
    Deleted(ChangesetId),
    Live,
}

impl From<Option<ChangesetId>> for Status {
    fn from(linknode: Option<ChangesetId>) -> Self {
        linknode.map_or(Status::Live, Status::Deleted)
    }
}

fn iterate_all_entries<
    Root: RootDeletedManifestIdCommon<Manifest = DeletedManifestV2, Id = DeletedManifestV2Id>,
>(
    ctx: CoreContext,
    repo: TestRepo,
    manifest_id: Root::Id,
) -> impl Stream<Item = Result<(MPath, Status, Root::Id), Error>> {
    async_stream::stream! {
        let blobstore = repo.repo_blobstore();
        let s = bounded_traversal_stream(256, Some((MPath::ROOT, manifest_id)), move |(path, manifest_id)| {
            cloned!(ctx, blobstore);
            async move {
                let manifest = manifest_id.load(&ctx, &blobstore).await?;
                let entry = (
                    path.clone(),
                    Status::from(manifest.linknode().cloned()),
                    manifest_id,
                );
                let recurse_subentries = manifest
                    .into_subentries(&ctx, &blobstore)
                    .map_ok(|(name, mf_id)| {
                        let full_path = path.join_element(Some(&name));
                        (full_path, mf_id)
                    })
                    .try_collect::<Vec<_>>().await?;

                Result::<_, Error>::Ok((vec![entry], recurse_subentries))
            }.boxed()
        })
        .map_ok(|entries| iter(entries.into_iter().map(Ok)))
        .try_flatten();

        pin_mut!(s);
        while let Some(value) = s.next().await {
            yield value;
        }
    }
}

fn path(path_str: &str) -> MPath {
    MPath::new(path_str).unwrap()
}
