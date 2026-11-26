/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Derive Hg changesets from augmented manifests, as stage 1 of
//! `scm/mononoke:hgmanifest_retirement_stage` does, and compare them with the
//! stored Hg changesets and Hg trees. Every write goes to memory.
//!
//! Each commit is derived from two augmented roots: the one stored for it, and
//! one derived in memory from its Bonsai changeset, as stage 1 derives a new
//! commit. A commit matches when both give its stored Hg changeset id, write
//! every Hg tree that the stored Hg changeset adds over its parents, and write
//! each Hg tree with the bytes stored for it.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;

use acl_manifest::RootAclManifestId;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use blobstore::BlobstoreBytes;
use blobstore::BlobstoreGetData;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bookmarks::BookmarkKey;
use bookmarks::BookmarksRef;
use bookmarks::Freshness;
use cacheblob::MemWritesBlobstore;
use clap::Args;
use clap::ValueEnum;
use commit_graph::CommitGraphRef;
use context::CoreContext;
use derived_data_manager::DerivedDataManager;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use manifest::Entry;
use manifest::find_intersection_of_diffs;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_derivation::access_labels;
use mercurial_derivation::derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai_changeset;
use mercurial_derivation::derive_hg_augmented_manifest::normalize_acl_root;
use mercurial_derivation::derive_hg_changeset_from_augmented_root;
use mercurial_derivation::rebuild_hg_manifests::CommitHgRoots;
use mercurial_derivation::rebuild_hg_manifests::commit_hg_roots;
use mercurial_types::HgChangesetId;
use mercurial_types::HgManifestId;
use mercurial_types::HgNodeHash;
use mercurial_types::NULL_HASH;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::FileChange;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use restricted_paths_common::NoopRestrictedPathsManifestIdStore;
use restricted_paths_common::RestrictedPathsConfigBased;
use samplingblob::SamplingBlobstore;
use samplingblob::SamplingHandler;
use strum::Display;

use super::Repo;
use super::verify_aug_direct::OverwritingOverlay;

const HG_MANIFEST_KEY: &str = "hgmanifest.sha1.";

#[derive(Args)]
pub(super) struct VerifyHgFromAugmentedArgs {
    /// Bonsai changesets to check. Their parents' Hg changesets and their own
    /// Hg changesets and augmented manifests must be derived.
    #[clap(
        long = "commit-id",
        short = 'i',
        conflicts_with = "bookmark",
        required_unless_present = "bookmark"
    )]
    commit_ids: Vec<ChangesetId>,

    /// Check ancestors of this bookmark, newest first.
    #[clap(long, short = 'B')]
    bookmark: Option<BookmarkKey>,

    /// How many ancestors of --bookmark to look at.
    #[clap(long, default_value_t = 1000, requires = "bookmark")]
    limit: usize,

    /// Check only ancestors of --bookmark with one of these kinds of change.
    #[clap(long, value_delimiter = ',', requires = "bookmark")]
    kinds: Vec<ChangeKind>,

    /// How many commits to check at once.
    #[clap(long, default_value = "10")]
    concurrency: NonZeroUsize,
}

/// The kinds of change a commit makes. A commit can have several.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, ValueEnum)]
enum ChangeKind {
    Merge,
    Subtree,
    Copy,
    Rename,
    Deletion,
    Empty,
    Other,
}

fn change_kinds(bonsai: &BonsaiChangeset) -> Vec<ChangeKind> {
    let deleted: HashSet<_> = bonsai
        .file_changes()
        .filter(|(_, change)| matches!(change, FileChange::Deletion))
        .map(|(path, _)| path)
        .collect();
    let copies: Vec<_> = bonsai
        .file_changes()
        .filter_map(|(_, change)| change.copy_from())
        .collect();
    let kinds: Vec<_> = [
        (bonsai.parents().count() > 1, ChangeKind::Merge),
        (bonsai.has_subtree_changes(), ChangeKind::Subtree),
        (!copies.is_empty(), ChangeKind::Copy),
        (
            copies.iter().any(|(from, _)| deleted.contains(from)),
            ChangeKind::Rename,
        ),
        (!deleted.is_empty(), ChangeKind::Deletion),
        (
            bonsai.file_changes().len() == 0 && !bonsai.has_subtree_changes(),
            ChangeKind::Empty,
        ),
    ]
    .into_iter()
    .filter_map(|(has, kind)| has.then_some(kind))
    .collect();
    if kinds.is_empty() {
        vec![ChangeKind::Other]
    } else {
        kinds
    }
}

