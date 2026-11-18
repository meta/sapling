/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::Storable;
use cloned::cloned;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use derived_data_manager::ManifestDerivationInput;
use derived_data_manager::ManifestStageOutput;
use either::Either;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future::FutureExt;
use futures::stream;
use manifest::Entry;
use manifest::LeafInfo;
use manifest::TreeInfoSubentries;
use manifest::derive_manifest;
use mononoke_types::BlobstoreValue;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ContentManifestId;
use mononoke_types::MPath;
use mononoke_types::NonRootMPath;
use mononoke_types::RepoPath;
use mononoke_types::TrieMap;
use mononoke_types::content_manifest::ContentManifest;
use mononoke_types::content_manifest::ContentManifestDirectory;
use mononoke_types::content_manifest::ContentManifestEntry;
use mononoke_types::content_manifest::ContentManifestFile;
use mononoke_types::content_manifest::ContentManifestRollupData;
use mononoke_types::sharded_map_v2::LoadableShardedMapV2Node;
use mononoke_types::sharded_map_v2::ShardedMapV2Node;
use restricted_paths_common::ManifestIdStoreWriteCallsite;
use restricted_paths_common::ManifestType;
use restricted_paths_common::RestrictedManifestId;
use restricted_paths_common::RestrictedPathManifestIdEntry;
use restricted_paths_common::RestrictedPathsConfigBased;
use restricted_paths_common::maybe_propagate_manifest_id_store_write_error;

use crate::ContentManifestDerivationError;

pub(crate) fn get_changes(
    bcs: &BonsaiChangeset,
) -> Vec<(NonRootMPath, Option<ContentManifestFile>)> {
    bcs.file_changes()
        .map(|(mpath, file_change)| {
            (
                mpath.clone(),
                file_change.simplify().map(|bc| ContentManifestFile {
                    content_id: bc.content_id(),
                    file_type: bc.file_type(),
                    size: bc.size(),
                }),
            )
        })
        .collect()
}

pub(crate) async fn empty_directory(
    ctx: &CoreContext,
    blobstore: &impl KeyedBlobstore,
) -> Result<ContentManifestId> {
    ContentManifest::empty()
        .into_blob()
        .store(ctx, blobstore)
        .await
}

/// Resolve a manifest entry, loading rollup data from blobstore if needed.
pub(crate) async fn resolve_entry(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    rollup_data: Option<ContentManifestRollupData>,
    entry: Entry<ContentManifestId, ContentManifestFile>,
) -> Result<ContentManifestEntry> {
    match entry {
        Entry::Leaf(file) => Ok(ContentManifestEntry::File(file)),
        Entry::Tree(id) => {
            let rollup_data = match rollup_data {
                Some(rollup) => rollup,
                None => {
                    let mf = id.load(ctx, blobstore).await?;
                    mf.subentries.rollup_data()
                }
            };
            Ok(ContentManifestEntry::Directory(ContentManifestDirectory {
                id,
                rollup_data,
            }))
        }
    }
}

pub(crate) async fn create_content_manifest_subentries(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    subentries: TreeInfoSubentries<
        ContentManifestId,
        ContentManifestFile,
        ContentManifestRollupData,
        LoadableShardedMapV2Node<ContentManifestEntry>,
    >,
) -> Result<ShardedMapV2Node<ContentManifestEntry>> {
    // Resolve entries, loading rollup data from blobstore for reused entries
    let subentries: Vec<_> = stream::iter(subentries)
        .map(|(prefix, entry_or_map)| {
            cloned!(ctx, blobstore);
            async move {
                let result = match entry_or_map {
                    Either::Left((rollup_data, entry)) => {
                        Either::Left(resolve_entry(&ctx, &blobstore, rollup_data, entry).await?)
                    }
                    Either::Right(map) => Either::Right(map),
                };
                anyhow::Ok((prefix, result))
            }
        })
        .buffered(100)
        .try_collect()
        .await?;

    let subentries: TrieMap<_> = subentries.into_iter().collect();
    let subentries =
        ShardedMapV2Node::from_entries_and_partial_maps(&ctx, &blobstore, subentries).await?;

    Ok(subentries)
}

