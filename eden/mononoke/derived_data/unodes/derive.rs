/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Error;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use borrowed::borrowed;
use cloned::cloned;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStageOutput;
use derived_data_manager::manifest_partitions::create_sorted_subentries;
use futures::future;
use futures::future::TryFutureExt;
use futures::future::try_join_all;
use manifest::Entry;
use manifest::LeafInfo;
use manifest::ManifestChanges;
use manifest::TreeInfo;
use manifest::derive_manifest;
use manifest::derive_manifests_for_simple_stack_of_commits;
use manifest::flatten_subentries;
use mononoke_types::BlobstoreKey;
use mononoke_types::BlobstoreValue;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::FileType;
use mononoke_types::FileUnodeId;
use mononoke_types::MPathElement;
use mononoke_types::MPathHash;
use mononoke_types::ManifestUnodeId;
use mononoke_types::NonRootMPath;
use mononoke_types::SortedVectorTrieMap;
use mononoke_types::path::MPath;
use mononoke_types::unode::FileUnode;
use mononoke_types::unode::ManifestUnode;
use mononoke_types::unode::UnodeEntry;
use sorted_vector_map::SortedVectorMap;

use crate::UnodeDerivationError;

pub(crate) async fn derive_unode_manifest_stack(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    file_changes: Vec<(
        ChangesetId,
        BTreeMap<NonRootMPath, Option<(ContentId, FileType)>>,
    )>,
    parent: Option<ManifestUnodeId>,
) -> Result<HashMap<ChangesetId, ManifestUnodeId>, Error> {
    let blobstore = derivation_ctx.blobstore();

    let manifest_changes = file_changes
        .into_iter()
        .map(|(cs_id, file_changes)| ManifestChanges {
            cs_id,
            changes: file_changes.into_iter().collect(),
        })
        .collect::<Vec<_>>();

    let res = derive_manifests_for_simple_stack_of_commits(
        ctx.clone(),
        blobstore.clone(),
        parent,
        manifest_changes,
        {
            cloned!(ctx, blobstore);
            move |tree_info, cs_id| {
                create_unode_manifest(ctx.clone(), cs_id, blobstore.clone(), tree_info)
            }
        },
        {
            cloned!(ctx, blobstore);
            move |leaf_info, cs_id| {
                create_unode_file(ctx.clone(), cs_id, blobstore.clone(), leaf_info)
            }
        },
    )
    .await?;

    Ok(res.into_iter().collect())
}

pub(crate) async fn derive_unode_entry(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    cs_id: ChangesetId,
    input: ManifestDerivationInput<
        ManifestUnodeId,
        FileUnodeId,
        SortedVectorTrieMap<Entry<ManifestUnodeId, FileUnodeId>>,
    >,
    changes: Vec<(NonRootMPath, Option<(ContentId, FileType)>)>,
) -> Result<
    ManifestStageOutput<
        ManifestUnodeId,
        FileUnodeId,
        SortedVectorTrieMap<Entry<ManifestUnodeId, FileUnodeId>>,
    >,
    Error,
> {
    let blobstore = derivation_ctx.blobstore();

    input
        .derive(
            ctx.clone(),
            blobstore.clone(),
            changes,
            {
                cloned!(ctx, blobstore);
                move |tree_info| {
                    create_unode_manifest(ctx.clone(), cs_id, blobstore.clone(), tree_info)
                }
            },
            {
                cloned!(ctx, blobstore);
                move |leaf_info| create_unode_file(ctx.clone(), cs_id, blobstore.clone(), leaf_info)
            },
            |subentries| create_sorted_subentries(ctx, subentries),
        )
        .await
}

/// Derives unode manifests for bonsai changeset `cs_id` given parent unode manifests.
/// Note that `derive_manifest()` does a lot of the heavy lifting for us, and this crate has to
/// provide only functions to create a single unode file or single unode tree (
/// `create_unode_manifest` and `create_unode_file`).
pub(crate) async fn derive_unode_manifest(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    cs_id: ChangesetId,
    parents: Vec<ManifestUnodeId>,
    changes: Vec<(NonRootMPath, Option<(ContentId, FileType)>)>,
) -> Result<ManifestUnodeId, Error> {
    let parents: Vec<_> = parents.into_iter().collect();
    let blobstore = derivation_ctx.blobstore();

    let maybe_tree_id = derive_manifest(
        ctx.clone(),
        blobstore.clone(),
        parents.clone(),
        changes,
        {
            cloned!(ctx, blobstore);
            move |tree_info| create_unode_manifest(ctx.clone(), cs_id, blobstore.clone(), tree_info)
        },
        {
            cloned!(ctx, blobstore);
            move |leaf_info| create_unode_file(ctx.clone(), cs_id, blobstore.clone(), leaf_info)
        },
    )
    .await?;

    match maybe_tree_id {
        Some(tree_id) => Ok(tree_id),
        None => {
            // All files have been deleted, generate empty **root** manifest
            let tree_info = TreeInfo {
                path: MPath::ROOT,
                parents,
                subentries: Default::default(),
            };
            let ((), tree_id) =
                create_unode_manifest(ctx.clone(), cs_id, blobstore.clone(), tree_info).await?;
            Ok(tree_id)
        }
    }
}