/// Where the augmented root that the Hg changeset is derived from comes from.
#[derive(Clone, Copy, Debug, Display)]
#[strum(serialize_all = "lowercase")]
enum RootSource {
    /// The augmented manifest stored for the commit.
    Stored,
    /// An augmented manifest derived in memory from the Bonsai changeset.
    Direct,
}

/// Records the Hg trees written through it, by id.
#[derive(Debug, Default)]
struct HgTreeWrites(Mutex<HashMap<HgNodeHash, BlobstoreBytes>>);

impl SamplingHandler for HgTreeWrites {
    fn sample_get(
        &self,
        _ctx: &CoreContext,
        _key: &str,
        _value: Option<&BlobstoreGetData>,
    ) -> Result<()> {
        Ok(())
    }

    fn sample_put(&self, _ctx: &CoreContext, key: &str, value: &BlobstoreBytes) -> Result<()> {
        if let Some((_, hex)) = key.split_once(HG_MANIFEST_KEY) {
            self.0
                .lock()
                .expect("lock should not be poisoned")
                .insert(hex.parse()?, value.clone());
        }
        Ok(())
    }
}

/// A derived data manager whose writes all go to a new in-memory overlay,
/// and the Hg trees it writes.
fn in_memory_manager(
    repo: &Repo,
    manager: &DerivedDataManager,
) -> (DerivedDataManager, Arc<HgTreeWrites>) {
    let writes = Arc::new(HgTreeWrites::default());
    let blobstore =
        RepoBlobstore::new_with_wrapped_inner_blobstore(repo.repo_blobstore().clone(), |inner| {
            Arc::new(SamplingBlobstore::new(
                OverwritingOverlay(Arc::new(MemWritesBlobstore::new(inner))),
                writes.clone(),
            ))
        });
    // Recording restricted-path manifest ids is a write, so it goes nowhere.
    let derivation_ctx = manager.derivation_context(None);
    let restricted_paths = Arc::new(RestrictedPathsConfigBased::new(
        derivation_ctx.restricted_paths().config().clone(),
        Arc::new(NoopRestrictedPathsManifestIdStore::new(
            derivation_ctx.repo_id(),
        )),
        None,
    ));
    let manager = manager
        .with_replaced_blobstore(blobstore)
        .with_replaced_restricted_paths(restricted_paths);
    (manager, writes)
}

/// The Hg trees that the commit's stored Hg changeset adds over its parents'.
async fn expected_new_trees(
    ctx: &CoreContext,
    repo: &Repo,
    roots: CommitHgRoots,
) -> Result<HashSet<HgNodeHash>> {
    // The null root is the empty tree, which has no Hg tree to write.
    if roots.root.into_nodehash() == NULL_HASH {
        return Ok(HashSet::new());
    }
    let parents = roots
        .parents
        .into_iter()
        .filter(|id| id.into_nodehash() != NULL_HASH)
        .collect();
    find_intersection_of_diffs(
        ctx.with_access_label(access_labels::HG_FROM_AUGMENTED_CHECK),
        repo.repo_blobstore().clone(),
        roots.root,
        parents,
    )
    .try_filter_map(|(_, entry)| async move {
        Ok(match entry {
            Entry::Tree(id) => Some(id.into_nodehash()),
            Entry::Leaf(_) => None,
        })
    })
    .try_collect()
    .await
}

