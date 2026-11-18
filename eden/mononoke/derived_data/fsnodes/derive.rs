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
use ascii::AsciiString;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use borrowed::borrowed;
use cloned::cloned;
use context::CoreContext;
use derived_data::prefetch_content_metadata;
use derived_data_manager::DerivationContext;
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStageOutput;
use derived_data_manager::manifest_partitions::create_sorted_subentries;
use digest::Digest;
use futures::stream::FuturesOrdered;
use futures::stream::FuturesUnordered;
use futures::stream::TryStreamExt;
use manifest::Entry;
use manifest::LeafInfo;
use manifest::ManifestChanges;
use manifest::TreeInfo;
use manifest::derive_manifests_for_simple_stack_of_commits;
use manifest::flatten_subentries;
use mononoke_types::BlobstoreKey;
use mononoke_types::BlobstoreValue;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::ContentMetadataV2;
use mononoke_types::FileType;
use mononoke_types::FsnodeId;
use mononoke_types::MPathElement;
use mononoke_types::NonRootMPath;
use mononoke_types::RepoPath;
use mononoke_types::SortedVectorTrieMap;
use mononoke_types::fsnode::Fsnode;
use mononoke_types::fsnode::FsnodeDirectory;
use mononoke_types::fsnode::FsnodeEntry;
use mononoke_types::fsnode::FsnodeFile;
use mononoke_types::fsnode::FsnodeSummary;
use mononoke_types::hash::Sha1;
use mononoke_types::hash::Sha256;
use mononoke_types::path::MPath;
use restricted_paths_common::ManifestIdStoreWriteCallsite;
use restricted_paths_common::ManifestType;
use restricted_paths_common::RestrictedManifestId;
use restricted_paths_common::RestrictedPathManifestIdEntry;
use restricted_paths_common::RestrictedPathsConfigBased;
use restricted_paths_common::maybe_propagate_manifest_id_store_write_error;
use sorted_vector_map::SortedVectorMap;

use crate::FsnodeDerivationError;

pub(crate) async fn derive_fsnodes_stack(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    file_changes: Vec<(
        ChangesetId,
        BTreeMap<NonRootMPath, Option<(ContentId, FileType)>>,
    )>,
    parent: Option<FsnodeId>,
) -> Result<HashMap<ChangesetId, FsnodeId>, Error> {
    let blobstore = derivation_ctx.blobstore();
    let restricted_paths = derivation_ctx.restricted_paths();

    let content_ids = file_changes
        .iter()
        .flat_map(|(_cs_id, per_commit_file_changes)| {
            per_commit_file_changes
                .values()
                .filter_map(|content_id_and_file_type| {
                    content_id_and_file_type.map(|(content_id, _file_type)| content_id)
                })
        })
        .collect::<HashSet<_>>();

    let prefetched_content_metadata =
        Arc::new(prefetch_content_metadata(ctx, &blobstore, content_ids).await?);

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
            cloned!(blobstore, ctx, restricted_paths);
            move |tree_info, _cs_id| {
                cloned!(blobstore, ctx, restricted_paths);
                async move { create_fsnode(&ctx, &blobstore, &restricted_paths, tree_info).await }
            }
        },
        move |leaf_info, _cs_id| {
            cloned!(prefetched_content_metadata);
            async move { check_fsnode_leaf(prefetched_content_metadata, leaf_info).await }
        },
    )
    .await?;

    Ok(res.into_iter().collect())
}

/// Derive an entry from parent entries and completed child stages.
pub(crate) async fn derive_fsnode_entry(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    input: ManifestDerivationInput<
        FsnodeId,
        FsnodeFile,
        SortedVectorTrieMap<Entry<FsnodeId, FsnodeFile>>,
    >,
    changes: Vec<(NonRootMPath, Option<(ContentId, FileType)>)>,
) -> Result<
    ManifestStageOutput<FsnodeId, FsnodeFile, SortedVectorTrieMap<Entry<FsnodeId, FsnodeFile>>>,
> {
    let blobstore = derivation_ctx.blobstore();
    let restricted_paths = derivation_ctx.restricted_paths();
    let content_ids = changes
        .iter()
        .filter(|(mpath, _)| input.contains_change(mpath))
        .filter_map(|(_, content_id_and_file_type)| {
            content_id_and_file_type.map(|(content_id, _file_type)| content_id)
        })
        .collect::<HashSet<_>>();

    let prefetched_content_metadata =
        Arc::new(prefetch_content_metadata(ctx, &blobstore, content_ids).await?);

    input
        .derive(
            ctx.clone(),
            blobstore.clone(),
            changes,
            {
                cloned!(blobstore, ctx, restricted_paths);
                move |tree_info| {
                    cloned!(blobstore, ctx, restricted_paths);
                    async move {
                        create_fsnode(&ctx, &blobstore, &restricted_paths, tree_info).await
                    }
                }
            },
            move |leaf_info| {
                cloned!(prefetched_content_metadata);
                async move { check_fsnode_leaf(prefetched_content_metadata, leaf_info).await }
            },
            |subentries| create_sorted_subentries(ctx, subentries),
        )
        .await
}

