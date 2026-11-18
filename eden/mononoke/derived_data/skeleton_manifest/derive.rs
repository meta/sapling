/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use borrowed::borrowed;
use cloned::cloned;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStageOutput;
use derived_data_manager::manifest_partitions::create_sorted_subentries;
use futures::future::FutureExt;
use futures::stream::FuturesOrdered;
use futures::stream::FuturesUnordered;
use futures::stream::TryStreamExt;
use manifest::Entry;
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
use mononoke_types::MPathElement;
use mononoke_types::NonRootMPath;
use mononoke_types::SkeletonManifestId;
use mononoke_types::SortedVectorTrieMap;
use mononoke_types::path::MPath;
use mononoke_types::skeleton_manifest::SkeletonManifest;
use mononoke_types::skeleton_manifest::SkeletonManifestDirectory;
use mononoke_types::skeleton_manifest::SkeletonManifestEntry;
use mononoke_types::skeleton_manifest::SkeletonManifestSummary;
use sorted_vector_map::SortedVectorMap;

use crate::SkeletonManifestDerivationError;

pub(crate) async fn derive_skeleton_manifest_stack(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    file_changes: Vec<(
        ChangesetId,
        BTreeMap<NonRootMPath, Option<(ContentId, FileType)>>,
    )>,
    parent: Option<SkeletonManifestId>,
) -> Result<HashMap<ChangesetId, SkeletonManifestId>, Error> {
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
            cloned!(blobstore, ctx);
            move |tree_info, _cs_id| {
                cloned!(blobstore, ctx);
                async move { create_skeleton_manifest(&ctx, &blobstore, tree_info).await }
            }
        },
        |_leaf_info, _cs_id| async { Ok((None, ())) },
    )
    .await?;

    Ok(res.into_iter().collect())
}

pub(crate) async fn derive_skeleton_manifest(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    parents: Vec<SkeletonManifestId>,
    changes: Vec<(NonRootMPath, Option<(ContentId, FileType)>)>,
) -> Result<SkeletonManifestId> {
    let blobstore = derivation_ctx.blobstore();

    // We must box and store the derivation future, otherwise lifetime
    // analysis is unable to see that the blobstore lasts long enough.
    let derive_fut = derive_manifest(
        ctx.clone(),
        blobstore.clone(),
        parents.clone(),
        changes,
        {
            cloned!(blobstore, ctx);
            move |tree_info| {
                cloned!(blobstore, ctx);
                async move { create_skeleton_manifest(&ctx, &blobstore, tree_info).await }
            }
        },
        |_leaf_info| async { Ok((None, ())) },
    )
    .boxed();
    let maybe_tree_id = derive_fut.await?;

    match maybe_tree_id {
        Some(tree_id) => Ok(tree_id),
        None => {
            // All files have been deleted, generate empty fsnode
            let tree_info = TreeInfo {
                path: MPath::ROOT,
                parents,
                subentries: Default::default(),
            };
            let (_, tree_id) = create_skeleton_manifest(ctx, blobstore, tree_info).await?;
            Ok(tree_id)
        }
    }
}

pub(crate) async fn derive_skeleton_manifest_entry(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    input: ManifestDerivationInput<
        SkeletonManifestId,
        (),
        SortedVectorTrieMap<Entry<SkeletonManifestId, ()>>,
    >,
    changes: Vec<(NonRootMPath, Option<(ContentId, FileType)>)>,
) -> Result<
    ManifestStageOutput<SkeletonManifestId, (), SortedVectorTrieMap<Entry<SkeletonManifestId, ()>>>,
> {
    let blobstore = derivation_ctx.blobstore();
    input
        .derive(
            ctx.clone(),
            blobstore.clone(),
            changes,
            {
                cloned!(blobstore, ctx);
                move |tree_info| {
                    cloned!(blobstore, ctx);
                    async move { create_skeleton_manifest(&ctx, &blobstore, tree_info).await }
                }
            },
            |_leaf_info| async { Ok((None, ())) },
            |subentries| create_sorted_subentries(ctx, subentries),
        )
        .await
}