/// Derive the commit's Hg changeset in memory from `source`. Returns how many
/// Hg trees were written, and each way the result differs from the stored one.
async fn check_source(
    ctx: &CoreContext,
    repo: &Repo,
    manager: &DerivedDataManager,
    bonsai: &BonsaiChangeset,
    source: RootSource,
    stored_hg_cs_id: HgChangesetId,
    expected_trees: &HashSet<HgNodeHash>,
) -> Result<(usize, Vec<String>)> {
    let cs_id = bonsai.get_changeset_id();
    let (manager, writes) = in_memory_manager(repo, manager);
    let derivation_ctx = manager.derivation_context(None);
    let root = match source {
        RootSource::Stored => manager
            .fetch_derived::<RootHgAugmentedManifestId>(ctx, cs_id, None)
            .await?
            .ok_or_else(|| anyhow!("{cs_id} has no augmented manifest"))?
            .hg_augmented_manifest_id(),
        RootSource::Direct => {
            let acl_root = manager
                .fetch_derived::<RootAclManifestId>(ctx, cs_id, None)
                .await?
                .ok_or_else(|| anyhow!("{cs_id} has no ACL manifest"))?;
            let parents = derivation_ctx
                .fetch_parents::<RootHgAugmentedManifestId>(ctx, bonsai)
                .await?
                .iter()
                .map(RootHgAugmentedManifestId::hg_augmented_manifest_id)
                .collect();
            derive_augmented_manifest_from_bonsai_changeset(
                ctx,
                derivation_ctx.blobstore(),
                bonsai,
                parents,
                &derivation_ctx.restricted_paths(),
                normalize_acl_root(&acl_root)?,
            )
            .await
            .with_context(|| format!("deriving the augmented manifest of {cs_id} in memory"))?
        }
    };
    let hg_cs_id =
        derive_hg_changeset_from_augmented_root(ctx, &derivation_ctx, bonsai.clone(), root)
            .await
            .with_context(|| format!("deriving the Hg changeset of {cs_id} from {source}"))?;

    let mut differences = Vec::new();
    if hg_cs_id != stored_hg_cs_id {
        differences.push(format!(
            "Hg changeset {hg_cs_id} is not the stored {stored_hg_cs_id}"
        ));
    }
    let written = std::mem::take(&mut *writes.0.lock().expect("lock should not be poisoned"));
    for id in expected_trees {
        if !written.contains_key(id) {
            differences.push(format!("Hg tree {id} was not written"));
        }
    }
    let stored = repo.repo_blobstore();
    let stored_ctx = &ctx.with_access_label(access_labels::HG_FROM_AUGMENTED_CHECK);
    let trees_written = written.len();
    let tree_differences: Vec<String> = futures::stream::iter(written)
        .map(|(id, bytes)| async move {
            let key = HgManifestId::new(id).blobstore_key();
            anyhow::Ok(match stored.get(stored_ctx, &key).await? {
                None => Some(format!("Hg tree {id} is not stored")),
                Some(stored) if stored.as_raw_bytes() != bytes.as_bytes() => {
                    Some(format!("Hg tree {id} differs from the stored bytes"))
                }
                Some(_) => None,
            })
        })
        .buffer_unordered(100)
        .try_filter_map(future::ok)
        .try_collect()
        .await?;
    differences.extend(tree_differences);
    Ok((
        trees_written,
        differences
            .into_iter()
            .map(|difference| format!("{cs_id} from {source} root: {difference}"))
            .collect(),
    ))
}

