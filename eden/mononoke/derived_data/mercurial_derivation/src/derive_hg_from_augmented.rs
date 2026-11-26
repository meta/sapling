/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Hg changeset derivation that takes a commit's root Hg tree id and changed
//! files from its augmented manifest instead of building Hg manifests from the
//! parents' Hg manifests. It still stores the commit's new Hg trees, converted
//! from the augmented manifest, so the Hg changeset ids and Hg tree bytes are
//! those that derivation from Hg manifests gives.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use blobrepo_common::changed_files::compute_changed_files_from_augmented;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use context::CoreContext;
use derived_data_manager::DerivationContext;
use futures::Stream;
use futures::StreamExt;
use futures::TryFutureExt;
use futures::TryStreamExt;
use futures::future;
use futures::future::try_join_all;
use futures::stream;
use futures::stream::BoxStream;
use manifest::Diff;
use manifest::Entry;
use manifest::ManifestOps;
use manifest::PathOrPrefix;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgChangesetId;
use mercurial_types::HgManifestId;
use mercurial_types::NULL_HASH;
use mercurial_types::blobs::store_manifest_envelope;
use mercurial_types::sharded_augmented_manifest::rebuild_hg_manifest_envelope;
use mercurial_types::subtree::HgSubtreeChanges;
use mononoke_macros::mononoke;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::RepoPath;
use mononoke_types::path::MPath;
use restricted_paths_common::ArcRestrictedPathsConfigBased;
use thiserror::Error;

use crate::derive_hg_changeset::generate_hg_changeset;
use crate::derive_hg_manifest::track_restricted_hg_tree;
use crate::mapping::HgChangesetDeriveOptions;
use crate::mapping::MappedHgChangesetId;
use crate::mapping::RootHgAugmentedManifestId;

const RETIREMENT_STAGE_KNOB: &str = "scm/mononoke:hgmanifest_retirement_stage";

/// How far a repo has gone in retiring Hg manifests, set per repo by
/// `scm/mononoke:hgmanifest_retirement_stage`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HgManifestRetirementStage {
    /// 0: Hg changesets are derived by building Hg manifests from the
    /// parents' Hg manifests.
    DeriveFromHg,
    /// 1: Hg changesets take their root tree id and changed files from
    /// augmented manifests, and Hg trees converted from them are stored.
    DeriveFromAugmented,
    /// 2: no Hg manifests are written. This build still writes them, as at 1.
    StopHgWrites,
}

#[derive(Debug, Error)]
#[error("{RETIREMENT_STAGE_KNOB} is {value} for repo {repo}, but must be 0, 1 or 2")]
pub(crate) struct InvalidRetirementStage {
    value: i64,
    repo: String,
}

impl HgManifestRetirementStage {
    pub(crate) fn for_repo(repo: &str) -> Result<Self, InvalidRetirementStage> {
        match justknobs::get(RETIREMENT_STAGE_KNOB, Some(repo)) {
            0 => Ok(Self::DeriveFromHg),
            1 => Ok(Self::DeriveFromAugmented),
            2 => Ok(Self::StopHgWrites),
            value => Err(InvalidRetirementStage {
                value,
                repo: repo.to_string(),
            }),
        }
    }

    pub(crate) fn derives_from_augmented(self) -> bool {
        match self {
            Self::DeriveFromHg => false,
            Self::DeriveFromAugmented | Self::StopHgWrites => true,
        }
    }
}

pub(crate) fn derives_from_augmented(derivation_ctx: &DerivationContext) -> Result<bool> {
    Ok(HgManifestRetirementStage::for_repo(derivation_ctx.repo_name())?.derives_from_augmented())
}