pub(crate) async fn create_content_manifest_directory(
    ctx: CoreContext,
    blobstore: Arc<dyn KeyedBlobstore>,
    path: &MPath,
    restricted_paths: &Arc<RestrictedPathsConfigBased>,
    subentries: TreeInfoSubentries<
        ContentManifestId,
        ContentManifestFile,
        ContentManifestRollupData,
        LoadableShardedMapV2Node<ContentManifestEntry>,
    >,
) -> Result<(ContentManifestRollupData, ContentManifestId)> {
    let subentries =
        create_content_manifest_subentries(ctx.clone(), blobstore.clone(), subentries).await?;

    let rollup = subentries.rollup_data();

    let directory = ContentManifest { subentries };
    let blob = directory.into_blob();
    let id = *blob.id();
    blob.store(&ctx, &blobstore).await?;

    let restricted_paths_enabled = justknobs::eval(
        "scm/mononoke:enabled_restricted_paths_access_logging",
        None,
        Some("content_manifest_write"),
    );

    if restricted_paths_enabled {
        if let Some(non_root_path) = path.clone().into_optional_non_root_path() {
            let should_record = restricted_paths.should_record_manifest_id_entry(&non_root_path);
            if should_record {
                let entry = RestrictedPathManifestIdEntry::new(
                    ManifestType::ContentManifest,
                    RestrictedManifestId::from(&id.blake2().into_inner()),
                    RepoPath::DirectoryPath(non_root_path),
                )?;

                if let Err(e) = restricted_paths
                    .manifest_id_store()
                    .add_entry(&ctx, entry)
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
                        ManifestIdStoreWriteCallsite::CreateContentManifestDirectory,
                    )?;
                }
            }
        }
    }

    Ok((rollup, id))
}

pub(crate) async fn create_content_manifest_file(
    leaf_info: LeafInfo<ContentManifestFile, ContentManifestFile>,
) -> Result<(ContentManifestRollupData, ContentManifestFile)> {
    if let Some(file) = leaf_info.change {
        Ok((Default::default(), file))
    } else {
        let mut iter = leaf_info.parents.into_iter();
        let file = iter
            .next()
            .ok_or(ContentManifestDerivationError::NoParents)?;
        if iter.all(|next| next == file) {
            Ok((Default::default(), file))
        } else {
            Err(ContentManifestDerivationError::MergeConflictNotResolved)?
        }
    }
}

pub(crate) async fn derive_content_manifest(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: BonsaiChangeset,
    parents: Vec<ContentManifestId>,
) -> Result<ContentManifestId> {
    let blobstore = derivation_ctx.blobstore();
    let restricted_paths = derivation_ctx.restricted_paths();
    let changes = get_changes(&bonsai);
    let derive_fut = derive_manifest(
        ctx.clone(),
        blobstore.clone(),
        parents.clone(),
        changes,
        {
            cloned!(blobstore, ctx, restricted_paths);
            move |tree_info| {
                cloned!(blobstore, ctx, restricted_paths);
                async move {
                    create_content_manifest_directory(
                        ctx,
                        blobstore,
                        &tree_info.path,
                        &restricted_paths,
                        tree_info.subentries,
                    )
                    .await
                }
            }
        },
        create_content_manifest_file,
    )
    .boxed();
    let root = derive_fut.await?;
    match root {
        Some(root) => Ok(root),
        None => Ok(empty_directory(ctx, blobstore).await?),
    }
}

pub(crate) async fn derive_content_manifest_entry(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    input: ManifestDerivationInput<
        ContentManifestId,
        ContentManifestFile,
        LoadableShardedMapV2Node<ContentManifestEntry>,
    >,
    changes: Vec<(NonRootMPath, Option<ContentManifestFile>)>,
) -> Result<
    ManifestStageOutput<
        ContentManifestId,
        ContentManifestFile,
        LoadableShardedMapV2Node<ContentManifestEntry>,
    >,
> {
    let blobstore = derivation_ctx.blobstore();
    let restricted_paths = derivation_ctx.restricted_paths();
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
                        create_content_manifest_directory(
                            ctx,
                            blobstore,
                            &tree_info.path,
                            &restricted_paths,
                            tree_info.subentries,
                        )
                        .await
                    }
                }
            },
            create_content_manifest_file,
            |subentries| async move {
                Ok(LoadableShardedMapV2Node::Inlined(
                    create_content_manifest_subentries(ctx.clone(), blobstore.clone(), subentries)
                        .await?,
                ))
            },
        )
        .await
}