// Note that in some rare cases it's possible to have unode where one parent is ancestor of another
// (that applies both to files and directories)
//
//  Example:
//       4 o <- merge commit modifies file 'A'
//        / \
//     2 o ---> changed file 'A' content to 'B'
//       |   |
//       | 3 o -> changed some other file
//       \  /
//      1 o  <- created file 'A' with content 'A'
//
// In that case unode for file 'A' in a merge commit will have two parents - from commit '2' and
// from commit '1', and unode from commit '1' is ancestor of unode from commit '2'.
// Case like that might create slight confusion, however it should be rare and we should be
// able to fix it in the ui.
async fn create_unode_manifest(
    ctx: CoreContext,
    linknode: ChangesetId,
    blobstore: Arc<dyn KeyedBlobstore>,
    tree_info: TreeInfo<
        ManifestUnodeId,
        FileUnodeId,
        (),
        SortedVectorTrieMap<Entry<ManifestUnodeId, FileUnodeId>>,
    >,
) -> Result<((), ManifestUnodeId), Error> {
    let mut subentries = SortedVectorMap::new();
    for (basename, (_context, entry)) in flatten_subentries(&ctx, &(), tree_info.subentries).await?
    {
        match entry {
            Entry::Tree(mf_unode) => {
                subentries.insert(basename, UnodeEntry::Directory(mf_unode));
            }
            Entry::Leaf(file_unode) => {
                subentries.insert(basename, UnodeEntry::File(file_unode));
            }
        }
    }
    if tree_info.parents.len() > 1 {
        if let Some(mf_unode_id) =
            reuse_manifest_parent(&ctx, &blobstore, &tree_info.parents, &subentries).await?
        {
            return Ok(((), mf_unode_id));
        }
    }

    let mf_unode = ManifestUnode::new(tree_info.parents, subentries, linknode);
    let mf_unode_id = mf_unode.get_unode_id();

    let key = mf_unode_id.blobstore_key();
    let blob = mf_unode.into_blob();
    let f = async move { blobstore.put(&ctx, key, blob.into()).await };

    f.await?;
    Ok(((), mf_unode_id))
}

async fn create_unode_file(
    ctx: CoreContext,
    linknode: ChangesetId,
    blobstore: Arc<dyn KeyedBlobstore>,
    leaf_info: LeafInfo<FileUnodeId, (ContentId, FileType)>,
) -> Result<((), FileUnodeId), Error> {
    borrowed!(ctx, blobstore);

    async fn save_unode(
        ctx: &CoreContext,
        blobstore: &Arc<dyn KeyedBlobstore>,
        parents: Vec<FileUnodeId>,
        content_id: ContentId,
        file_type: FileType,
        path_hash: MPathHash,
        linknode: ChangesetId,
    ) -> Result<FileUnodeId, Error> {
        if parents.len() > 1 {
            if let Some(parent) =
                reuse_file_parent(ctx, blobstore, &parents, &content_id, file_type).await?
            {
                return Ok(parent);
            }
        };
        let file_unode = FileUnode::new(parents, content_id, file_type, path_hash, linknode);
        let file_unode_id = file_unode.get_unode_id();

        let f = {
            cloned!(ctx, blobstore);
            async move {
                blobstore
                    .put(
                        &ctx,
                        file_unode_id.blobstore_key(),
                        file_unode.into_blob().into(),
                    )
                    .await
            }
        };

        f.await?;
        Ok(file_unode_id)
    }

    let LeafInfo {
        change,
        path,
        parents,
    } = leaf_info;

    let leaf = if let Some((content_id, file_type)) = change {
        save_unode(
            ctx,
            blobstore,
            parents,
            content_id,
            file_type,
            path.get_path_hash(),
            linknode,
        )
        .await?
    } else {
        // We can end up in this codepath if there are at least 2 parent commits have a unode with
        // this file path, and these unodes are different, but current bonsai changeset have no
        // changes for this file path.
        //
        //  Example:
        //         o <- merge commit, it doesn't modify any of the files
        //        / \
        //       o ---> changed file 'A' content to 'B'
        //       |  |
        //       |   o -> changed file 'A content to 'B' as well
        //       \  /
        //        o  <- created file 'A' with content 'A'
        //
        // In that case we need to check file content and file type.
        // if they are the same then we need to create a new file unode.
        //
        // If content or file type are different then we need to return an error

        // Note that there's a difference from how we handle this case in mercurial manifests.
        // In mercurial manifests we compare file content with copy information, while in unodes
        // copy information is ignored. It might mean that some bonsai changesets would be
        // considered valid for unode manifests, but invalid for mercurial
        if parents.len() < 2 {
            return Err(UnodeDerivationError::InvalidBonsai(
                "no change is provided, but file unode has only one parent".to_string(),
            )
            .into());
        }
        let parent_unodes = try_join_all(
            parents
                .clone()
                .into_iter()
                .map(|id| async move { id.load(ctx, blobstore).await }),
        )
        .await?;

        match return_if_unique_filenode(&parent_unodes) {
            Some((content_id, file_type)) => {
                save_unode(
                    ctx,
                    blobstore,
                    parents,
                    content_id.clone(),
                    *file_type,
                    path.get_path_hash(),
                    linknode,
                )
                .await?
            }
            _ => {
                return Err(UnodeDerivationError::InvalidBonsai(
                    "no change is provided, but content is different".to_string(),
                )
                .into());
            }
        }
    };

    Ok(((), leaf))
}