/// Collect all the subentries for a new skeleton manifest, reusing entries
/// from the parent skeleton manifests to avoid fetching too much.
async fn collect_skeleton_subentries(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    parents: &[SkeletonManifestId],
    subentries: BTreeMap<
        MPathElement,
        (
            Option<Option<SkeletonManifestSummary>>,
            Entry<SkeletonManifestId, ()>,
        ),
    >,
) -> Result<Vec<(MPathElement, SkeletonManifestEntry)>> {
    // Load the parent skeleton manifests and collect their entries into a cache
    let mut dir_cache = HashMap::new();
    let mut parent_skeletons = parents
        .iter()
        .map({
            move |skeleton_id| async move {
                skeleton_id
                    .load(ctx, blobstore)
                    .await
                    .context(SkeletonManifestDerivationError::MissingParent(*skeleton_id))
            }
        })
        .collect::<FuturesUnordered<_>>();
    while let Some(parent_skeleton) = parent_skeletons.try_next().await? {
        for (_elem, entry) in parent_skeleton.list() {
            if let SkeletonManifestEntry::Directory(dir) = entry {
                dir_cache.entry(*dir.id()).or_insert_with(|| dir.clone());
            }
        }
    }

    // Find (from the traversal or the cache) or fetch (from the blobstore)
    // the `SkeletonManifestEntry` for each of the subentries.
    borrowed!(dir_cache);
    subentries
        .into_iter()
        .map(move |(elem, (summary, entry))| {
            async move {
                match entry {
                    Entry::Tree(skeleton_id) => {
                        if let Some(Some(summary)) = summary {
                            // The subdirectory was just created. Use the
                            // summary we just calculated.
                            Ok((
                                elem.clone(),
                                SkeletonManifestEntry::Directory(SkeletonManifestDirectory::new(
                                    skeleton_id,
                                    summary,
                                )),
                            ))
                        } else if let Some(entry) = dir_cache.get(&skeleton_id) {
                            // The subdirectory was already in this
                            // directory. Use the cached entry.
                            Ok((
                                elem.clone(),
                                SkeletonManifestEntry::Directory(entry.clone()),
                            ))
                        } else {
                            // Some other directory is being used. Fetch its
                            // summary from the blobstore.
                            let skeleton_manifest =
                                skeleton_id.load(ctx, blobstore).await.with_context({
                                    || {
                                        SkeletonManifestDerivationError::MissingSubentry(
                                            String::from_utf8_lossy(elem.as_ref()).to_string(),
                                            skeleton_id,
                                        )
                                    }
                                })?;

                            let entry =
                                SkeletonManifestEntry::Directory(SkeletonManifestDirectory::new(
                                    skeleton_id,
                                    skeleton_manifest.summary().clone(),
                                ));
                            Ok((elem.clone(), entry))
                        }
                    }
                    Entry::Leaf(()) => Ok((elem.clone(), SkeletonManifestEntry::File)),
                }
            }
        })
        .collect::<FuturesOrdered<_>>()
        .try_collect()
        .await
}

/// Create a new skeleton manifest for the tree described by `tree_info`.
async fn create_skeleton_manifest(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    tree_info: TreeInfo<
        SkeletonManifestId,
        (),
        Option<SkeletonManifestSummary>,
        SortedVectorTrieMap<Entry<SkeletonManifestId, ()>>,
    >,
) -> Result<(Option<SkeletonManifestSummary>, SkeletonManifestId)> {
    let entries = collect_skeleton_subentries(
        ctx,
        blobstore,
        &tree_info.parents,
        flatten_subentries(ctx, &(), tree_info.subentries)
            .await?
            .collect(),
    )
    .await?;

    // Build a summary of the entries and store it as the new skeleton
    // manifest.
    let entries: SortedVectorMap<_, _> = entries.into_iter().collect();
    let mut summary = SkeletonManifestSummary {
        child_files_count: 0,
        descendant_files_count: 0,
        child_dirs_count: 0,
        descendant_dirs_count: 0,
        max_path_len: 0,
        max_path_wchar_len: 0,
        child_case_conflicts: false,
        descendant_case_conflicts: false,
        child_non_utf8_filenames: false,
        descendant_non_utf8_filenames: false,
        child_invalid_windows_filenames: false,
        descendant_invalid_windows_filenames: false,
    };
    let mut lower_entries = HashSet::new();
    for (elem, entry) in entries.iter() {
        let elem_len = elem.len() as u32;
        let elem_wchar_len = elem.wchar_len() as u32;
        match entry {
            SkeletonManifestEntry::Directory(dir) => {
                let sub = dir.summary();
                summary.descendant_files_count += sub.descendant_files_count;
                summary.child_dirs_count += 1;
                summary.descendant_dirs_count += sub.descendant_dirs_count + 1;
                summary.max_path_len = summary.max_path_len.max(elem_len + 1 + sub.max_path_len);
                summary.max_path_wchar_len = summary
                    .max_path_wchar_len
                    .max(elem_wchar_len + 1 + sub.max_path_wchar_len);
                summary.descendant_case_conflicts |=
                    sub.child_case_conflicts | sub.descendant_case_conflicts;
                summary.descendant_non_utf8_filenames |=
                    sub.child_non_utf8_filenames | sub.descendant_non_utf8_filenames;
                summary.descendant_invalid_windows_filenames |=
                    sub.child_invalid_windows_filenames | sub.descendant_invalid_windows_filenames;
            }
            SkeletonManifestEntry::File => {
                summary.descendant_files_count += 1;
                summary.child_files_count += 1;
                summary.max_path_len = summary.max_path_len.max(elem_len);
                summary.max_path_wchar_len = summary.max_path_wchar_len.max(elem_wchar_len);
            }
        }
        if !summary.child_case_conflicts {
            if let Some(lower) = elem.to_lowercase_utf8() {
                if !lower_entries.insert(lower) {
                    // A case conflict has been found.
                    summary.child_case_conflicts = true;
                }
            }
        }
        if !summary.child_non_utf8_filenames && !elem.is_utf8() {
            // A non-utf8 filename has been found.
            summary.child_non_utf8_filenames = true;
        }
        if !summary.child_invalid_windows_filenames && !elem.is_valid_windows_filename() {
            // An invalid windows filename has been found.
            summary.child_invalid_windows_filenames = true;
        }
    }
    let skeleton_manifest = SkeletonManifest::new(entries, summary.clone());
    let blob = skeleton_manifest.into_blob();
    let skeleton_manifest_id = *blob.id();

    // Skeleton manifests are frequently unchanged.  If any of our parents
    // have the same ID, there is no need to store this blob.
    if !tree_info.parents.contains(&skeleton_manifest_id) {
        let key = skeleton_manifest_id.blobstore_key();
        let f = {
            cloned!(ctx, blobstore);
            async move { blobstore.put(&ctx, key, blob.into()).await }
        };

        f.await?;
    }
    Ok((Some(summary), skeleton_manifest_id))
}