#[cfg(test)]
mod test {
    use std::collections::BTreeMap;
    use std::collections::HashMap;

    use anyhow::Result;
    use blobstore::Loadable;
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphRef;
    use commit_graph::CommitGraphWriter;
    use context::CoreContext;
    use derivation_queue_thrift::DerivationPriority;
    use derived_data_test_utils::iterate_all_manifest_entries;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use fixtures::TestRepoFixture;
    use futures::TryStreamExt;
    use manifest::Entry;
    use mononoke_macros::mononoke;
    use mononoke_types::ChangesetId;
    use mononoke_types::ChangesetIdPrefix;
    use mononoke_types::ChangesetIdsResolvedFromPrefix;
    use mononoke_types::ContentManifestId;
    use mononoke_types::MPath;
    use mononoke_types::MPathElement;
    use mononoke_types::content_manifest::ContentManifest;
    use mononoke_types::content_manifest::ContentManifestCounts;
    use mononoke_types::content_manifest::ContentManifestDirectory;
    use mononoke_types::content_manifest::ContentManifestEntry;
    use mononoke_types::content_manifest::ContentManifestFile;
    use mononoke_types::content_manifest::ContentManifestRollupData;
    use repo_blobstore::RepoBlobstore;
    use repo_blobstore::RepoBlobstoreRef;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;

    use crate::RootContentManifestId;

    #[facet::container]
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

    /// Derives content manifests for all commits in the ManyFilesDirs fixture.
    /// Returns manifest IDs in topological order: [A, B, C, D].
    async fn derive_all(ctx: &CoreContext, repo: &TestRepo) -> Result<Vec<ContentManifestId>> {
        let all_commits = match repo
            .commit_graph()
            .find_by_prefix(ctx, ChangesetIdPrefix::from_bytes("").unwrap(), 1000)
            .await?
        {
            ChangesetIdsResolvedFromPrefix::Multiple(cs) => cs,
            other => anyhow::bail!("Unexpected: {other:?}"),
        };

        // Build parent -> child map and find root for topological ordering
        let mut child_of: HashMap<ChangesetId, ChangesetId> = HashMap::new();
        let mut root = None;
        for &cs_id in &all_commits {
            let parents = repo.commit_graph().changeset_parents(ctx, cs_id).await?;
            if parents.is_empty() {
                root = Some(cs_id);
            } else {
                child_of.insert(parents[0], cs_id);
            }
        }

        // Walk the linear chain from root
        let root = root.ok_or_else(|| anyhow::anyhow!("no root commit found"))?;
        let ordered: Vec<_> =
            std::iter::successors(Some(root), |cs| child_of.get(cs).copied()).collect();

        // Derive each commit in topological order
        let derived_data = repo.repo_derived_data();
        let mut manifests = Vec::new();
        for cs_id in ordered {
            let id = derived_data
                .derive::<RootContentManifestId>(ctx, cs_id, DerivationPriority::LOW)
                .await?
                .into_content_manifest_id();
            manifests.push(id);
        }
        Ok(manifests)
    }

    async fn lookup_dir(
        mf: &ContentManifest,
        ctx: &CoreContext,
        blobstore: &impl blobstore::KeyedBlobstore,
        name: &str,
    ) -> ContentManifestDirectory {
        match mf
            .lookup(
                ctx,
                blobstore,
                &MPathElement::new(name.as_bytes().to_vec()).unwrap(),
            )
            .await
            .unwrap()
            .unwrap()
        {
            ContentManifestEntry::Directory(d) => d,
            other => panic!("expected directory for '{name}', got {other:?}"),
        }
    }

    async fn all_manifest_entries(
        ctx: &CoreContext,
        repo: &TestRepo,
        root_id: ContentManifestId,
    ) -> BTreeMap<MPath, Entry<ContentManifestId, ContentManifestFile>> {
        iterate_all_manifest_entries(ctx, repo, Entry::Tree(root_id))
            .try_collect()
            .await
            .unwrap()
    }