/// Check one commit from both roots, returning how many Hg trees were written
/// and its differences. A commit whose Hg changeset or augmented manifest is
/// not derived yet is skipped.
async fn check_commit(
    ctx: &CoreContext,
    repo: &Repo,
    manager: &DerivedDataManager,
    bonsai: BonsaiChangeset,
) -> Result<Option<(usize, Vec<String>)>> {
    let cs_id = bonsai.get_changeset_id();
    let (Some(stored_hg_cs_id), Some(_)) = future::try_join(
        repo.bonsai_hg_mapping().get_hg_from_bonsai(ctx, cs_id),
        async {
            anyhow::Ok(
                manager
                    .fetch_derived::<RootHgAugmentedManifestId>(ctx, cs_id, None)
                    .await?,
            )
        },
    )
    .await?
    else {
        return Ok(None);
    };
    let roots =
        commit_hg_roots(ctx, repo.repo_blobstore(), repo.bonsai_hg_mapping(), cs_id).await?;
    let expected_trees = expected_new_trees(ctx, repo, roots).await?;
    let mut differences = Vec::new();
    let mut trees_written = 0;
    for source in [RootSource::Stored, RootSource::Direct] {
        match check_source(
            ctx,
            repo,
            manager,
            &bonsai,
            source,
            stored_hg_cs_id,
            &expected_trees,
        )
        .await
        {
            Ok((written, source_differences)) => {
                trees_written += written;
                differences.extend(source_differences);
            }
            Err(err) => differences.push(format!("{cs_id} from {source} root: error: {err:#}")),
        }
    }
    Ok(Some((trees_written, differences)))
}

async fn commits_to_check(
    ctx: &CoreContext,
    repo: &Repo,
    args: &VerifyHgFromAugmentedArgs,
) -> Result<Vec<ChangesetId>> {
    match &args.bookmark {
        None => Ok(args.commit_ids.clone()),
        Some(bookmark) => {
            let head = repo
                .bookmarks()
                .get(ctx.clone(), bookmark, Freshness::MostRecent)
                .await?
                .ok_or_else(|| anyhow!("bookmark {bookmark} not found"))?;
            repo.commit_graph()
                .ancestors_difference_stream(ctx, vec![head], vec![])
                .await?
                .take(args.limit)
                .try_collect()
                .await
        }
    }
}

/// What checking one commit found.
enum CommitOutcome {
    /// The commit has none of the kinds of change asked for.
    FilteredOut,
    /// The commit's Hg changeset or augmented manifest is not derived yet.
    NotDerived,
    /// The commit's kinds of change, how many Hg trees were written, and
    /// each way it differs from the stored data, or why it could not be
    /// checked.
    Checked(Vec<ChangeKind>, usize, Vec<String>),
}

/// Load and check one commit. An error is reported as that commit's
/// mismatch, so that one commit that fails to load or derive does not end
/// the run.
async fn load_and_check_commit(
    ctx: &CoreContext,
    repo: &Repo,
    manager: &DerivedDataManager,
    kinds_wanted: &[ChangeKind],
    cs_id: ChangesetId,
) -> CommitOutcome {
    let bonsai = match cs_id.load(ctx, repo.repo_blobstore()).await {
        Ok(bonsai) => bonsai,
        Err(err) => {
            return CommitOutcome::Checked(
                Vec::new(),
                0,
                vec![format!(
                    "{cs_id}: error: failed to load the commit: {err:#}"
                )],
            );
        }
    };
    let kinds = change_kinds(&bonsai);
    if !kinds_wanted.is_empty() && !kinds.iter().any(|kind| kinds_wanted.contains(kind)) {
        return CommitOutcome::FilteredOut;
    }
    match check_commit(ctx, repo, manager, bonsai).await {
        Ok(None) => CommitOutcome::NotDerived,
        Ok(Some((trees_written, differences))) => {
            CommitOutcome::Checked(kinds, trees_written, differences)
        }
        Err(err) => CommitOutcome::Checked(kinds, 0, vec![format!("{cs_id}: error: {err:#}")]),
    }
}