#[cfg(test)]
mod test {
    use anyhow::anyhow;
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use manifest::KnownEntry;
    use manifest::ManifestOps;
    use mononoke_macros::mononoke;
    use mononoke_types::ChangesetId;
    use mononoke_types::PrefixTrie;
    use pretty_assertions::assert_eq;
    use repo_blobstore::RepoBlobstore;
    use repo_blobstore::RepoBlobstoreRef;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;
    use tests_utils::CreateCommitContext;
    use tests_utils::drawdag::changes;
    use tests_utils::drawdag::create_from_dag_with_changes;

    use super::*;
    use crate::mapping::get_file_changes;

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

    const B_FILES: &[&str] = &[
        "dir1/subdir1/subsubdir1/file1",
        "dir1/subdir1/subsubdir2/file2",
        "dir1/subdir1/subsubdir3/file3",
        "dir1/subdir2/subsubdir1/file1",
        "dir1/subdir2/subsubdir2/file1",
        "dir2/subdir1/subsubdir1/file1",
    ];

    const C_FILES: &[&str] = &[
        "dir1/subdir1/subsubdir1/FILE1",
        "dir1/subdir1/SUBSUBDIR2/file2",
        "dir1/subdir1/SUBSUBDIR3/FILE3",
        "dir1/subdir2/SUBSUBDIR1/file1",
    ];

    const D_FILES: &[&str] = &[
        "dir1/subdir1/subsubdir2/file2",
        "dir1/subdir1/SUBSUBDIR3/FILE3",
    ];

    const E_FILES: &[&str] = &[
        "dir1/subdir1/subsubdir1/FILE1",
        "dir1/subdir2/subsubdir1/file1",
    ];

    const G_FILES: &[&str] = &[
        "windows/dirnamewith\u{00E9}/filenamewith\u{9854}",
        "nonwin/dir1/com7",
        "nonwin/dir2/PRN.txt",
        "nonwin/dir3/Nul.JPG",
        "nonwin/dir4/enddot.",
        "nonwin/dir5/endspace ",
        "nonwin/dir6/ctrl\tchars.txt",
    ];

    const H_FILES: &[&str] = &["dir1/subdir1/SUBSUBDIR3/File3"];

    const J_FILES: &[&str] = &["dir3/FILE1", "dir3/file1"];

    const K_FILES: &[&str] = &["dir3/file2"];

    const L_FILES: &[&str] = &["dir2/subdir1/subsubdir1/FILE1"];