    /// Flat directory: file "1" (2 bytes). No subdirectories.
    /// Root descendant rollup = 1 file, 0 dirs, 2 bytes.
    #[mononoke::fbinit_test]
    async fn test_rollup_flat_directory(fb: FacebookInit) {
        let ctx = &CoreContext::test_mock(fb);
        let repo: TestRepo = fixtures::ManyFilesDirs::get_repo(fb).await;
        let blobstore = repo.repo_blobstore();
        let manifests = derive_all(ctx, &repo).await.unwrap();

        // Commit A
        let root_id = manifests[0];
        let entries = all_manifest_entries(ctx, &repo, root_id).await;
        let paths: Vec<_> = entries.keys().collect();
        assert_eq!(
            paths,
            vec![
                &MPath::ROOT,
                &MPath::new("1").unwrap(), // 2B
            ]
        );

        let mf = root_id.load(ctx, blobstore).await.unwrap();
        // 1 file, 0 dirs, 2B total
        assert_eq!(
            mf.subentries.rollup_data(),
            ContentManifestRollupData {
                child_counts: ContentManifestCounts {
                    files_count: 1,
                    dirs_count: 0,
                    files_total_size: 2,
                },
                descendant_counts: ContentManifestCounts {
                    files_count: 1,
                    dirs_count: 0,
                    files_total_size: 2,
                },
            }
        );
    }

    /// Deep tree at commit C of ManyFilesDirs.
    #[mononoke::fbinit_test]
    async fn test_rollup_deep_tree(fb: FacebookInit) {
        let ctx = &CoreContext::test_mock(fb);
        let repo: TestRepo = fixtures::ManyFilesDirs::get_repo(fb).await;
        let blobstore = repo.repo_blobstore();
        let manifests = derive_all(ctx, &repo).await.unwrap();

        // Commit C
        let root_id = manifests[2];
        let entries = all_manifest_entries(ctx, &repo, root_id).await;
        let paths: Vec<_> = entries.keys().collect();
        assert_eq!(
            paths,
            vec![
                &MPath::ROOT,
                &MPath::new("1").unwrap(),                       // 2B
                &MPath::new("2").unwrap(),                       // 2B
                &MPath::new("dir1").unwrap(),                    // dir
                &MPath::new("dir1/file_1_in_dir1").unwrap(),     // 9B
                &MPath::new("dir1/file_2_in_dir1").unwrap(),     // 9B
                &MPath::new("dir1/subdir1").unwrap(),            // dir
                &MPath::new("dir1/subdir1/file_1").unwrap(),     // 9B
                &MPath::new("dir1/subdir1/subsubdir1").unwrap(), // dir
                &MPath::new("dir1/subdir1/subsubdir1/file_1").unwrap(), // 9B
                &MPath::new("dir1/subdir1/subsubdir2").unwrap(), // dir
                &MPath::new("dir1/subdir1/subsubdir2/file_1").unwrap(), // 9B
                &MPath::new("dir1/subdir1/subsubdir2/file_2").unwrap(), // 9B
                &MPath::new("dir2").unwrap(),                    // dir
                &MPath::new("dir2/file_1_in_dir2").unwrap(),     // 9B
            ]
        );

        let mf = root_id.load(ctx, blobstore).await.unwrap();

        // Root: child counts = 2 files + 2 dirs, descendant counts = 9 files, 5 dirs, 67 bytes
        assert_eq!(
            mf.subentries.rollup_data(),
            ContentManifestRollupData {
                child_counts: ContentManifestCounts {
                    files_count: 2,
                    dirs_count: 2,
                    files_total_size: 4,
                },
                descendant_counts: ContentManifestCounts {
                    files_count: 9,
                    dirs_count: 5,
                    files_total_size: 67,
                },
            }
        );

        let dir1 = lookup_dir(&mf, ctx, blobstore, "dir1").await;
        assert_eq!(
            dir1.rollup_data.child_counts,
            ContentManifestCounts {
                files_count: 2,
                dirs_count: 1,
                files_total_size: 18,
            }
        );
        assert_eq!(
            dir1.rollup_data.descendant_counts,
            ContentManifestCounts {
                files_count: 6,
                dirs_count: 3,
                files_total_size: 54,
            }
        );

        let dir1_mf = dir1.id.load(ctx, blobstore).await.unwrap();
        let subdir1 = lookup_dir(&dir1_mf, ctx, blobstore, "subdir1").await;
        assert_eq!(
            subdir1.rollup_data.child_counts,
            ContentManifestCounts {
                files_count: 1,
                dirs_count: 2,
                files_total_size: 9,
            }
        );
        assert_eq!(
            subdir1.rollup_data.descendant_counts,
            ContentManifestCounts {
                files_count: 4,
                dirs_count: 2,
                files_total_size: 36,
            }
        );

        let subdir1_mf = subdir1.id.load(ctx, blobstore).await.unwrap();
        let subsubdir1 = lookup_dir(&subdir1_mf, ctx, blobstore, "subsubdir1").await;
        assert_eq!(
            subsubdir1.rollup_data.child_counts,
            ContentManifestCounts {
                files_count: 1,
                dirs_count: 0,
                files_total_size: 9,
            }
        );
        assert_eq!(
            subsubdir1.rollup_data.descendant_counts,
            ContentManifestCounts {
                files_count: 1,
                dirs_count: 0,
                files_total_size: 9,
            }
        );

        let subsubdir2 = lookup_dir(&subdir1_mf, ctx, blobstore, "subsubdir2").await;
        assert_eq!(
            subsubdir2.rollup_data.child_counts,
            ContentManifestCounts {
                files_count: 2,
                dirs_count: 0,
                files_total_size: 18,
            }
        );
        assert_eq!(
            subsubdir2.rollup_data.descendant_counts,
            ContentManifestCounts {
                files_count: 2,
                dirs_count: 0,
                files_total_size: 18,
            }
        );

        let dir2 = lookup_dir(&mf, ctx, blobstore, "dir2").await;
        assert_eq!(
            dir2.rollup_data.child_counts,
            ContentManifestCounts {
                files_count: 1,
                dirs_count: 0,
                files_total_size: 9,
            }
        );
        assert_eq!(
            dir2.rollup_data.descendant_counts,
            ContentManifestCounts {
                files_count: 1,
                dirs_count: 0,
                files_total_size: 9,
            }
        );
    }