// reuse_manifest_parent() and reuse_file_parent() are used in unodes v2 in order to avoid
// creating unnecessary unodes that were created in unodes v1. Consider this graph with a diamond merge:
//
//   O <- merge commit, doesn't change anything
//  / \
// P1  |  <- modified "file.txt"
// |   P2    <- created "other.txt"
// \  /
//  ROOT <- created "file.txt"
//
// In unodes v1 merge commit would contain a unode for "file.txt" which points to "file.txt"
// unode from P1 and and "file.txt" unode from ROOT. This is
// actually quite unexpected - merge commit didn't touch "file.txt" at all, but "file.txt"'s history
// would contain a merge commit.
//
// reuse_manifest_parent() and reuse_file_parent() are heuristics that help fix this problem.
// They check if content of the unode in the merge commit is the same as the content of unode
// in one of the parents. If yes, then this unode is reused. Note - it actually might also lead
// to surprising results in some cases:
//
//   O <- merge commit, doesn't change anything
//  / \
// P1  |  <- modified "file.txt" to "B"
// |   P2    <- modified "file.txt" to "B"
// \  /
//  ROOT <- created "file.txt" with content "A"
//
// In that case it would be ideal to preserve that "file.txt" was modified in both commits.
// But we consider this case is rare enough to not worry about it.

async fn reuse_manifest_parent(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    parents: &[ManifestUnodeId],
    subentries: &SortedVectorMap<MPathElement, UnodeEntry>,
) -> Result<Option<ManifestUnodeId>, Error> {
    let parents = future::try_join_all(
        parents
            .iter()
            .map(|id| id.load(ctx, blobstore).map_err(Error::from)),
    )
    .await?;

    if let Some(mf_unode) = parents.iter().find(|p| p.subentries() == subentries) {
        Ok(Some(mf_unode.get_unode_id()))
    } else {
        Ok(None)
    }
}

async fn reuse_file_parent(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    parents: &[FileUnodeId],
    content_id: &ContentId,
    file_type: FileType,
) -> Result<Option<FileUnodeId>, Error> {
    let parents = future::try_join_all(
        parents
            .iter()
            .map(|id| id.load(ctx, blobstore).map_err(Error::from)),
    )
    .await?;

    if let Some(file_unode) = parents
        .iter()
        .find(|p| p.content_id() == content_id && p.file_type() == &file_type)
    {
        Ok(Some(file_unode.get_unode_id()))
    } else {
        Ok(None)
    }
}