pub(super) async fn verify_hg_from_augmented(
    ctx: &CoreContext,
    repo: &Repo,
    manager: &DerivedDataManager,
    args: VerifyHgFromAugmentedArgs,
) -> Result<()> {
    let cs_ids = commits_to_check(ctx, repo, &args).await?;
    println!("looking at {} commits", cs_ids.len());

    let outcomes: Vec<_> = futures::stream::iter(cs_ids)
        .map(|cs_id| load_and_check_commit(ctx, repo, manager, &args.kinds, cs_id))
        .buffer_unordered(args.concurrency.get())
        .collect()
        .await;
    let filtered_out = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, CommitOutcome::FilteredOut))
        .count();
    let not_derived = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, CommitOutcome::NotDerived))
        .count();
    let results: Vec<_> = outcomes
        .into_iter()
        .filter_map(|outcome| match outcome {
            CommitOutcome::Checked(kinds, trees_written, differences) => {
                Some((kinds, trees_written, differences))
            }
            CommitOutcome::FilteredOut | CommitOutcome::NotDerived => None,
        })
        .collect();
    if !args.kinds.is_empty() {
        println!("skipped {filtered_out} commits without the kinds of change asked for");
    }
    println!("skipped {not_derived} commits that are not derived yet");

    // For each kind of change: commits checked, and commits that matched.
    let mut by_kind: BTreeMap<ChangeKind, (usize, usize)> = BTreeMap::new();
    let mut mismatched = 0;
    let mut trees_compared = 0;
    for (kinds, trees_written, differences) in &results {
        trees_compared += trees_written;
        for kind in kinds {
            let counts = by_kind.entry(*kind).or_default();
            counts.0 += 1;
            counts.1 += usize::from(differences.is_empty());
        }
        mismatched += usize::from(!differences.is_empty());
        for difference in differences {
            println!("MISMATCH {difference}");
        }
    }
    for (kind, (checked, matched)) in by_kind {
        println!("{kind:?}: checked={checked} matched={matched}");
    }
    println!(
        "done: checked={} mismatched={mismatched} trees_compared={trees_compared}",
        results.len()
    );
    if mismatched > 0 {
        bail!("{mismatched} commits did not match their stored Hg changesets");
    }
    if results.is_empty() {
        bail!("no commit was checked");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use clap::Parser;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use mononoke_macros::mononoke;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;
    use tests_utils::CreateCommitContext;

    use super::*;

    #[facet::container]
    struct TestRepo {
        #[delegate(
            dyn BonsaiHgMapping,
            dyn Bookmarks,
            CommitGraph,
            FilestoreConfig,
            RepoBlobstore,
            RepoDerivedData,
            RepoIdentity,
        )]
        repo: Repo,

        #[facet]
        commit_graph_writer: dyn CommitGraphWriter,
    }

    #[derive(Parser)]
    struct ArgsParser {
        #[clap(flatten)]
        args: VerifyHgFromAugmentedArgs,
    }

    #[mononoke::test]
    fn commits_to_check_are_required() {
        assert!(
            ArgsParser::try_parse_from(["verify-hg-from-augmented"]).is_err(),
            "neither --commit-id nor --bookmark should be rejected"
        );
        assert!(ArgsParser::try_parse_from(["verify-hg-from-augmented", "-B", "master"]).is_ok());
    }

    #[mononoke::fbinit_test]
    async fn fails_when_no_commit_is_checked(fb: FacebookInit) -> Result<()> {
        // Given: a commit whose Hg changeset is not derived, so it is skipped.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let cs_id = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("file", "content")
            .commit()
            .await?;
        let args =
            ArgsParser::try_parse_from(["verify-hg-from-augmented", "-i", &cs_id.to_string()])?
                .args;

        // When: checking it.
        let result =
            verify_hg_from_augmented(&ctx, &repo.repo, repo.repo_derived_data().manager(), args)
                .await;

        // Then: the command fails, as it verified nothing.
        let err = result.expect_err("checking no commit should fail");
        assert!(
            format!("{err:#}").contains("no commit was checked"),
            "got {err:#}"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn null_root_has_no_new_trees(fb: FacebookInit) -> Result<()> {
        // Given: the null root of an uploaded commit with no files, as a root
        // commit and as the child of another such commit.
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let null = HgManifestId::new(NULL_HASH);
        for parents in [vec![], vec![null]] {
            // When: listing the Hg trees its derivation must write.
            let trees = expected_new_trees(
                &ctx,
                &repo,
                CommitHgRoots {
                    root: null,
                    parents: parents.clone(),
                },
            )
            .await?;

            // Then: there is none, as derivation writes none for it.
            assert!(trees.is_empty(), "parents {parents:?}: {trees:?}");
        }
        Ok(())
    }
}