    fn add_files<'a>(
        mut c: CreateCommitContext<'a, TestRepo>,
        files: &[&str],
    ) -> CreateCommitContext<'a, TestRepo> {
        for &file in files {
            c = c.add_file(file, file);
        }
        c
    }

    fn delete_files<'a>(
        mut c: CreateCommitContext<'a, TestRepo>,
        files: &[&str],
    ) -> CreateCommitContext<'a, TestRepo> {
        for &file in files {
            c = c.delete_file(file);
        }
        c
    }

    async fn init_repo(ctx: &CoreContext) -> Result<(TestRepo, BTreeMap<String, ChangesetId>)> {
        let repo: TestRepo = test_repo_factory::build_empty(ctx.fb).await.unwrap();
        let changesets = create_from_dag_with_changes(
            ctx,
            &repo,
            r"
                    J-K
                     /
                A-B-C-D-E-F-G
                     \
                      H
                       \
                      L-M
            ",
            changes! {
                "B" => |c| add_files(c, B_FILES),
                "C" => |c| add_files(c, C_FILES),
                "D" => |c| delete_files(c, D_FILES),
                "E" => |c| delete_files(c, E_FILES),
                "F" => |c| c.add_file(b"cp1252/euro\x80".as_ref(), "euro"),
                "G" => |c| add_files(c, G_FILES),
                "H" => |c| add_files(c, H_FILES),
                "J" => |c| add_files(c, J_FILES),
                "K" => |c| add_files(c, K_FILES),
                "L" => |c| add_files(c, L_FILES),
            },
        )
        .await?;
        Ok((repo, changesets))
    }

    fn skeleton_dir<'a>(
        skeleton: &'a SkeletonManifest,
        elem: &[u8],
    ) -> Result<&'a SkeletonManifestDirectory> {
        match skeleton.lookup(&MPathElement::new(elem.to_vec())?) {
            Some(SkeletonManifestEntry::Directory(dir)) => Ok(dir),
            entry => Err(anyhow!("Unexpected skeleton manifest entry: {entry:?}")),
        }
    }

    #[mononoke::fbinit_test]
    async fn test_skeleton_manifests(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, changesets) = init_repo(&ctx).await?;
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);

        let a_bcs = changesets["A"].load(&ctx, repo.repo_blobstore()).await?;
        let a_skeleton_id =
            derive_skeleton_manifest(&ctx, &derivation_ctx, vec![], get_file_changes(&a_bcs))
                .await?;
        let a_skeleton = a_skeleton_id.load(&ctx, repo.repo_blobstore()).await?;
        assert_eq!(
            a_skeleton.lookup(&MPathElement::new(b"A".to_vec())?),
            Some(&SkeletonManifestEntry::File)
        );
        assert_eq!(
            a_skeleton.summary(),
            &SkeletonManifestSummary {
                child_files_count: 1,
                descendant_files_count: 1,
                max_path_len: 1,
                max_path_wchar_len: 1,
                ..Default::default()
            }
        );

        // Changeset B introduces some subdirectories
        let b_bcs = changesets["B"].load(&ctx, repo.repo_blobstore()).await?;
        let b_skeleton_id = derive_skeleton_manifest(
            &ctx,
            &derivation_ctx,
            vec![a_skeleton_id],
            get_file_changes(&b_bcs),
        )
        .await?;
        let b_skeleton = b_skeleton_id.load(&ctx, repo.repo_blobstore()).await?;
        assert_eq!(
            b_skeleton.summary(),
            &SkeletonManifestSummary {
                child_files_count: 2,
                descendant_files_count: 8,
                child_dirs_count: 2,
                descendant_dirs_count: 11,
                max_path_len: 29,
                max_path_wchar_len: 29,
                ..Default::default()
            }
        );
        assert_eq!(
            skeleton_dir(&b_skeleton, b"dir1")?.summary(),
            &SkeletonManifestSummary {
                child_files_count: 0,
                descendant_files_count: 5,
                child_dirs_count: 2,
                descendant_dirs_count: 7,
                max_path_len: 24,
                max_path_wchar_len: 24,
                ..Default::default()
            }
        );
        assert_eq!(
            b_skeleton
                .first_case_conflict(&ctx, repo.repo_blobstore())
                .await?,
            None,
        );

        // Changeset C introduces some case conflicts
        let c_bcs = changesets["C"].load(&ctx, repo.repo_blobstore()).await?;
        let c_skeleton_id = derive_skeleton_manifest(
            &ctx,
            &derivation_ctx,
            vec![b_skeleton_id],
            get_file_changes(&c_bcs),
        )
        .await?;
        let c_skeleton = c_skeleton_id.load(&ctx, repo.repo_blobstore()).await?;
        assert_eq!(
            c_skeleton.summary(),
            &SkeletonManifestSummary {
                child_files_count: 3,
                descendant_files_count: 13,
                child_dirs_count: 2,
                descendant_dirs_count: 14,
                max_path_len: 29,
                max_path_wchar_len: 29,
                descendant_case_conflicts: true,
                ..Default::default()
            }
        );
        assert_eq!(
            c_skeleton
                .first_case_conflict(&ctx, repo.repo_blobstore())
                .await?,
            Some((
                NonRootMPath::new(b"dir1/subdir1/SUBSUBDIR2")?,
                NonRootMPath::new(b"dir1/subdir1/subsubdir2")?
            ))
        );
        assert_eq!(
            c_skeleton
                .clone()
                .first_new_case_conflict(
                    &ctx,
                    repo.repo_blobstore(),
                    vec![b_skeleton.clone()],
                    &PrefixTrie::new()
                )
                .await?,
            Some((
                NonRootMPath::new(b"dir1/subdir1/SUBSUBDIR2")?,
                NonRootMPath::new(b"dir1/subdir1/subsubdir2")?
            ))
        );

        let mut excluded_paths = PrefixTrie::new();
        excluded_paths.add(&NonRootMPath::new(b"dir1/subdir1")?);
        assert_eq!(
            c_skeleton
                .clone()
                .first_new_case_conflict(
                    &ctx,
                    repo.repo_blobstore(),
                    vec![b_skeleton],
                    &excluded_paths
                )
                .await?,
            Some((
                NonRootMPath::new(b"dir1/subdir2/SUBSUBDIR1")?,
                NonRootMPath::new(b"dir1/subdir2/subsubdir1")?
            ))
        );

        let c_sk_dir1 = skeleton_dir(&c_skeleton, b"dir1")?
            .id()
            .load(&ctx, repo.repo_blobstore())
            .await?;
        assert!(!c_sk_dir1.summary().child_case_conflicts);
        assert!(c_sk_dir1.summary().descendant_case_conflicts);

        let c_sk_subdir1 = skeleton_dir(&c_sk_dir1, b"subdir1")?
            .id()
            .load(&ctx, repo.repo_blobstore())
            .await?;
        assert!(c_sk_subdir1.summary().child_case_conflicts);
        assert!(c_sk_subdir1.summary().descendant_case_conflicts);

        let c_sk_subsubdir1 = skeleton_dir(&c_sk_subdir1, b"subsubdir1")?;
        assert!(c_sk_subsubdir1.summary().child_case_conflicts);
        assert!(!c_sk_subsubdir1.summary().descendant_case_conflicts);

        let c_sk_subsubdir2 = skeleton_dir(&c_sk_subdir1, b"subsubdir2")?;
        assert!(!c_sk_subsubdir2.summary().child_case_conflicts);
        assert!(!c_sk_subsubdir2.summary().descendant_case_conflicts);

        let c_sk_subdir2 = skeleton_dir(&c_sk_dir1, b"subdir2")?;
        assert!(c_sk_subdir2.summary().child_case_conflicts);
        assert!(!c_sk_subdir2.summary().descendant_case_conflicts);

        let c_sk_dir2 = skeleton_dir(&c_skeleton, b"dir2")?;
        assert!(!c_sk_dir2.summary().child_case_conflicts);
        assert!(!c_sk_dir2.summary().descendant_case_conflicts);

        // Changeset D removes some of the conflicts
        let d_bcs = changesets["D"].load(&ctx, repo.repo_blobstore()).await?;
        let d_skeleton_id = derive_skeleton_manifest(
            &ctx,
            &derivation_ctx,
            vec![c_skeleton_id],
            get_file_changes(&d_bcs),
        )
        .await?;
        let d_skeleton = d_skeleton_id.load(&ctx, repo.repo_blobstore()).await?;
        assert!(!d_skeleton.summary().child_case_conflicts);
        assert!(d_skeleton.summary().descendant_case_conflicts);
        assert_eq!(
            d_skeleton
                .first_case_conflict(&ctx, repo.repo_blobstore())
                .await?,
            Some((
                NonRootMPath::new(b"dir1/subdir1/subsubdir1/FILE1")?,
                NonRootMPath::new(b"dir1/subdir1/subsubdir1/file1")?
            ))
        );
        assert_eq!(
            d_skeleton
                .clone()
                .first_new_case_conflict(
                    &ctx,
                    repo.repo_blobstore(),
                    vec![c_skeleton.clone()],
                    &PrefixTrie::new()
                )
                .await?,
            None,
        );

        let d_sk_dir1 = skeleton_dir(&d_skeleton, b"dir1")?
            .id()
            .load(&ctx, repo.repo_blobstore())
            .await?;
        assert!(!d_sk_dir1.summary().child_case_conflicts);
        assert!(d_sk_dir1.summary().descendant_case_conflicts);

        let d_sk_subdir1 = skeleton_dir(&d_sk_dir1, b"subdir1")?
            .id()
            .load(&ctx, repo.repo_blobstore())
            .await?;
        assert!(!d_sk_subdir1.summary().child_case_conflicts);
        assert!(d_sk_subdir1.summary().descendant_case_conflicts);

        // Changeset E removes them all
        let e_bcs = changesets["E"].load(&ctx, repo.repo_blobstore()).await?;
        let e_skeleton_id = derive_skeleton_manifest(
            &ctx,
            &derivation_ctx,
            vec![d_skeleton_id],
            get_file_changes(&e_bcs),
        )
        .await?;
        let e_skeleton = e_skeleton_id.load(&ctx, repo.repo_blobstore()).await?;
        assert_eq!(
            e_skeleton.summary(),
            &SkeletonManifestSummary {
                child_files_count: 5,
                descendant_files_count: 11,
                child_dirs_count: 2,
                descendant_dirs_count: 11,
                max_path_len: 29,
                max_path_wchar_len: 29,
                ..Default::default()
            }
        );

        // Changeset F adds a non-UTF-8 filename
        let f_bcs = changesets["F"].load(&ctx, repo.repo_blobstore()).await?;
        let f_skeleton_id = derive_skeleton_manifest(
            &ctx,
            &derivation_ctx,
            vec![e_skeleton_id],
            get_file_changes(&f_bcs),
        )
        .await?;
        let f_skeleton = f_skeleton_id.load(&ctx, repo.repo_blobstore()).await?;
        assert_eq!(
            f_skeleton.summary(),
            &SkeletonManifestSummary {
                child_files_count: 6,
                descendant_files_count: 13,
                child_dirs_count: 3,
                descendant_dirs_count: 12,
                max_path_len: 29,
                max_path_wchar_len: 29,
                descendant_non_utf8_filenames: true,
                ..Default::default()
            }
        );
        assert_eq!(
            skeleton_dir(&f_skeleton, b"cp1252")?.summary(),
            &SkeletonManifestSummary {
                child_files_count: 1,
                descendant_files_count: 1,
                child_dirs_count: 0,
                descendant_dirs_count: 0,
                max_path_len: 5,
                max_path_wchar_len: 5,
                child_non_utf8_filenames: true,
                ..Default::default()
            }
        );

        // Changeset G adds some files that are not valid on Windows
        let g_bcs = changesets["G"].load(&ctx, repo.repo_blobstore()).await?;
        let g_skeleton_id = derive_skeleton_manifest(
            &ctx,
            &derivation_ctx,
            vec![f_skeleton_id],
            get_file_changes(&g_bcs),
        )
        .await?;
        let g_skeleton = g_skeleton_id.load(&ctx, repo.repo_blobstore()).await?;
        assert_eq!(
            g_skeleton.summary(),
            &SkeletonManifestSummary {
                child_files_count: 7,
                descendant_files_count: 21,
                child_dirs_count: 5,
                descendant_dirs_count: 21,
                max_path_len: 37,
                max_path_wchar_len: 34,
                descendant_non_utf8_filenames: true,
                descendant_invalid_windows_filenames: true,
                ..Default::default()
            }
        );
        assert_eq!(
            skeleton_dir(&g_skeleton, b"windows")?.summary(),
            &SkeletonManifestSummary {
                child_files_count: 0,
                descendant_files_count: 1,
                child_dirs_count: 1,
                descendant_dirs_count: 1,
                max_path_len: 29,
                max_path_wchar_len: 26,
                ..Default::default()
            }
        );
        let g_sk_nonwin = skeleton_dir(&g_skeleton, b"nonwin")?
            .id()
            .load(&ctx, repo.repo_blobstore())
            .await?;
        for dir in ["dir1", "dir2", "dir3", "dir4", "dir5", "dir6"] {
            assert_eq!(
                skeleton_dir(&g_sk_nonwin, dir.as_bytes())?
                    .summary()
                    .child_invalid_windows_filenames,
                true,
                "Missing child_invalid_windows_filenames for {}",
                dir
            );
        }

        // Changeset H introduces a new case conflict on top of the ones
        // already in changeset C.
        let h_bcs = changesets["H"].load(&ctx, repo.repo_blobstore()).await?;
        let h_skeleton_id = derive_skeleton_manifest(
            &ctx,
            &derivation_ctx,
            vec![c_skeleton_id],
            get_file_changes(&h_bcs),
        )
        .await?;
        let h_skeleton = h_skeleton_id.load(&ctx, repo.repo_blobstore()).await?;
        assert_eq!(
            h_skeleton.summary(),
            &SkeletonManifestSummary {
                child_files_count: 4,
                descendant_files_count: 15,
                child_dirs_count: 2,
                descendant_dirs_count: 14,
                max_path_len: 29,
                max_path_wchar_len: 29,
                descendant_case_conflicts: true,
                ..Default::default()
            }
        );
        assert_eq!(
            h_skeleton
                .first_case_conflict(&ctx, repo.repo_blobstore())
                .await?,
            Some((
                NonRootMPath::new(b"dir1/subdir1/SUBSUBDIR2")?,
                NonRootMPath::new(b"dir1/subdir1/subsubdir2")?
            ))
        );
        assert_eq!(
            h_skeleton
                .clone()
                .first_new_case_conflict(
                    &ctx,
                    repo.repo_blobstore(),
                    vec![c_skeleton],
                    &PrefixTrie::new()
                )
                .await?,
            Some((
                NonRootMPath::new(b"dir1/subdir1/SUBSUBDIR3/FILE3")?,
                NonRootMPath::new(b"dir1/subdir1/SUBSUBDIR3/File3")?,
            ))
        );

        // Changeset J has an internal case conflict.
        let j_bcs = changesets["J"].load(&ctx, repo.repo_blobstore()).await?;
        let j_skeleton_id =
            derive_skeleton_manifest(&ctx, &derivation_ctx, vec![], get_file_changes(&j_bcs))
                .await?;
        let j_skeleton = j_skeleton_id.load(&ctx, repo.repo_blobstore()).await?;
        assert_eq!(
            j_skeleton
                .first_case_conflict(&ctx, repo.repo_blobstore())
                .await?,
            Some((
                NonRootMPath::new(b"dir3/FILE1")?,
                NonRootMPath::new(b"dir3/file1")?
            ))
        );

        // Changeset K is a merge of H and J, both of which have case
        // conflicts.  It does not introduce any new case conflicts.
        let k_bcs = changesets["K"].load(&ctx, repo.repo_blobstore()).await?;
        let k_skeleton_id = derive_skeleton_manifest(
            &ctx,
            &derivation_ctx,
            vec![h_skeleton_id, j_skeleton_id],
            get_file_changes(&k_bcs),
        )
        .await?;
        let k_skeleton = k_skeleton_id.load(&ctx, repo.repo_blobstore()).await?;
        assert_eq!(
            k_skeleton
                .first_new_case_conflict(
                    &ctx,
                    repo.repo_blobstore(),
                    vec![h_skeleton.clone(), j_skeleton],
                    &PrefixTrie::new()
                )
                .await?,
            None,
        );

        // Changeset L has no case conflicts.
        let l_bcs = changesets["L"].load(&ctx, repo.repo_blobstore()).await?;
        let l_skeleton_id =
            derive_skeleton_manifest(&ctx, &derivation_ctx, vec![], get_file_changes(&l_bcs))
                .await?;
        let l_skeleton = l_skeleton_id.load(&ctx, repo.repo_blobstore()).await?;
        assert_eq!(
            l_skeleton
                .first_case_conflict(&ctx, repo.repo_blobstore())
                .await?,
            None,
        );

        // Changeset M introduces a case conflict by virtue of merging the
        // manifests of H (which has its own case conflicts) and L.
        let m_bcs = changesets["M"].load(&ctx, repo.repo_blobstore()).await?;
        let m_skeleton_id = derive_skeleton_manifest(
            &ctx,
            &derivation_ctx,
            vec![h_skeleton_id, l_skeleton_id],
            get_file_changes(&m_bcs),
        )
        .await?;
        let m_skeleton = m_skeleton_id.load(&ctx, repo.repo_blobstore()).await?;
        assert_eq!(
            m_skeleton
                .first_case_conflict(&ctx, repo.repo_blobstore())
                .await?,
            Some((
                NonRootMPath::new(b"dir1/subdir1/SUBSUBDIR2")?,
                NonRootMPath::new(b"dir1/subdir1/subsubdir2")?
            ))
        );
        assert_eq!(
            m_skeleton
                .first_new_case_conflict(
                    &ctx,
                    repo.repo_blobstore(),
                    vec![h_skeleton, l_skeleton],
                    &PrefixTrie::new()
                )
                .await?,
            Some((
                NonRootMPath::new(b"dir2/subdir1/subsubdir1/FILE1")?,
                NonRootMPath::new(b"dir2/subdir1/subsubdir1/file1")?
            )),
        );

        Ok(())
    }

    /// Pipeline: derive top-level dirs independently, assemble the root; must equal full derivation.
    #[mononoke::fbinit_test]
    async fn test_derivation_pipeline_independent_dirs(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, changesets) = init_repo(&ctx).await?;
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
        let blobstore = repo.repo_blobstore().clone();

        let a_bcs = changesets["A"].load(&ctx, &blobstore).await?;
        let parent_id =
            derive_skeleton_manifest(&ctx, &derivation_ctx, vec![], get_file_changes(&a_bcs))
                .await?;

        let b_bcs = changesets["B"].load(&ctx, &blobstore).await?;
        let changes = get_file_changes(&b_bcs);
        let expected_id =
            derive_skeleton_manifest(&ctx, &derivation_ctx, vec![parent_id], changes.clone())
                .await?;

        let mut known_entries = HashMap::new();
        for dir in ["dir1", "dir2"] {
            let prefix = MPath::new(dir)?;
            let parents = match parent_id
                .find_entry(ctx.clone(), blobstore.clone(), prefix.clone())
                .await?
            {
                Some(entry) => vec![entry],
                None => vec![],
            };
            let entry = derive_skeleton_manifest_entry(
                &ctx,
                &derivation_ctx,
                ManifestDerivationInput::Entry {
                    parents: parents.into_iter().map(Some).collect(),
                    known_entries: HashMap::new(),
                    path: prefix.clone(),
                },
                changes.clone(),
            )
            .await?
            .into_entry()?;
            known_entries.insert(prefix, KnownEntry::Entry(entry));
        }

        let root_entry = derive_skeleton_manifest_entry(
            &ctx,
            &derivation_ctx,
            ManifestDerivationInput::Entry {
                parents: vec![Some(Entry::Tree(parent_id))],
                known_entries,
                path: MPath::ROOT,
            },
            changes,
        )
        .await?
        .into_entry()?
        .expect("root stage should produce an entry");

        let pipeline_id = root_entry.into_tree().expect("root entry should be a tree");
        assert_eq!(pipeline_id, expected_id);

        Ok(())
    }

    /// Pipeline: derive a deep subtree, then its ancestor via known_entries, then the root.
    #[mononoke::fbinit_test]
    async fn test_derivation_pipeline_nested_dirs(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, changesets) = init_repo(&ctx).await?;
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
        let blobstore = repo.repo_blobstore().clone();

        // Build parents A, B, C; commit D's changes are all under dir1/subdir1.
        let a_bcs = changesets["A"].load(&ctx, &blobstore).await?;
        let a_id =
            derive_skeleton_manifest(&ctx, &derivation_ctx, vec![], get_file_changes(&a_bcs))
                .await?;
        let b_bcs = changesets["B"].load(&ctx, &blobstore).await?;
        let b_id =
            derive_skeleton_manifest(&ctx, &derivation_ctx, vec![a_id], get_file_changes(&b_bcs))
                .await?;
        let c_bcs = changesets["C"].load(&ctx, &blobstore).await?;
        let parent_id =
            derive_skeleton_manifest(&ctx, &derivation_ctx, vec![b_id], get_file_changes(&c_bcs))
                .await?;

        let d_bcs = changesets["D"].load(&ctx, &blobstore).await?;
        let changes = get_file_changes(&d_bcs);
        let expected_id =
            derive_skeleton_manifest(&ctx, &derivation_ctx, vec![parent_id], changes.clone())
                .await?;

        let subdir1 = MPath::new("dir1/subdir1")?;
        let subdir1_parents = vec![
            parent_id
                .find_entry(ctx.clone(), blobstore.clone(), subdir1.clone())
                .await?,
        ];
        let subdir1_entry = derive_skeleton_manifest_entry(
            &ctx,
            &derivation_ctx,
            ManifestDerivationInput::Entry {
                parents: subdir1_parents,
                known_entries: HashMap::new(),
                path: subdir1.clone(),
            },
            changes.clone(),
        )
        .await?
        .into_entry()?
        .expect("dir1/subdir1 stage should produce an entry");

        let dir1 = MPath::new("dir1")?;
        let dir1_parents = vec![
            parent_id
                .find_entry(ctx.clone(), blobstore.clone(), dir1.clone())
                .await?,
        ];
        let dir1_known = HashMap::from([(subdir1, KnownEntry::Entry(Some(subdir1_entry)))]);
        let dir1_entry = derive_skeleton_manifest_entry(
            &ctx,
            &derivation_ctx,
            ManifestDerivationInput::Entry {
                parents: dir1_parents,
                known_entries: dir1_known,
                path: dir1.clone(),
            },
            changes.clone(),
        )
        .await?
        .into_entry()?
        .expect("dir1 stage should produce an entry");

        let root_known = HashMap::from([(dir1, KnownEntry::Entry(Some(dir1_entry)))]);
        let root_entry = derive_skeleton_manifest_entry(
            &ctx,
            &derivation_ctx,
            ManifestDerivationInput::Entry {
                parents: vec![Some(Entry::Tree(parent_id))],
                known_entries: root_known,
                path: MPath::ROOT,
            },
            changes,
        )
        .await?
        .into_entry()?
        .expect("root stage should produce an entry");

        let pipeline_id = root_entry.into_tree().expect("root entry should be a tree");
        assert_eq!(pipeline_id, expected_id);

        Ok(())
    }
}