// If all elements in `unodes` are the same than this element is returned, otherwise None is returned
fn return_if_unique_filenode(unodes: &[FileUnode]) -> Option<(&ContentId, &FileType)> {
    let mut iter = unodes
        .iter()
        .map(|elem| (elem.content_id(), elem.file_type()));
    let first_elem = iter.next()?;
    if iter.all(|next_elem| next_elem == first_elem) {
        Some(first_elem)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::HashSet;
    use std::collections::VecDeque;

    use anyhow::Result;
    use async_trait::async_trait;
    use blobstore::Storable;
    use bytes::Bytes;
    use changesets_creation::save_changesets;
    use derivation_queue_thrift::DerivationPriority;
    use derived_data_test_utils::bonsai_changeset_from_hg;
    use derived_data_test_utils::iterate_all_manifest_entries;
    use fbinit::FacebookInit;
    use filenodes_derivation::FilenodesOnlyPublic;
    use fixtures::Linear;
    use fixtures::ManyFilesDirs;
    use fixtures::TestRepoFixture;
    use futures::TryStreamExt;
    use manifest::KnownEntry;
    use manifest::ManifestOps;
    use maplit::btreemap;
    use mercurial_derivation::DeriveHgChangeset;
    use mercurial_types::HgFileNodeId;
    use mercurial_types::HgManifestId;
    use mercurial_types::blobs::HgBlobManifest;
    use mononoke_macros::mononoke;
    use mononoke_types::BlobstoreValue;
    use mononoke_types::BonsaiChangeset;
    use mononoke_types::BonsaiChangesetMut;
    use mononoke_types::DateTime;
    use mononoke_types::FileChange;
    use mononoke_types::FileContents;
    use mononoke_types::GitLfs;
    use mononoke_types::RepoPath;
    use mononoke_types::path::MPath;
    use repo_derived_data::RepoDerivedDataRef;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;
    use tests_utils::resolve_cs_id;

    use super::*;
    use crate::mapping::RootUnodeManifestId;
    use crate::mapping::get_file_changes;
    use crate::tests::TestRepo;

    #[mononoke::fbinit_test]
    async fn linear_test(fb: FacebookInit) -> Result<(), Error> {
        let repo: TestRepo = Linear::get_repo(fb).await;
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
        let ctx = CoreContext::test_mock(fb);

        // Derive filenodes because they are going to be used in this test
        let master_cs_id = resolve_cs_id(&ctx, &repo, "master").await?;
        repo.repo_derived_data()
            .derive::<FilenodesOnlyPublic>(&ctx, master_cs_id, DerivationPriority::LOW)
            .await?;

        let parent_unode_id = {
            let parent_hg_cs = "2d7d4ba9ce0a6ffd222de7785b249ead9c51c536";
            let (bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, parent_hg_cs).await?;
            let unode_id = derive_unode_manifest(
                &ctx,
                &derivation_ctx,
                bcs_id,
                vec![],
                get_file_changes(&bcs),
            )
            .await?;

            // Make sure it's saved in the blobstore
            unode_id.load(&ctx, &repo.repo_blobstore).await?;
            let all_unodes: Vec<_> =
                iterate_all_manifest_entries(&ctx, &repo, Entry::Tree(unode_id))
                    .try_collect()
                    .await?;
            let mut paths: Vec<_> = all_unodes.into_iter().map(|(path, _)| path).collect();
            paths.sort();
            assert_eq!(
                paths,
                vec![
                    MPath::ROOT,
                    MPath::new("1").unwrap(),
                    MPath::new("files").unwrap()
                ]
            );
            unode_id
        };

        {
            let child_hg_cs = "3e0e761030db6e479a7fb58b12881883f9f8c63f";
            let (bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, child_hg_cs).await?;

            let unode_id = derive_unode_manifest(
                &ctx,
                &derivation_ctx,
                bcs_id,
                vec![parent_unode_id.clone()],
                get_file_changes(&bcs),
            )
            .await?;

            // Make sure it's saved in the blobstore
            let root_unode = unode_id.load(&ctx, &repo.repo_blobstore).await?;
            assert_eq!(root_unode.parents(), &vec![parent_unode_id]);

            let root_filenode_id = fetch_root_filenode_id(fb, repo.clone(), bcs_id).await?;
            assert_eq!(
                find_unode_history(fb, repo.clone(), UnodeEntry::Directory(unode_id)).await?,
                find_filenode_history(fb, repo.clone(), root_filenode_id).await?,
            );

            let all_unodes: Vec<_> =
                iterate_all_manifest_entries(&ctx, &repo, Entry::Tree(unode_id))
                    .try_collect()
                    .await?;
            let mut paths: Vec<_> = all_unodes.into_iter().map(|(path, _)| path).collect();
            paths.sort();
            assert_eq!(
                paths,
                vec![
                    MPath::ROOT,
                    MPath::new("1").unwrap(),
                    MPath::new("2").unwrap(),
                    MPath::new("files").unwrap()
                ]
            );
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_same_content_different_paths(fb: FacebookInit) -> Result<(), Error> {
        let repo: TestRepo = Linear::get_repo(fb).await;
        let ctx = CoreContext::test_mock(fb);

        async fn check_unode_uniqueness(
            ctx: CoreContext,
            repo: TestRepo,
            file_changes: BTreeMap<NonRootMPath, FileChange>,
        ) -> Result<(), Error> {
            let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
            let bcs = create_bonsai_changeset(ctx.fb, repo.clone(), file_changes).await?;
            let bcs_id = bcs.get_changeset_id();

            let unode_id = derive_unode_manifest(
                &ctx,
                &derivation_ctx,
                bcs_id,
                vec![],
                get_file_changes(&bcs),
            )
            .await?;
            let unode_mf = unode_id.load(&ctx, &repo.repo_blobstore).await?;

            // Unodes should be unique even if content is the same. Check it
            let vals: Vec<_> = unode_mf.list().collect();
            assert_eq!(vals.len(), 2);
            assert_ne!(vals.first(), vals.get(1));
            Ok(())
        }

        let file_changes = store_files(
            ctx.clone(),
            btreemap! {"file1" => Some(("content", FileType::Regular)), "file2" => Some(("content", FileType::Regular))},
            repo.clone(),
        ).await?;
        check_unode_uniqueness(ctx.clone(), repo.clone(), file_changes).await?;

        let file_changes = store_files(
            ctx.clone(),
            btreemap! {"dir1/file" => Some(("content", FileType::Regular)), "dir2/file" => Some(("content", FileType::Regular))},
            repo.clone(),
        ).await?;
        check_unode_uniqueness(ctx.clone(), repo.clone(), file_changes).await?;
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_same_content_no_change(fb: FacebookInit) -> Result<(), Error> {
        let repo: TestRepo = Linear::get_repo(fb).await;
        let ctx = CoreContext::test_mock(fb);

        build_diamond_graph(
            ctx.clone(),
            repo.clone(),
            btreemap! {"A" => Some(("A", FileType::Regular))},
            btreemap! {"A" => Some(("B", FileType::Regular))},
            btreemap! {"A" => Some(("B", FileType::Regular))},
            btreemap! {},
        )
        .await?;

        // Content is different - fail!
        assert!(
            build_diamond_graph(
                ctx.clone(),
                repo.clone(),
                btreemap! {"A" => Some(("A", FileType::Regular))},
                btreemap! {"A" => Some(("B", FileType::Regular))},
                btreemap! {"A" => Some(("C", FileType::Regular))},
                btreemap! {},
            )
            .await
            .is_err()
        );

        // Type is different - fail!
        assert!(
            build_diamond_graph(
                ctx,
                repo,
                btreemap! {"A" => Some(("A", FileType::Regular))},
                btreemap! {"A" => Some(("B", FileType::Regular))},
                btreemap! {"A" => Some(("B", FileType::Executable))},
                btreemap! {},
            )
            .await
            .is_err()
        );

        Ok(())
    }

    async fn diamond_merge_unodes_v2(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let factory = TestRepoFactory::new(fb)?;
        let repo: TestRepo = factory.build().await?;
        let merged_files = "dir/file.txt";
        let root_commit = CreateCommitContext::new_root(&ctx, &repo)
            .add_file(merged_files, "a")
            .commit()
            .await?;

        let p1 = CreateCommitContext::new(&ctx, &repo, vec![root_commit])
            .add_file("p1.txt", "p1")
            .add_file(merged_files, "b")
            .commit()
            .await?;

        let p2 = CreateCommitContext::new(&ctx, &repo, vec![root_commit])
            .add_file("p2.txt", "p2")
            .commit()
            .await?;

        let merge = CreateCommitContext::new(&ctx, &repo, vec![p1, p2])
            .add_file(merged_files, "b")
            .commit()
            .await?;

        let find_unodes = {
            |ctx: CoreContext, repo: TestRepo| async move {
                let p1_root_unode_mf_id = repo
                    .repo_derived_data
                    .derive::<RootUnodeManifestId>(&ctx, p1, DerivationPriority::LOW)
                    .await?;

                let mut p1_unodes: Vec<_> = p1_root_unode_mf_id
                    .manifest_unode_id()
                    .find_entries(
                        ctx.clone(),
                        repo.repo_blobstore.clone(),
                        vec![MPath::new(merged_files)?, MPath::new("dir")?],
                        // Some(NonRootMPath::new(&merged_files)?),
                    )
                    .try_collect()
                    .await?;
                p1_unodes.sort_by_key(|(path, _)| path.clone());

                let merge_root_unode_mf_id = repo
                    .repo_derived_data()
                    .derive::<RootUnodeManifestId>(&ctx, merge, DerivationPriority::LOW)
                    .await?;

                let mut merge_unodes: Vec<_> = merge_root_unode_mf_id
                    .manifest_unode_id()
                    .find_entries(
                        ctx.clone(),
                        repo.repo_blobstore.clone(),
                        vec![MPath::new(merged_files)?, MPath::new("dir")?],
                    )
                    .try_collect()
                    .await?;
                merge_unodes.sort_by_key(|(path, _)| path.clone());

                let res: Result<_, Error> = Ok((p1_unodes, merge_unodes));
                res
            }
        };

        // Unodes v2 should just reuse merged filenode
        let (p1_unodes, merge_unodes) = find_unodes(ctx.clone(), repo.clone()).await?;
        assert_eq!(p1_unodes, merge_unodes);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_diamond_merge_unodes_v2(fb: FacebookInit) -> Result<(), Error> {
        diamond_merge_unodes_v2(fb).await
    }

    #[mononoke::fbinit_test]
    async fn test_parent_order(fb: FacebookInit) -> Result<(), Error> {
        let repo: TestRepo = test_repo_factory::build_empty(fb).await.unwrap();
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
        let ctx = CoreContext::test_mock(fb);

        let p1_root_unode_id = create_changeset_and_derive_unode(
            ctx.clone(),
            repo.clone(),
            btreemap! {"A" => Some(("A", FileType::Regular))},
        )
        .await?;

        let p2_root_unode_id = create_changeset_and_derive_unode(
            ctx.clone(),
            repo.clone(),
            btreemap! {"A" => Some(("B", FileType::Regular))},
        )
        .await?;

        let file_changes = store_files(
            ctx.clone(),
            btreemap! { "A" => Some(("C", FileType::Regular)) },
            repo.clone(),
        )
        .await?;
        let bcs = create_bonsai_changeset(fb, repo.clone(), file_changes).await?;
        let bcs_id = bcs.get_changeset_id();

        let root_unode = derive_unode_manifest(
            &ctx,
            &derivation_ctx,
            bcs_id,
            vec![p1_root_unode_id, p2_root_unode_id],
            get_file_changes(&bcs),
        )
        .await?;

        // Make sure hash is the same if nothing was changed
        let same_root_unode = derive_unode_manifest(
            &ctx,
            &derivation_ctx,
            bcs_id,
            vec![p1_root_unode_id, p2_root_unode_id],
            get_file_changes(&bcs),
        )
        .await?;
        assert_eq!(root_unode, same_root_unode);

        // Now change parent order, make sure hashes are different
        let reverse_root_unode = derive_unode_manifest(
            &ctx,
            &derivation_ctx,
            bcs_id,
            vec![p2_root_unode_id, p1_root_unode_id],
            get_file_changes(&bcs),
        )
        .await?;

        assert_ne!(root_unode, reverse_root_unode);

        Ok(())
    }

    async fn create_changeset_and_derive_unode(
        ctx: CoreContext,
        repo: TestRepo,
        file_changes: BTreeMap<&str, Option<(&str, FileType)>>,
    ) -> Result<ManifestUnodeId, Error> {
        let file_changes = store_files(ctx.clone(), file_changes, repo.clone()).await?;
        let bcs = create_bonsai_changeset(ctx.fb, repo.clone(), file_changes).await?;

        let bcs_id = bcs.get_changeset_id();
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);

        derive_unode_manifest(
            &ctx,
            &derivation_ctx,
            bcs_id,
            vec![],
            get_file_changes(&bcs),
        )
        .await
    }

    async fn build_diamond_graph(
        ctx: CoreContext,
        repo: TestRepo,
        changes_first: BTreeMap<&str, Option<(&str, FileType)>>,
        changes_merge_p1: BTreeMap<&str, Option<(&str, FileType)>>,
        changes_merge_p2: BTreeMap<&str, Option<(&str, FileType)>>,
        changes_merge: BTreeMap<&str, Option<(&str, FileType)>>,
    ) -> Result<ManifestUnodeId> {
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
        let file_changes = store_files(ctx.clone(), changes_first, repo.clone()).await?;

        let bcs = create_bonsai_changeset(ctx.fb, repo.clone(), file_changes).await?;
        let first_bcs_id = bcs.get_changeset_id();

        let first_unode_id = derive_unode_manifest(
            &ctx,
            &derivation_ctx,
            first_bcs_id,
            vec![],
            get_file_changes(&bcs),
        )
        .await?;

        let (merge_p1, merge_p1_unode_id) = {
            let file_changes = store_files(ctx.clone(), changes_merge_p1, repo.clone()).await?;
            let merge_p1 = create_bonsai_changeset_with_params(
                ctx.fb,
                repo.clone(),
                file_changes.clone(),
                "merge_p1",
                vec![first_bcs_id.clone()],
            )
            .await?;
            let merge_p1_id = merge_p1.get_changeset_id();
            let merge_p1_unode_id = derive_unode_manifest(
                &ctx,
                &derivation_ctx,
                merge_p1_id,
                vec![first_unode_id.clone()],
                get_file_changes(&merge_p1),
            )
            .await?;
            (merge_p1, merge_p1_unode_id)
        };

        let (merge_p2, merge_p2_unode_id) = {
            let file_changes = store_files(ctx.clone(), changes_merge_p2, repo.clone()).await?;

            let merge_p2 = create_bonsai_changeset_with_params(
                ctx.fb,
                repo.clone(),
                file_changes,
                "merge_p2",
                vec![first_bcs_id.clone()],
            )
            .await?;
            let merge_p2_id = merge_p2.get_changeset_id();
            let merge_p2_unode_id = derive_unode_manifest(
                &ctx,
                &derivation_ctx,
                merge_p2_id,
                vec![first_unode_id.clone()],
                get_file_changes(&merge_p2),
            )
            .await?;
            (merge_p2, merge_p2_unode_id)
        };

        let file_changes = store_files(ctx.clone(), changes_merge, repo.clone()).await?;
        let merge = create_bonsai_changeset_with_params(
            ctx.fb,
            repo.clone(),
            file_changes,
            "merge",
            vec![merge_p1.get_changeset_id(), merge_p2.get_changeset_id()],
        )
        .await?;
        let merge_id = merge.get_changeset_id();
        derive_unode_manifest(
            &ctx,
            &derivation_ctx,
            merge_id,
            vec![merge_p1_unode_id, merge_p2_unode_id],
            get_file_changes(&merge),
        )
        .await
    }

    async fn create_bonsai_changeset(
        fb: FacebookInit,
        repo: TestRepo,
        file_changes: BTreeMap<NonRootMPath, FileChange>,
    ) -> Result<BonsaiChangeset, Error> {
        create_bonsai_changeset_with_params(fb, repo, file_changes, "message", vec![]).await
    }

    async fn create_bonsai_changeset_with_params(
        fb: FacebookInit,
        repo: TestRepo,
        file_changes: BTreeMap<NonRootMPath, FileChange>,
        message: &str,
        parents: Vec<ChangesetId>,
    ) -> Result<BonsaiChangeset, Error> {
        let bcs = BonsaiChangesetMut {
            parents,
            author: "author".to_string(),
            author_date: DateTime::now(),
            message: message.to_string(),
            hg_extra: Default::default(),
            file_changes: file_changes.into(),
            ..Default::default()
        }
        .freeze()
        .unwrap();

        save_changesets(&CoreContext::test_mock(fb), &repo, vec![bcs.clone()]).await?;
        Ok(bcs)
    }

    async fn store_files(
        ctx: CoreContext,
        files: BTreeMap<&str, Option<(&str, FileType)>>,
        repo: TestRepo,
    ) -> Result<BTreeMap<NonRootMPath, FileChange>, Error> {
        let mut res = btreemap! {};

        for (path, content) in files {
            let path = NonRootMPath::new(path).unwrap();
            match content {
                Some((content, file_type)) => {
                    let size = content.len();
                    let content =
                        FileContents::Bytes(Bytes::copy_from_slice(content.as_bytes())).into_blob();
                    let content_id = content.store(&ctx, &repo.repo_blobstore).await?;
                    let file_change = FileChange::tracked(
                        content_id,
                        file_type,
                        size as u64,
                        None,
                        GitLfs::FullContent,
                    );
                    res.insert(path, file_change);
                }
                None => {
                    res.insert(path, FileChange::Deletion);
                }
            }
        }
        Ok(res)
    }

    #[async_trait]
    trait UnodeHistory {
        async fn get_parents<'a>(
            &'a self,
            ctx: &'a CoreContext,
            repo: &'a TestRepo,
        ) -> Result<Vec<UnodeEntry>, Error>;

        async fn get_linknode<'a>(
            &'a self,
            ctx: &'a CoreContext,
            repo: &'a TestRepo,
        ) -> Result<ChangesetId, Error>;
    }

    #[async_trait]
    impl UnodeHistory for UnodeEntry {
        async fn get_parents<'a>(
            &'a self,
            ctx: &'a CoreContext,
            repo: &'a TestRepo,
        ) -> Result<Vec<UnodeEntry>, Error> {
            match self {
                UnodeEntry::File(file_unode_id) => {
                    let unode_mf = file_unode_id.load(ctx, &repo.repo_blobstore).await?;
                    Ok(unode_mf
                        .parents()
                        .iter()
                        .cloned()
                        .map(UnodeEntry::File)
                        .collect())
                }
                UnodeEntry::Directory(mf_unode_id) => {
                    let unode_mf = mf_unode_id.load(ctx, &repo.repo_blobstore).await?;
                    Ok(unode_mf
                        .parents()
                        .iter()
                        .cloned()
                        .map(UnodeEntry::Directory)
                        .collect())
                }
            }
        }

        async fn get_linknode<'a>(
            &'a self,
            ctx: &'a CoreContext,
            repo: &'a TestRepo,
        ) -> Result<ChangesetId, Error> {
            match self {
                UnodeEntry::File(file_unode_id) => {
                    let unode_file = file_unode_id.load(ctx, &repo.repo_blobstore).await?;
                    Ok(unode_file.linknode().clone())
                }
                UnodeEntry::Directory(mf_unode_id) => {
                    let unode_mf = mf_unode_id.load(ctx, &repo.repo_blobstore).await?;
                    Ok(unode_mf.linknode().clone())
                }
            }
        }
    }

    async fn find_unode_history(
        fb: FacebookInit,
        repo: TestRepo,
        start: UnodeEntry,
    ) -> Result<Vec<ChangesetId>, Error> {
        let ctx = CoreContext::test_mock(fb);
        let mut q = VecDeque::new();
        q.push_back(start.clone());

        let mut visited = HashSet::new();
        visited.insert(start);
        let mut history = vec![];
        loop {
            let unode_entry = q.pop_front();
            let unode_entry = match unode_entry {
                Some(unode_entry) => unode_entry,
                None => {
                    break;
                }
            };
            let linknode = unode_entry.get_linknode(&ctx, &repo).await?;
            history.push(linknode);
            let parents = unode_entry.get_parents(&ctx, &repo).await?;
            q.extend(parents.into_iter().filter(|x| visited.insert(x.clone())));
        }

        Ok(history)
    }

    async fn find_filenode_history(
        fb: FacebookInit,
        repo: TestRepo,
        start: HgFileNodeId,
    ) -> Result<Vec<ChangesetId>, Error> {
        let ctx = CoreContext::test_mock(fb);

        let mut q = VecDeque::new();
        q.push_back(start);

        let mut visited = HashSet::new();
        visited.insert(start);
        let mut history = vec![];
        loop {
            let filenode_id = q.pop_front();
            let filenode_id = match filenode_id {
                Some(filenode_id) => filenode_id,
                None => {
                    break;
                }
            };

            let hg_linknode = repo
                .filenodes
                .get_filenode(&ctx.clone(), &RepoPath::RootPath, filenode_id)
                .await?
                .do_not_handle_disabled_filenodes()?
                .map(|filenode| filenode.linknode)
                .unwrap();

            let linknode = repo
                .bonsai_hg_mapping
                .get_bonsai_from_hg(&ctx, hg_linknode)
                .await?
                .unwrap();
            history.push(linknode);

            let mf = HgBlobManifest::load(
                &ctx,
                &repo.repo_blobstore,
                HgManifestId::new(filenode_id.into_nodehash()),
            )
            .await?
            .unwrap();

            q.extend(
                mf.p1()
                    .into_iter()
                    .map(HgFileNodeId::new)
                    .filter(|x| visited.insert(*x)),
            );
            q.extend(
                mf.p2()
                    .into_iter()
                    .map(HgFileNodeId::new)
                    .filter(|x| visited.insert(*x)),
            );
        }

        Ok(history)
    }

    async fn fetch_root_filenode_id(
        fb: FacebookInit,
        repo: TestRepo,
        bcs_id: ChangesetId,
    ) -> Result<HgFileNodeId, Error> {
        let ctx = CoreContext::test_mock(fb);
        let hg_cs_id = repo.derive_hg_changeset(&ctx, bcs_id).await?;
        let hg_cs = hg_cs_id.load(&ctx, &repo.repo_blobstore).await?;
        Ok(HgFileNodeId::new(hg_cs.manifestid().into_nodehash()))
    }

    /// Derivation pipeline of independent directories: derive dir1/ and dir2/
    /// separately, then assemble the root using known_entries. Verifies the
    /// result matches normal full-tree derivation.
    #[mononoke::fbinit_test]
    async fn test_derivation_pipeline_independent_dirs(fb: FacebookInit) {
        let repo: TestRepo = ManyFilesDirs::get_repo(fb).await;
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
        let ctx = CoreContext::test_mock(fb);
        let blobstore = repo.repo_blobstore.clone();

        // Derive parent unode (commit A: creates "1")
        let parent_unode_id = {
            let parent_hg_cs = "5a28e25f924a5d209b82ce0713d8d83e68982bc8";
            let (bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, parent_hg_cs)
                .await
                .unwrap();
            derive_unode_manifest(
                &ctx,
                &derivation_ctx,
                bcs_id,
                vec![],
                get_file_changes(&bcs),
            )
            .await
            .unwrap()
        };

        // Commit B adds: "2", "dir1/file_1_in_dir1", "dir1/file_2_in_dir1",
        // "dir1/subdir1/file_1", "dir2/file_1_in_dir2"
        let child_hg_cs = "2f866e7e549760934e31bf0420a873f65100ad63";
        let (bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, child_hg_cs)
            .await
            .unwrap();
        let changes = get_file_changes(&bcs);

        // Normal derivation to get expected result
        let expected_id = derive_unode_manifest(
            &ctx,
            &derivation_ctx,
            bcs_id,
            vec![parent_unode_id],
            changes.clone(),
        )
        .await
        .unwrap();

        // Stage dir1: pass all changes with prefix
        let dir1_parent = parent_unode_id
            .find_entry(ctx.clone(), blobstore.clone(), MPath::new("dir1").unwrap())
            .await
            .unwrap();
        let dir1_parents = vec![dir1_parent];
        let dir1_entry = derive_unode_entry(
            &ctx,
            &derivation_ctx,
            bcs_id,
            ManifestDerivationInput::Entry {
                parents: dir1_parents,
                known_entries: HashMap::new(),
                path: MPath::new("dir1").unwrap(),
            },
            changes.clone(),
        )
        .await
        .unwrap()
        .into_entry()
        .unwrap()
        .expect("dir1 stage should produce an entry");

        // Stage dir2: pass all changes with prefix
        let dir2_parent = parent_unode_id
            .find_entry(ctx.clone(), blobstore.clone(), MPath::new("dir2").unwrap())
            .await
            .unwrap();
        let dir2_parents = vec![dir2_parent];
        let dir2_entry = derive_unode_entry(
            &ctx,
            &derivation_ctx,
            bcs_id,
            ManifestDerivationInput::Entry {
                parents: dir2_parents,
                known_entries: HashMap::new(),
                path: MPath::new("dir2").unwrap(),
            },
            changes.clone(),
        )
        .await
        .unwrap()
        .into_entry()
        .unwrap()
        .expect("dir2 stage should produce an entry");

        // Stage root: assemble with known_entries for dir1 and dir2
        let known_entries = HashMap::from([
            (
                MPath::new("dir1").unwrap(),
                KnownEntry::Entry(Some(dir1_entry)),
            ),
            (
                MPath::new("dir2").unwrap(),
                KnownEntry::Entry(Some(dir2_entry)),
            ),
        ]);
        let root_entry = derive_unode_entry(
            &ctx,
            &derivation_ctx,
            bcs_id,
            ManifestDerivationInput::Entry {
                parents: vec![Some(Entry::Tree(parent_unode_id))],
                known_entries,
                path: MPath::ROOT,
            },
            changes,
        )
        .await
        .unwrap()
        .into_entry()
        .unwrap()
        .expect("root stage should produce an entry");

        let pipeline_id = root_entry.into_tree().expect("root entry should be a tree");

        // Unode IDs should match because both paths use the same cs_id,
        // so linknodes, parents, and subentries are identical.
        assert_eq!(pipeline_id, expected_id);
    }

    /// Derivation pipeline of nested directories: derive dir1/subdir1/ first,
    /// then dir1/ using known_entries for subdir1, then root using known_entries
    /// for dir1. Verifies the result matches normal full-tree derivation.
    #[mononoke::fbinit_test]
    async fn test_derivation_pipeline_nested_dirs(fb: FacebookInit) {
        let repo: TestRepo = ManyFilesDirs::get_repo(fb).await;
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
        let ctx = CoreContext::test_mock(fb);
        let blobstore = repo.repo_blobstore.clone();

        // Derive parent unodes for commits A and B
        let parent_unode_id = {
            let hg_cs_a = "5a28e25f924a5d209b82ce0713d8d83e68982bc8";
            let (bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, hg_cs_a)
                .await
                .unwrap();
            derive_unode_manifest(
                &ctx,
                &derivation_ctx,
                bcs_id,
                vec![],
                get_file_changes(&bcs),
            )
            .await
            .unwrap()
        };
        let parent_unode_id = {
            let hg_cs_b = "2f866e7e549760934e31bf0420a873f65100ad63";
            let (bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, hg_cs_b)
                .await
                .unwrap();
            derive_unode_manifest(
                &ctx,
                &derivation_ctx,
                bcs_id,
                vec![parent_unode_id],
                get_file_changes(&bcs),
            )
            .await
            .unwrap()
        };

        // Commit C modifies: "dir1/subdir1/subsubdir1/file_1",
        // "dir1/subdir1/subsubdir2/file_1", "dir1/subdir1/subsubdir2/file_2"
        let child_hg_cs = "d261bc7900818dea7c86935b3fb17a33b2e3a6b4";
        let (bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, child_hg_cs)
            .await
            .unwrap();
        let changes = get_file_changes(&bcs);

        // Normal derivation to get expected result
        let expected_id = derive_unode_manifest(
            &ctx,
            &derivation_ctx,
            bcs_id,
            vec![parent_unode_id],
            changes.clone(),
        )
        .await
        .unwrap();

        // Stage dir1/subdir1: pass all changes with prefix
        let subdir1_parent = parent_unode_id
            .find_entry(
                ctx.clone(),
                blobstore.clone(),
                MPath::new("dir1/subdir1").unwrap(),
            )
            .await
            .unwrap();
        let subdir1_parents = vec![subdir1_parent];
        let subdir1_entry = derive_unode_entry(
            &ctx,
            &derivation_ctx,
            bcs_id,
            ManifestDerivationInput::Entry {
                parents: subdir1_parents,
                known_entries: HashMap::new(),
                path: MPath::new("dir1/subdir1").unwrap(),
            },
            changes.clone(),
        )
        .await
        .unwrap()
        .into_entry()
        .unwrap()
        .expect("dir1/subdir1 stage should produce an entry");

        // Stage dir1: pass all changes with prefix, known_entries for subdir1
        let dir1_parent = parent_unode_id
            .find_entry(ctx.clone(), blobstore.clone(), MPath::new("dir1").unwrap())
            .await
            .unwrap();
        let dir1_parents = vec![dir1_parent];
        let dir1_known_entries = HashMap::from([(
            MPath::new("dir1/subdir1").unwrap(),
            KnownEntry::Entry(Some(subdir1_entry)),
        )]);
        let dir1_entry = derive_unode_entry(
            &ctx,
            &derivation_ctx,
            bcs_id,
            ManifestDerivationInput::Entry {
                parents: dir1_parents,
                known_entries: dir1_known_entries,
                path: MPath::new("dir1").unwrap(),
            },
            changes.clone(),
        )
        .await
        .unwrap()
        .into_entry()
        .unwrap()
        .expect("dir1 stage should produce an entry");

        // Stage root: assemble with known_entries for dir1
        let root_known_entries = HashMap::from([(
            MPath::new("dir1").unwrap(),
            KnownEntry::Entry(Some(dir1_entry)),
        )]);
        let root_entry = derive_unode_entry(
            &ctx,
            &derivation_ctx,
            bcs_id,
            ManifestDerivationInput::Entry {
                parents: vec![Some(Entry::Tree(parent_unode_id))],
                known_entries: root_known_entries,
                path: MPath::ROOT,
            },
            changes,
        )
        .await
        .unwrap()
        .into_entry()
        .unwrap()
        .expect("root stage should produce an entry");

        let pipeline_id = root_entry.into_tree().expect("root entry should be a tree");

        assert_eq!(pipeline_id, expected_id);
    }
}