/// The directories of `tree` that differ from the directory at the same path
/// in every one of `parents`, with their paths under `path`. Directories at a
/// path in `skip`, relative to `tree`, and below them are left out. Only
/// directories are kept as the trees are compared, so a commit that changes
/// many files does not hold its file entries in memory.
pub(crate) fn new_hg_trees(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    path: MPath,
    tree: HgAugmentedManifestId,
    parents: Vec<HgAugmentedManifestId>,
    skip: Arc<HashSet<MPath>>,
) -> BoxStream<'static, Result<(MPath, HgAugmentedManifestId)>> {
    // A null tree is the empty tree, which has no Hg tree to store, and a
    // null parent is the empty tree, which every tree differs from. The diffs
    // below never ask `skip` about the root.
    if tree.into_nodehash() == NULL_HASH || skip.contains(&MPath::ROOT) {
        return stream::empty().boxed();
    }
    let mut parents = parents
        .into_iter()
        .filter(|parent| parent.into_nodehash() != NULL_HASH);
    let Some(p1) = parents.next() else {
        return tree
            .find_entries_filtered(
                ctx.clone(),
                blobstore.clone(),
                vec![PathOrPrefix::Prefix(MPath::ROOT)],
                move |dir: &MPath, _| !skip.contains(dir),
            )
            .try_filter_map(move |(relative_path, entry)| {
                future::ok(entry.into_tree().map(|id| (path.join(&relative_path), id)))
            })
            .boxed();
    };
    let other_parents: Vec<_> = parents.collect();

    // Directories in `skip` are not entered, so they are never output.
    let changed_over_p1 = p1.filtered_diff(
        ctx.clone(),
        blobstore.clone(),
        tree,
        blobstore.clone(),
        |diff| match diff {
            Diff::Added(path, Entry::Tree(id)) | Diff::Changed(path, _, Entry::Tree(id)) => {
                Some((path, id))
            }
            _ => None,
        },
        move |diff| match diff {
            Diff::Added(path, _) | Diff::Changed(path, ..) => !skip.contains(path),
            Diff::Removed(..) => false,
        },
        HashMap::new(),
    );
    if other_parents.is_empty() {
        return changed_over_p1
            .map_ok(move |(relative_path, id)| (path.join(&relative_path), id))
            .boxed();
    }

    let ctx = ctx.clone();
    let blobstore = blobstore.clone();
    async move {
        let changed: Vec<_> = changed_over_p1.try_collect().await?;
        let in_other_parents = try_join_all(other_parents.into_iter().map(|parent| {
            parent
                .find_entries(
                    ctx.clone(),
                    blobstore.clone(),
                    changed.iter().map(|(path, _)| path.clone()),
                )
                .try_collect::<HashMap<_, _>>()
        }))
        .await?;
        anyhow::Ok(stream::iter(
            changed
                .into_iter()
                .filter(move |(relative_path, id)| {
                    !in_other_parents
                        .iter()
                        .any(|entries| entries.get(relative_path) == Some(&Entry::Tree(*id)))
                })
                .map(move |(relative_path, id)| Ok((path.join(&relative_path), id))),
        ))
    }
    .try_flatten_stream()
    .boxed()
}

/// Store the Hg form of each directory of `tree` that `new_hg_trees` gives.
pub(crate) async fn store_hg_trees(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    restricted_paths: &ArcRestrictedPathsConfigBased,
    path: &MPath,
    tree: HgAugmentedManifestId,
    parents: Vec<HgAugmentedManifestId>,
    skip: Arc<HashSet<MPath>>,
) -> Result<()> {
    store_new_hg_trees(
        ctx,
        blobstore,
        restricted_paths,
        new_hg_trees(ctx, blobstore, path.clone(), tree, parents, skip),
    )
    .await
}

/// Store the Hg form of each `(path, directory)` in `trees`.
pub(crate) async fn store_new_hg_trees(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    restricted_paths: &ArcRestrictedPathsConfigBased,
    trees: impl Stream<Item = Result<(MPath, HgAugmentedManifestId)>>,
) -> Result<()> {
    trees
        .map_ok(|(path, id)| store_hg_tree(ctx, blobstore, restricted_paths, path, id))
        .try_buffer_unordered(100)
        .try_collect()
        .await
}

/// Store the Hg form of the augmented directory `id` found at `path`.
pub(crate) async fn store_hg_tree(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    restricted_paths: &ArcRestrictedPathsConfigBased,
    path: MPath,
    id: HgAugmentedManifestId,
) -> Result<()> {
    let mfid = HgManifestId::new(id.into_nodehash());
    let envelope = rebuild_hg_manifest_envelope(ctx, blobstore, mfid)
        .await
        .with_context(|| format!("failed to convert augmented manifest {id} at {path}"))?;
    let path = match path.into_optional_non_root_path() {
        None => RepoPath::RootPath,
        Some(path) => RepoPath::DirectoryPath(path),
    };
    track_restricted_hg_tree(ctx, restricted_paths, mfid, &path).await?;
    store_manifest_envelope(ctx, blobstore, envelope).await?;
    Ok(())
}