/// Derives fsnodes for bonsai_changeset `cs_id` given parent fsnodes. Note
/// that `derive_manifest()` does a lot of the heavy lifting for us, and this
/// crate has to provide only functions to create a single fsnode, and check
/// that the leaf entries (which are `(ContentId, FileType)` pairs) are valid
/// during merges.
pub(crate) async fn derive_fsnode(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    parents: Vec<FsnodeId>,
    changes: Vec<(NonRootMPath, Option<(ContentId, FileType)>)>,
) -> Result<FsnodeId> {
    Ok(derive_fsnode_entry(
        ctx,
        derivation_ctx,
        ManifestDerivationInput::Entry {
            parents: parents
                .into_iter()
                .map(|id| Some(Entry::Tree(id)))
                .collect(),
            known_entries: HashMap::new(),
            path: MPath::ROOT,
        },
        changes,
    )
    .await?
    .into_entry()?
    .and_then(Entry::into_tree)
    .expect("root derivation should always produce a tree"))
}

/// Collect all the subentries for a new fsnode, reusing entries the parent
/// fsnodes to avoid fetching too much.
async fn collect_fsnode_subentries(
    ctx: &CoreContext,
    blobstore: &impl KeyedBlobstore,
    parents: Vec<FsnodeId>,
    subentries: BTreeMap<
        MPathElement,
        (Option<Option<FsnodeSummary>>, Entry<FsnodeId, FsnodeFile>),
    >,
) -> Result<Vec<(MPathElement, FsnodeEntry)>> {
    // Load the parent fsnodes and collect their entries into a cache
    let mut dir_cache = HashMap::new();
    let mut parent_fsnodes = parents
        .into_iter()
        .map({
            move |fsnode_id| async move {
                fsnode_id
                    .load(ctx, blobstore)
                    .await
                    .context(FsnodeDerivationError::MissingParent(fsnode_id))
            }
        })
        .collect::<FuturesUnordered<_>>();
    while let Some(parent_fsnode) = parent_fsnodes.try_next().await? {
        for (_elem, entry) in parent_fsnode.list() {
            if let FsnodeEntry::Directory(dir) = entry {
                dir_cache.entry(*dir.id()).or_insert_with(|| dir.clone());
            }
        }
    }

    // Find (from the traversal or the cache) or fetch (from the blobstore)
    // the `FsnodeEntry` for each of the subentries.
    borrowed!(dir_cache);
    subentries
        .into_iter()
        .map(move |(elem, (summary, entry))| {
            async move {
                match entry {
                    Entry::Tree(fsnode_id) => {
                        if let Some(Some(summary)) = summary {
                            // The subdirectory was just created. Use the
                            // summary we just calculated.
                            Ok((
                                elem.clone(),
                                FsnodeEntry::Directory(FsnodeDirectory::new(fsnode_id, summary)),
                            ))
                        } else if let Some(entry) = dir_cache.get(&fsnode_id) {
                            // The subdirectory was already in this
                            // directory. Use the cached entry.
                            Ok((elem.clone(), FsnodeEntry::Directory(entry.clone())))
                        } else {
                            // Some other directory is being used. Fetch its
                            // summary from the blobstore.
                            let fsnode = fsnode_id.load(ctx, blobstore).await.with_context({
                                || {
                                    FsnodeDerivationError::MissingSubentry(
                                        String::from_utf8_lossy(elem.as_ref()).to_string(),
                                        fsnode_id,
                                    )
                                }
                            })?;

                            let entry = FsnodeEntry::Directory(FsnodeDirectory::new(
                                fsnode_id,
                                fsnode.summary().clone(),
                            ));
                            Ok((elem.clone(), entry))
                        }
                    }
                    Entry::Leaf(fsnode_file) => Ok((elem.clone(), FsnodeEntry::File(fsnode_file))),
                }
            }
        })
        .collect::<FuturesOrdered<_>>()
        .try_collect()
        .await
}