    /// Commit D replaces the dir1/ directory with a file named "dir1" (12B).
    #[mononoke::fbinit_test]
    async fn test_rollup_after_dir_replaced_by_file(fb: FacebookInit) {
        let ctx = &CoreContext::test_mock(fb);
        let repo: TestRepo = fixtures::ManyFilesDirs::get_repo(fb).await;
        let blobstore = repo.repo_blobstore();
        let manifests = derive_all(ctx, &repo).await.unwrap();

        // Commit D
        let root_id = manifests[3];
        let entries = all_manifest_entries(ctx, &repo, root_id).await;
        let paths: Vec<_> = entries.keys().collect();
        assert_eq!(
            paths,
            vec![
                &MPath::ROOT,
                &MPath::new("1").unwrap(),                   // 2B
                &MPath::new("2").unwrap(),                   // 2B
                &MPath::new("dir1").unwrap(),                // 12B (file, was a dir)
                &MPath::new("dir2").unwrap(),                // dir
                &MPath::new("dir2/file_1_in_dir2").unwrap(), // 9B
            ]
        );

        // dir1 is now a file, not a directory
        assert!(matches!(
            entries[&MPath::new("dir1").unwrap()],
            Entry::Leaf(_)
        ));

        let mf = root_id.load(ctx, blobstore).await.unwrap();
        assert_eq!(
            mf.subentries.rollup_data(),
            ContentManifestRollupData {
                child_counts: ContentManifestCounts {
                    files_count: 3,
                    dirs_count: 1,
                    files_total_size: 16,
                },
                descendant_counts: ContentManifestCounts {
                    files_count: 4,
                    dirs_count: 1,
                    files_total_size: 25,
                },
            }
        );

        let dir2 = lookup_dir(&mf, ctx, blobstore, "dir2").await;
        assert_eq!(
            dir2.rollup_data.child_counts,
            ContentManifestCounts {
                files_count: 1,
                dirs_count: 0,
                files_total_size: 9,
            }
        );
        assert_eq!(
            dir2.rollup_data.descendant_counts,
            ContentManifestCounts {
                files_count: 1,
                dirs_count: 0,
                files_total_size: 9,
            }
        );
    }
}