/// Fail unless the augmented root stored for `parent` is `hg_root`, the root
/// tree of its Hg changeset. Derivation from augmented manifests reads a
/// parent's tree as the augmented record stored under its Hg root tree id, so
/// a parent mapped to another root would give the commit Hg trees built
/// against the wrong parent.
pub(crate) async fn check_parent_root(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    parent: ChangesetId,
    hg_root: HgManifestId,
) -> Result<()> {
    let augmented_root = derivation_ctx
        .fetch_dependency::<RootHgAugmentedManifestId>(ctx, parent)
        .await?
        .hg_augmented_manifest_id();
    ensure!(
        augmented_root.into_nodehash() == hg_root.into_nodehash(),
        "parent {parent} maps to augmented root {augmented_root}, but its Hg changeset's root tree is {hg_root}"
    );
    Ok(())
}

/// Derive the Hg changeset of `bonsai` from its augmented manifest. The
/// commit's new Hg trees are stored before its Hg changeset.
pub(crate) async fn derive_from_augmented(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: BonsaiChangeset,
    parents: Vec<MappedHgChangesetId>,
    subtree_change_sources: HashMap<ChangesetId, HgChangesetId>,
    options: &HgChangesetDeriveOptions,
) -> Result<MappedHgChangesetId> {
    let root = derivation_ctx
        .fetch_dependency::<RootHgAugmentedManifestId>(ctx, bonsai.get_changeset_id())
        .await?
        .hg_augmented_manifest_id();
    derive_from_augmented_root(
        ctx,
        derivation_ctx,
        bonsai,
        root,
        parents,
        subtree_change_sources,
        options,
    )
    .await
}