/// Create a new fsnode for the tree described by `tree_info`.
async fn create_fsnode(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    restricted_paths: &Arc<RestrictedPathsConfigBased>,
    tree_info: TreeInfo<
        FsnodeId,
        FsnodeFile,
        Option<FsnodeSummary>,
        SortedVectorTrieMap<Entry<FsnodeId, FsnodeFile>>,
    >,
) -> Result<(Option<FsnodeSummary>, FsnodeId)> {
    let entries = collect_fsnode_subentries(
        ctx,
        &blobstore,
        tree_info.parents,
        flatten_subentries(ctx, &(), tree_info.subentries)
            .await?
            .collect(),
    )
    .await?;

    // Build a summary of the entries and store it as the new fsnode.
    let entries: SortedVectorMap<_, _> = entries.into_iter().collect();
    let simple_format_sha1 = {
        let digest = generate_simple_format_digest(
            sha1::Sha1::new(),
            &entries,
            |fsnode_file| fsnode_file.content_sha1().to_hex(),
            |fsnode_dir| fsnode_dir.summary().simple_format_sha1.to_hex(),
        );
        let bytes = digest.finalize().into();
        Sha1::from_byte_array(bytes)
    };
    let simple_format_sha256 = {
        let digest = generate_simple_format_digest(
            sha2::Sha256::new(),
            &entries,
            |fsnode_file| fsnode_file.content_sha256().to_hex(),
            |fsnode_dir| fsnode_dir.summary().simple_format_sha256.to_hex(),
        );
        let bytes = digest.finalize().into();
        Sha256::from_byte_array(bytes)
    };
    let mut summary = FsnodeSummary {
        simple_format_sha1,
        simple_format_sha256,
        child_files_count: 0,
        child_files_total_size: 0,
        child_dirs_count: 0,
        descendant_files_count: 0,
        descendant_files_total_size: 0,
    };
    for (_elem, entry) in entries.iter() {
        match entry {
            FsnodeEntry::File(fsnode_file) => {
                let size = fsnode_file.size();
                summary.child_files_count += 1;
                summary.child_files_total_size += size;
                summary.descendant_files_count += 1;
                summary.descendant_files_total_size += size;
            }
            FsnodeEntry::Directory(fsnode_dir) => {
                let subdir_summary = fsnode_dir.summary();
                summary.child_dirs_count += 1;
                summary.descendant_files_count += subdir_summary.descendant_files_count;
                summary.descendant_files_total_size += subdir_summary.descendant_files_total_size;
            }
        }
    }
    let fsnode = Fsnode::new(entries, summary.clone());
    let blob = fsnode.into_blob();
    let fsnode_id = *blob.id();
    let key = fsnode_id.blobstore_key();

    let f = {
        cloned!(blobstore, ctx);
        async move { blobstore.put(&ctx, key, blob.into()).await }
    };

    f.await?;

    let restricted_paths_enabled = justknobs::eval(
        "scm/mononoke:enabled_restricted_paths_access_logging",
        None,
        Some("fsnodes_write"),
    );

    let path = &tree_info.path;
    if restricted_paths_enabled {
        if let Some(non_root_path) = path.clone().into_optional_non_root_path() {
            let should_record = restricted_paths.should_record_manifest_id_entry(&non_root_path);
            if should_record {
                let entry = RestrictedPathManifestIdEntry::new(
                    ManifestType::Fsnode,
                    RestrictedManifestId::from(&fsnode_id.blake2().into_inner()),
                    RepoPath::DirectoryPath(non_root_path),
                )?;

                if let Err(e) = restricted_paths
                    .manifest_id_store()
                    .add_entry(ctx, entry)
                    .await
                    .with_context(|| format!("Failed to track restricted path at {path}"))
                {
                    tracing::error!(
                        path = %path,
                        error = %e,
                        "Failed to track restricted path"
                    );
                    maybe_propagate_manifest_id_store_write_error(
                        e,
                        ManifestIdStoreWriteCallsite::CreateFsnode,
                    )?;
                }
            }
        }
    }

    Ok((Some(summary), fsnode_id))
}

/// Generate the simple format hash for a directory. See
/// `mononoke_types/src/fsnodes.rs` for a description of what the simple
/// format is.
fn generate_simple_format_digest<H, F, D>(
    mut digest: H,
    dir: &SortedVectorMap<MPathElement, FsnodeEntry>,
    get_file_hash: F,
    get_dir_hash: D,
) -> H
where
    H: Digest,
    F: Fn(&FsnodeFile) -> AsciiString,
    D: Fn(&FsnodeDirectory) -> AsciiString,
{
    for (elem, entry) in dir.iter() {
        match entry {
            FsnodeEntry::File(file) => {
                digest.update(get_file_hash(file).as_bytes());
                digest.update(match file.file_type() {
                    FileType::Regular => b" file ",
                    FileType::Executable => b" exec ",
                    FileType::Symlink => b" link ",
                    FileType::GitSubmodule => b" gitm ",
                });
            }
            FsnodeEntry::Directory(dir) => {
                digest.update(get_dir_hash(dir).as_bytes());
                digest.update(b" tree ");
            }
        }
        digest.update(elem.as_ref());
        digest.update(b"\0");
    }
    digest
}