/// Derive the Hg changeset of `bonsai` as `derive_from_augmented` does, from
/// the augmented manifest `root`.
pub(crate) async fn derive_from_augmented_root(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: BonsaiChangeset,
    root: HgAugmentedManifestId,
    parents: Vec<MappedHgChangesetId>,
    subtree_change_sources: HashMap<ChangesetId, HgChangesetId>,
    options: &HgChangesetDeriveOptions,
) -> Result<MappedHgChangesetId> {
    let blobstore = derivation_ctx.blobstore();
    let parents = try_join_all(bonsai.parents().zip(parents.iter()).map(
        |(parent_cs_id, parent)| async move {
            let parent = parent.hg_changeset_id().load(ctx, blobstore).await?;
            check_parent_root(ctx, derivation_ctx, parent_cs_id, parent.manifestid()).await?;
            anyhow::Ok(parent)
        },
    ))
    .await?;
    let parent_roots: Vec<_> = parents
        .iter()
        .map(|parent| HgAugmentedManifestId::from(parent.manifestid()))
        .collect();
    let subtree_changes = HgSubtreeChanges::from_bonsai_subtree_changes(
        bonsai.subtree_changes(),
        subtree_change_sources,
    )?;

    let ((), files) = future::try_join(
        store_hg_trees(
            ctx,
            blobstore,
            &derivation_ctx.restricted_paths(),
            &MPath::ROOT,
            root,
            parent_roots.clone(),
            Arc::new(HashSet::new()),
        ),
        // In its own task, so that it runs beside storing the Hg trees.
        async {
            mononoke::spawn_task(compute_changed_files_from_augmented(
                ctx.clone(),
                blobstore.clone(),
                root,
                parent_roots.first().copied(),
                parent_roots.get(1).copied(),
            ))
            .await?
        },
    )
    .await?;
    let (hg_cs_id, _) = generate_hg_changeset(
        ctx,
        blobstore,
        bonsai,
        HgManifestId::new(root.into_nodehash()),
        parents
            .iter()
            .map(|parent| parent.get_changeset_id())
            .collect(),
        files,
        subtree_changes,
        options,
    )
    .await?;
    Ok(MappedHgChangesetId::new(hg_cs_id))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use filestore::FilestoreConfig;
    use mononoke_types::NonRootMPath;
    use repo_blobstore::RepoBlobstore;
    use repo_blobstore::RepoBlobstoreArc;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;
    use tests_utils::CreateCommitContext;

    use super::*;

    #[derive(Clone)]
    #[facet::container]
    struct TestRepo {
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

    #[mononoke::fbinit_test]
    async fn null_tree_has_no_new_hg_trees(fb: fbinit::FacebookInit) -> Result<()> {
        // Given: the null tree, as the root of an uploaded commit with no
        // files, and its null parent.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let blobstore: Arc<dyn KeyedBlobstore> = repo.repo_blobstore_arc();
        let null = HgAugmentedManifestId::new(NULL_HASH);

        // When: listing its new Hg trees.
        let trees: Vec<_> = new_hg_trees(
            &ctx,
            &blobstore,
            MPath::ROOT,
            null,
            vec![null],
            Arc::new(HashSet::new()),
        )
        .try_collect()
        .await?;

        // Then: there is none to store.
        assert_eq!(trees, vec![]);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn new_hg_trees_keeps_only_directories(fb: fbinit::FacebookInit) -> Result<()> {
        // Given: a parent with 20 directories of 50 files each, a child that
        // changes every file of the first 10 directories, and a merge of the
        // child with an unrelated commit.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let files = |dirs: std::ops::Range<usize>, content: &str| {
            dirs.flat_map(|dir| {
                (0..50).map(move |file| {
                    (
                        format!("dir{dir}/file{file}").as_str().try_into().unwrap(),
                        content.to_string(),
                    )
                })
            })
            .collect::<Vec<(NonRootMPath, String)>>()
        };
        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .add_files(files(0..20, "one"))
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .add_files(files(0..10, "two"))
            .commit()
            .await?;
        let other = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("other/file", "other")
            .commit()
            .await?;
        let merge = CreateCommitContext::new(&ctx, &repo, vec![child, other])
            .commit()
            .await?;
        let blobstore: Arc<dyn KeyedBlobstore> = repo.repo_blobstore_arc();
        let root = |cs_id| {
            let ctx = ctx.clone();
            let repo = repo.clone();
            async move {
                anyhow::Ok(
                    repo.repo_derived_data()
                        .derive::<RootHgAugmentedManifestId>(
                            &ctx,
                            cs_id,
                            derivation_queue_thrift::DerivationPriority::LOW,
                        )
                        .await?
                        .hg_augmented_manifest_id(),
                )
            }
        };
        let (parent, child, other, merge) =
            futures::try_join!(root(parent), root(child), root(other), root(merge))?;
        let list = |tree, parents| {
            new_hg_trees(
                &ctx,
                &blobstore,
                MPath::ROOT,
                tree,
                parents,
                Arc::new(HashSet::new()),
            )
            .map_ok(|(path, _)| path.to_string())
            .try_collect::<BTreeSet<_>>()
        };

        // When: listing the child's new Hg trees, and the merge's.
        let child_trees = list(child, vec![parent]).await?;
        let merge_trees = list(merge, vec![child, other]).await?;

        // Then: only the root and the 10 changed directories are listed for
        // the child, none of its 500 changed files, and only the root for the
        // merge, whose other directories each match one of its parents.
        let expected: BTreeSet<_> = std::iter::once(String::new())
            .chain((0..10).map(|dir| format!("dir{dir}")))
            .collect();
        assert_eq!(child_trees, expected);
        assert_eq!(merge_trees, BTreeSet::from([String::new()]));
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn new_hg_trees_skips_a_skipped_root(fb: fbinit::FacebookInit) -> Result<()> {
        // Given: a child that changes its root and a directory over its
        // parent, and a merge of it with an unrelated commit.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("dir/a", "a")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .add_file("dir/b", "b")
            .add_file("c", "c")
            .commit()
            .await?;
        let other = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("other/file", "other")
            .commit()
            .await?;
        let merge = CreateCommitContext::new(&ctx, &repo, vec![child, other])
            .commit()
            .await?;
        let blobstore: Arc<dyn KeyedBlobstore> = repo.repo_blobstore_arc();
        let root = |cs_id| {
            let ctx = ctx.clone();
            let repo = repo.clone();
            async move {
                anyhow::Ok(
                    repo.repo_derived_data()
                        .derive::<RootHgAugmentedManifestId>(
                            &ctx,
                            cs_id,
                            derivation_queue_thrift::DerivationPriority::LOW,
                        )
                        .await?
                        .hg_augmented_manifest_id(),
                )
            }
        };
        let (parent, child, other, merge) =
            futures::try_join!(root(parent), root(child), root(other), root(merge))?;

        // When: listing new Hg trees with the root in `skip`, with no parent,
        // one parent and two.
        for (name, tree, parents) in [
            ("no parent", child, vec![]),
            ("one parent", child, vec![parent]),
            ("a merge", merge, vec![child, other]),
        ] {
            let trees: Vec<_> = new_hg_trees(
                &ctx,
                &blobstore,
                MPath::ROOT,
                tree,
                parents,
                Arc::new(HashSet::from([MPath::ROOT])),
            )
            .try_collect()
            .await?;

            // Then: nothing is listed, as a skipped directory and everything
            // below it are left out.
            assert_eq!(trees, vec![], "{name}");
        }
        Ok(())
    }

    /// Holds back reads of one key until released.
    struct GatedBlobstore {
        inner: Arc<dyn KeyedBlobstore>,
        key: String,
        gate: future::Shared<futures::channel::oneshot::Receiver<()>>,
    }

    impl std::fmt::Debug for GatedBlobstore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "GatedBlobstore({})", self.key)
        }
    }

    impl std::fmt::Display for GatedBlobstore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Debug::fmt(self, f)
        }
    }

    #[async_trait::async_trait]
    impl KeyedBlobstore for GatedBlobstore {
        async fn get<'a>(
            &'a self,
            ctx: &'a CoreContext,
            key: &'a str,
        ) -> Result<Option<blobstore::BlobstoreGetData>> {
            if key == self.key {
                let _ = self.gate.clone().await;
            }
            self.inner.get(ctx, key).await
        }

        async fn put<'a>(
            &'a self,
            ctx: &'a CoreContext,
            key: String,
            value: blobstore::BlobstoreBytes,
        ) -> Result<()> {
            self.inner.put(ctx, key, value).await
        }

        async fn is_present<'a>(
            &'a self,
            ctx: &'a CoreContext,
            key: &'a str,
        ) -> Result<blobstore::BlobstoreIsPresent> {
            self.inner.is_present(ctx, key).await
        }

        async fn copy<'a>(
            &'a self,
            ctx: &'a CoreContext,
            old_key: &'a str,
            new_key: String,
        ) -> Result<()> {
            self.inner.copy(ctx, old_key, new_key).await
        }

        async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
            self.inner.unlink(ctx, key).await
        }
    }

    #[mononoke::fbinit_test]
    async fn new_hg_trees_streams_directories(fb: fbinit::FacebookInit) -> Result<()> {
        // Given: a child that changes a file in each of two directories, and a
        // blobstore that holds back reads of the child's second directory.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let parent = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a/file", "one")
            .add_file("b/file", "one")
            .commit()
            .await?;
        let child = CreateCommitContext::new(&ctx, &repo, vec![parent])
            .add_file("a/file", "two")
            .add_file("b/file", "two")
            .commit()
            .await?;
        let root = |cs_id| {
            let ctx = ctx.clone();
            let repo = repo.clone();
            async move {
                anyhow::Ok(
                    repo.repo_derived_data()
                        .derive::<RootHgAugmentedManifestId>(
                            &ctx,
                            cs_id,
                            derivation_queue_thrift::DerivationPriority::LOW,
                        )
                        .await?
                        .hg_augmented_manifest_id(),
                )
            }
        };
        let (parent, child) = futures::try_join!(root(parent), root(child))?;
        let inner: Arc<dyn KeyedBlobstore> = repo.repo_blobstore_arc();
        let Some(Entry::Tree(held)) = child
            .find_entry(ctx.clone(), inner.clone(), MPath::new("b")?)
            .await?
        else {
            anyhow::bail!("the child should have directory b");
        };
        let (release, gate) = futures::channel::oneshot::channel();
        let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(GatedBlobstore {
            inner,
            key: held.blobstore_key(),
            gate: future::FutureExt::shared(gate),
        });

        // When: reading the first directory listed while `b` is held back.
        let mut trees = new_hg_trees(
            &ctx,
            &blobstore,
            MPath::ROOT,
            child,
            vec![parent],
            Arc::new(HashSet::new()),
        );
        let first = trees.try_next().await?;

        // Then: it is listed before `b` is read, so the listing streams
        // rather than collecting the whole diff first, which would wait here
        // until the test times out.
        let (first, _) = first.context("the child has new directories")?;
        assert_ne!(first.to_string(), "b");
        release.send(()).ok();
        let rest: BTreeSet<_> = trees
            .map_ok(|(path, _)| path.to_string())
            .try_collect()
            .await?;
        let all: BTreeSet<_> = rest.into_iter().chain([first.to_string()]).collect();
        assert_eq!(
            all,
            BTreeSet::from(["".to_string(), "a".to_string(), "b".to_string()])
        );
        Ok(())
    }
}