/// There are no leaves stored for fsnodes, however we still need to check
/// that any merge operations have resulted in valid fsnodes, where, for each
/// file, either all the parents have the same file contents, or the
/// changeset includes a change for that file.
async fn check_fsnode_leaf(
    prefetched_content_metadata: Arc<HashMap<ContentId, ContentMetadataV2>>,
    leaf_info: LeafInfo<FsnodeFile, (ContentId, FileType)>,
) -> Result<(Option<FsnodeSummary>, FsnodeFile)> {
    if let Some(content_id_and_file_type) = leaf_info.change {
        let (content_id, file_type) = content_id_and_file_type;
        if let Some(metadata) = prefetched_content_metadata.get(&content_id) {
            let fsnode_file = FsnodeFile::new(
                content_id,
                file_type,
                metadata.total_size,
                metadata.sha1,
                metadata.sha256,
            );
            Ok((None, fsnode_file))
        } else {
            Err(FsnodeDerivationError::MissingContent(content_id).into())
        }
    } else {
        // This bonsai changeset is a merge. If all content IDs and file
        // types match for this file, then the content ID is valid. Check
        // this is so.
        if leaf_info.parents.len() < 2 {
            return Err(FsnodeDerivationError::InvalidBonsai(
                "no change is provided, but file has only one parent".to_string(),
            )
            .into());
        }
        let mut iter = leaf_info.parents.into_iter();
        let fsnode_file = iter
            .next()
            .filter(|&first_elem| iter.all(|next_elem| next_elem == first_elem));
        if let Some(fsnode_file) = fsnode_file {
            Ok((None, fsnode_file))
        } else {
            Err(FsnodeDerivationError::InvalidBonsai(
                "no change is provided, but file content or type is different".to_string(),
            )
            .into())
        }
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use derived_data_test_utils::bonsai_changeset_from_hg;
    use derived_data_test_utils::iterate_all_manifest_entries;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use fixtures::Linear;
    use fixtures::ManyFilesDirs;
    use fixtures::TestRepoFixture;
    use manifest::KnownEntry;
    use manifest::ManifestOps;
    use mononoke_macros::mononoke;
    use repo_blobstore::RepoBlobstore;
    use repo_blobstore::RepoBlobstoreRef;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;

    use super::*;
    use crate::mapping::get_file_changes;

    #[facet::container]
    #[derive(Clone)]
    struct TestRepo(
        dyn BonsaiHgMapping,
        dyn Bookmarks,
        RepoBlobstore,
        RepoDerivedData,
        RepoIdentity,
        CommitGraph,
        dyn CommitGraphWriter,
        FilestoreConfig,
    );

    #[mononoke::fbinit_test]
    async fn flat_linear_test(fb: FacebookInit) {
        let repo: TestRepo = Linear::get_repo(fb).await;
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);

        let ctx = CoreContext::test_mock(fb);
        let parent_fsnode_id = {
            let parent_hg_cs = "2d7d4ba9ce0a6ffd222de7785b249ead9c51c536";
            let (_bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, parent_hg_cs)
                .await
                .unwrap();

            let f = derive_fsnode(&ctx, &derivation_ctx, vec![], get_file_changes(&bcs));

            let root_fsnode_id = f.await.unwrap();

            // Make sure it's saved in the blobstore.
            let root_fsnode = root_fsnode_id
                .load(&ctx, repo.repo_blobstore())
                .await
                .unwrap();

            // Make sure the fsnodes describe the full manifest.
            let all_fsnodes: BTreeMap<_, _> =
                iterate_all_manifest_entries(&ctx, &repo, Entry::Tree(root_fsnode_id))
                    .try_collect()
                    .await
                    .unwrap();
            assert_eq!(
                all_fsnodes.keys().collect::<Vec<_>>(),
                vec![
                    &MPath::ROOT,
                    &MPath::new("1").unwrap(),
                    &MPath::new("files").unwrap()
                ]
            );

            // Make sure the root fsnode is correct.
            assert_eq!(
                root_fsnode.summary(),
                &FsnodeSummary {
                    simple_format_sha1: Sha1::from_str("b23c1ffbd30ffa92942ec67748ea28084bbbb100")
                        .unwrap(),
                    simple_format_sha256: Sha256::from_str(
                        "6639d6877355e0b26b135cc0bf90b8cb901c85f43e71fbf4e8a5202ff1a7f868"
                    )
                    .unwrap(),
                    child_files_count: 2,
                    child_files_total_size: 4,
                    child_dirs_count: 0,
                    descendant_files_count: 2,
                    descendant_files_total_size: 4,
                }
            );
            root_fsnode_id
        };

        {
            let child_hg_cs = "3e0e761030db6e479a7fb58b12881883f9f8c63f";
            let (_bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, child_hg_cs)
                .await
                .unwrap();

            let f = derive_fsnode(
                &ctx,
                &derivation_ctx,
                vec![parent_fsnode_id.clone()],
                get_file_changes(&bcs),
            );

            let root_fsnode_id = f.await.unwrap();

            // Make sure it's saved in the blobstore
            let root_fsnode = root_fsnode_id
                .load(&ctx, repo.repo_blobstore())
                .await
                .unwrap();

            // Make sure the fsnodes describe the full manifest.
            let all_fsnodes: BTreeMap<_, _> =
                iterate_all_manifest_entries(&ctx, &repo, Entry::Tree(root_fsnode_id))
                    .try_collect()
                    .await
                    .unwrap();
            assert_eq!(
                all_fsnodes.keys().collect::<Vec<_>>(),
                vec![
                    &MPath::ROOT,
                    &MPath::new("1").unwrap(),
                    &MPath::new("2").unwrap(),
                    &MPath::new("files").unwrap()
                ]
            );

            // Make sure the root fsnode is correct.
            assert_eq!(
                root_fsnode.summary(),
                &FsnodeSummary {
                    simple_format_sha1: Sha1::from_str("e1c394ce3584f6c69680807b93997e46099e3f84")
                        .unwrap(),
                    simple_format_sha256: Sha256::from_str(
                        "bae96aa31c752e678f22bd1d051ba0a2def7992cf6857c1a1736fd6517106c08"
                    )
                    .unwrap(),
                    child_files_count: 3,
                    child_files_total_size: 8,
                    child_dirs_count: 0,
                    descendant_files_count: 3,
                    descendant_files_total_size: 8,
                }
            );
        }
    }

    #[mononoke::fbinit_test]
    async fn nested_directories_test(fb: FacebookInit) {
        let repo: TestRepo = ManyFilesDirs::get_repo(fb).await;
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);

        let ctx = CoreContext::test_mock(fb);

        // Derive fsnodes for the first two commits.  We will look at commit 3 and 4.
        let parent_fsnode_id = {
            let parent_hg_cs = "5a28e25f924a5d209b82ce0713d8d83e68982bc8";
            let (_bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, parent_hg_cs)
                .await
                .unwrap();
            let f = derive_fsnode(&ctx, &derivation_ctx, vec![], get_file_changes(&bcs));
            f.await.unwrap()
        };

        let parent_fsnode_id = {
            let parent_hg_cs = "2f866e7e549760934e31bf0420a873f65100ad63";
            let (_bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, parent_hg_cs)
                .await
                .unwrap();
            let f = derive_fsnode(
                &ctx,
                &derivation_ctx,
                vec![parent_fsnode_id.clone()],
                get_file_changes(&bcs),
            );
            f.await.unwrap()
        };

        let parent_fsnode_id = {
            let parent_hg_cs = "d261bc7900818dea7c86935b3fb17a33b2e3a6b4";
            let (_bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, parent_hg_cs)
                .await
                .unwrap();

            let f = derive_fsnode(
                &ctx,
                &derivation_ctx,
                vec![parent_fsnode_id.clone()],
                get_file_changes(&bcs),
            );

            let root_fsnode_id = f.await.unwrap();

            // Make sure it's saved in the blobstore.
            let root_fsnode = root_fsnode_id
                .load(&ctx, repo.repo_blobstore())
                .await
                .unwrap();

            // Make sure the fsnodes describe the full manifest.
            let all_fsnodes: BTreeMap<_, _> =
                iterate_all_manifest_entries(&ctx, &repo, Entry::Tree(root_fsnode_id))
                    .try_collect()
                    .await
                    .unwrap();
            assert_eq!(
                all_fsnodes.keys().collect::<Vec<_>>(),
                vec![
                    &MPath::ROOT,
                    &MPath::new("1").unwrap(),
                    &MPath::new("2").unwrap(),
                    &MPath::new("dir1").unwrap(),
                    &MPath::new("dir1/file_1_in_dir1").unwrap(),
                    &MPath::new("dir1/file_2_in_dir1").unwrap(),
                    &MPath::new("dir1/subdir1").unwrap(),
                    &MPath::new("dir1/subdir1/file_1").unwrap(),
                    &MPath::new("dir1/subdir1/subsubdir1").unwrap(),
                    &MPath::new("dir1/subdir1/subsubdir1/file_1").unwrap(),
                    &MPath::new("dir1/subdir1/subsubdir2").unwrap(),
                    &MPath::new("dir1/subdir1/subsubdir2/file_1").unwrap(),
                    &MPath::new("dir1/subdir1/subsubdir2/file_2").unwrap(),
                    &MPath::new("dir2").unwrap(),
                    &MPath::new("dir2/file_1_in_dir2").unwrap(),
                ]
            );

            // Make sure the root fsnode is correct.  Test the simple format hashes are as expected.
            let simple_format_sha1 = {
                let text = concat!(
                    "e5fa44f2b31c1fb553b6021e7360d07d5d91ff5e file 1\0",
                    "7448d8798a4380162d4b56f9b452e2f6f9e24e7a file 2\0",
                    "be0dface7b74d8a69b39fb4691ef9eee36077ede tree dir1\0",
                    "ad02b5a5f778d9ad6afd42fcc8e0b889254b5215 tree dir2\0",
                );
                let mut digest = sha1::Sha1::new();
                digest.update(text);
                let bytes = digest.finalize().into();
                Sha1::from_byte_array(bytes)
            };
            let simple_format_sha256 = {
                let text = concat!(
                    "4355a46b19d348dc2f57c046f8ef63d4538ebb936000f3c9ee954a27460dd865 file 1\0",
                    "53c234e5e8472b6ac51c1ae1cab3fe06fad053beb8ebfd8977b010655bfdd3c3 file 2\0",
                    "eebf7e41f348db6b31c11fe7adf577bd0951300436a1fd37e53d628127b3517e tree dir1\0",
                    "583c3d388efb78eb9dec46626662d6657bb53706c1ee10770c0fb3e859bd36e1 tree dir2\0",
                );
                let mut digest = sha2::Sha256::new();
                digest.update(text);
                let bytes = digest.finalize().into();
                Sha256::from_byte_array(bytes)
            };

            assert_eq!(
                root_fsnode.summary(),
                &FsnodeSummary {
                    simple_format_sha1,
                    simple_format_sha256,
                    child_files_count: 2,
                    child_files_total_size: 4,
                    child_dirs_count: 2,
                    descendant_files_count: 9,
                    descendant_files_total_size: 67,
                }
            );

            // Check one of the deeper fsnodes.
            let deep_fsnode_id = match all_fsnodes.get(&MPath::new("dir1/subdir1").unwrap()) {
                Some(Entry::Tree(fsnode_id)) => fsnode_id,
                _ => panic!("dir1/subdir1 fsnode should be a tree"),
            };
            let deep_fsnode = deep_fsnode_id
                .load(&ctx, repo.repo_blobstore())
                .await
                .unwrap();
            let deep_fsnode_entries: Vec<_> = deep_fsnode.list().collect();
            assert_eq!(
                deep_fsnode_entries,
                vec![
                    (
                        &MPathElement::new(b"file_1".to_vec()).unwrap(),
                        &FsnodeEntry::File(FsnodeFile::new(
                            ContentId::from_str(
                                "271119c630193464c55cf7feb72c16dd18b953e65ee85047d4d956381cdb96d9",
                            )
                            .unwrap(),
                            FileType::Regular,
                            9,
                            Sha1::from_str("eba7b799b1e587506677c108eaa51ca273ddcfdc").unwrap(),
                            Sha256::from_str(
                                "8bfab644c313b0227b1862786426697a9b5283eea8eb6066cc8d8e134ce0daa6",
                            )
                            .unwrap(),
                        )),
                    ),
                    (
                        &MPathElement::new(b"subsubdir1".to_vec()).unwrap(),
                        &FsnodeEntry::Directory(FsnodeDirectory::new(
                            FsnodeId::from_str(
                                "a8deafcaa9fdafad84447d754b1e24140f1113e68b3f7ec17c55a4fd9c90fdc7",
                            )
                            .unwrap(),
                            FsnodeSummary {
                                simple_format_sha1: Sha1::from_str(
                                    "b58f168a508c4388f496b21acee468b13b89ac50",
                                )
                                .unwrap(),
                                simple_format_sha256: Sha256::from_str(
                                    "dc19c9a55b59d97dbb2912e5acb1bad17c1aa2835c4cf288f2d47bb0b0b539bc",
                                )
                                .unwrap(),
                                child_files_count: 1,
                                child_files_total_size: 9,
                                child_dirs_count: 0,
                                descendant_files_count: 1,
                                descendant_files_total_size: 9,
                            },
                        )),
                    ),
                    (
                        &MPathElement::new(b"subsubdir2".to_vec()).unwrap(),
                        &FsnodeEntry::Directory(FsnodeDirectory::new(
                            FsnodeId::from_str(
                                "3daadf48916e4b25e28e21a15de4352188154fcaa749207eb00fb72dbd812343",
                            )
                            .unwrap(),
                            FsnodeSummary {
                                simple_format_sha1: Sha1::from_str(
                                    "89babd6eab901598b3df0a0c3318b83bd2edfa4b",
                                )
                                .unwrap(),
                                simple_format_sha256: Sha256::from_str(
                                    "5fa3074b0a96f8ac8f5ab31266d1ce95b1c84a35e8b3f4b8108385881e9e9f15",
                                )
                                .unwrap(),
                                child_files_count: 2,
                                child_files_total_size: 18,
                                child_dirs_count: 0,
                                descendant_files_count: 2,
                                descendant_files_total_size: 18,
                            },
                        )),
                    ),
                ]
            );
            assert_eq!(
                deep_fsnode.summary(),
                &FsnodeSummary {
                    simple_format_sha1: Sha1::from_str("56daa486e90686b5ead6f3ce360a6cbb7b73f23f")
                        .unwrap(),
                    simple_format_sha256: Sha256::from_str(
                        "8c24cf135f74933775b0ac1453c127c987874beb6fde5454ccf9c6c8c726e032"
                    )
                    .unwrap(),
                    child_files_count: 1,
                    child_files_total_size: 9,
                    child_dirs_count: 2,
                    descendant_files_count: 4,
                    descendant_files_total_size: 36,
                }
            );

            root_fsnode_id
        };

        {
            let child_hg_cs = "051946ed218061e925fb120dac02634f9ad40ae2";
            let (_bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, child_hg_cs)
                .await
                .unwrap();

            let f = derive_fsnode(
                &ctx,
                &derivation_ctx,
                vec![parent_fsnode_id.clone()],
                get_file_changes(&bcs),
            );

            let root_fsnode_id = f.await.unwrap();

            // Make sure it's saved in the blobstore
            let root_fsnode = root_fsnode_id
                .load(&ctx, repo.repo_blobstore())
                .await
                .unwrap();

            // Make sure the fsnodes describe the full manifest.
            let all_fsnodes: BTreeMap<_, _> =
                iterate_all_manifest_entries(&ctx, &repo, Entry::Tree(root_fsnode_id))
                    .try_collect()
                    .await
                    .unwrap();
            assert_eq!(
                all_fsnodes.keys().collect::<Vec<_>>(),
                vec![
                    &MPath::ROOT,
                    &MPath::new("1").unwrap(),
                    &MPath::new("2").unwrap(),
                    &MPath::new("dir1").unwrap(),
                    &MPath::new("dir2").unwrap(),
                    &MPath::new("dir2/file_1_in_dir2").unwrap(),
                ]
            );

            // Make sure the root fsnode is correct.
            assert_eq!(
                root_fsnode.summary(),
                &FsnodeSummary {
                    simple_format_sha1: Sha1::from_str("753b41c7bf23bda7eabb71095867c2ddf3e485df")
                        .unwrap(),
                    simple_format_sha256: Sha256::from_str(
                        "d86a29b0e92bcbff179e20a27fde18aacedc225a6c4884a7760d2880794c9e51"
                    )
                    .unwrap(),
                    child_files_count: 3,
                    child_files_total_size: 16,
                    child_dirs_count: 1,
                    descendant_files_count: 4,
                    descendant_files_total_size: 25,
                }
            );
        }
    }

    /// Derivation pipeline of independent directories: derive dir1/ and dir2/
    /// separately, then assemble the root using known_entries. Verifies the
    /// result matches normal full-tree derivation.
    #[mononoke::fbinit_test]
    async fn test_derivation_pipeline_independent_dirs(fb: FacebookInit) {
        let repo: TestRepo = ManyFilesDirs::get_repo(fb).await;
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
        let ctx = CoreContext::test_mock(fb);
        let blobstore = repo.repo_blobstore().clone();

        // Derive parent fsnode (commit A: creates "1")
        let parent_fsnode_id = {
            let parent_hg_cs = "5a28e25f924a5d209b82ce0713d8d83e68982bc8";
            let (_bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, parent_hg_cs)
                .await
                .unwrap();
            derive_fsnode(&ctx, &derivation_ctx, vec![], get_file_changes(&bcs))
                .await
                .unwrap()
        };

        // Commit B adds: "2", "dir1/file_1_in_dir1", "dir1/file_2_in_dir1",
        // "dir1/subdir1/file_1", "dir2/file_1_in_dir2"
        let child_hg_cs = "2f866e7e549760934e31bf0420a873f65100ad63";
        let (_bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, child_hg_cs)
            .await
            .unwrap();
        let changes = get_file_changes(&bcs);

        // Normal derivation to get expected result
        let expected_id = derive_fsnode(
            &ctx,
            &derivation_ctx,
            vec![parent_fsnode_id],
            changes.clone(),
        )
        .await
        .unwrap();

        // Stage dir1: pass all changes with prefix
        let dir1_parent = parent_fsnode_id
            .find_entry(ctx.clone(), blobstore.clone(), MPath::new("dir1").unwrap())
            .await
            .unwrap();
        let dir1_parents = vec![dir1_parent];
        let dir1_entry = derive_fsnode_entry(
            &ctx,
            &derivation_ctx,
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
        let dir2_parent = parent_fsnode_id
            .find_entry(ctx.clone(), blobstore.clone(), MPath::new("dir2").unwrap())
            .await
            .unwrap();
        let dir2_parents = vec![dir2_parent];
        let dir2_entry = derive_fsnode_entry(
            &ctx,
            &derivation_ctx,
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
        let root_entry = derive_fsnode_entry(
            &ctx,
            &derivation_ctx,
            ManifestDerivationInput::Entry {
                parents: vec![Some(Entry::Tree(parent_fsnode_id))],
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

        // Content-addressed: if FsnodeIds match, the trees are identical.
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
        let blobstore = repo.repo_blobstore().clone();

        // Derive parent fsnodes for commits A and B
        let parent_fsnode_id = {
            let hg_cs_a = "5a28e25f924a5d209b82ce0713d8d83e68982bc8";
            let (_bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, hg_cs_a)
                .await
                .unwrap();
            derive_fsnode(&ctx, &derivation_ctx, vec![], get_file_changes(&bcs))
                .await
                .unwrap()
        };
        let parent_fsnode_id = {
            let hg_cs_b = "2f866e7e549760934e31bf0420a873f65100ad63";
            let (_bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, hg_cs_b)
                .await
                .unwrap();
            derive_fsnode(
                &ctx,
                &derivation_ctx,
                vec![parent_fsnode_id],
                get_file_changes(&bcs),
            )
            .await
            .unwrap()
        };

        // Commit C modifies: "dir1/subdir1/subsubdir1/file_1",
        // "dir1/subdir1/subsubdir2/file_1", "dir1/subdir1/subsubdir2/file_2"
        let child_hg_cs = "d261bc7900818dea7c86935b3fb17a33b2e3a6b4";
        let (_bcs_id, bcs) = bonsai_changeset_from_hg(&ctx, &repo, child_hg_cs)
            .await
            .unwrap();
        let changes = get_file_changes(&bcs);

        // Normal derivation to get expected result
        let expected_id = derive_fsnode(
            &ctx,
            &derivation_ctx,
            vec![parent_fsnode_id],
            changes.clone(),
        )
        .await
        .unwrap();

        // Stage dir1/subdir1: pass all changes with prefix
        let subdir1_parent = parent_fsnode_id
            .find_entry(
                ctx.clone(),
                blobstore.clone(),
                MPath::new("dir1/subdir1").unwrap(),
            )
            .await
            .unwrap();
        let subdir1_parents = vec![subdir1_parent];
        let subdir1_entry = derive_fsnode_entry(
            &ctx,
            &derivation_ctx,
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
        let dir1_parent = parent_fsnode_id
            .find_entry(ctx.clone(), blobstore.clone(), MPath::new("dir1").unwrap())
            .await
            .unwrap();
        let dir1_parents = vec![dir1_parent];
        let dir1_known_entries = HashMap::from([(
            MPath::new("dir1/subdir1").unwrap(),
            KnownEntry::Entry(Some(subdir1_entry)),
        )]);
        let dir1_entry = derive_fsnode_entry(
            &ctx,
            &derivation_ctx,
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
        let root_entry = derive_fsnode_entry(
            &ctx,
            &derivation_ctx,
            ManifestDerivationInput::Entry {
                parents: vec![Some(Entry::Tree(parent_fsnode_id))],
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

        // Content-addressed: if FsnodeIds match, the trees are identical.
        assert_eq!(pipeline_id, expected_id);
    }
}
