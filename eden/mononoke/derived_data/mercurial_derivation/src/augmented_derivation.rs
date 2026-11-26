/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use acl_manifest::RootAclManifestId;
use anyhow::Context;
use anyhow::Result;
use blobstore::KeyedBlobstore;
use blobstore::LoadableError;
use blobstore::StoreLoadable;
use context::CoreContext;
use derived_data::prefetch_content_metadata;
use derived_data_manager::DerivationContext;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use manifest::Diff;
use manifest::ManifestOps;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgChangesetId;
use mercurial_types::HgManifestId;
use mercurial_types::HgNodeHash;
use mercurial_types::NULL_HASH;
use mercurial_types::blobs::MononokeHgBlobError;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::typed_hash::AclManifestId;
use restricted_paths_common::ManifestIdStoreWriteCallsite;
use restricted_paths_common::maybe_propagate_manifest_id_store_write_error;
use stats::prelude::*;
use thiserror::Error;
use tracing::warn;

use crate::access_labels;
use crate::derive_hg_augmented_manifest::compute_hg_node_id;
use crate::derive_hg_augmented_manifest::find_restricted_path_entries;
use crate::derive_hg_augmented_manifest::normalize_acl_root;
use crate::derive_hg_augmented_manifest::store_restricted_path_entries;
use crate::mapping::RootHgAugmentedManifestId;

define_stats! {
    prefix = "mononoke.derived_data.hg_augmented_manifests";
    // Keyed by repo and by `Count::name`.
    stored_root: dynamic_timeseries("{}.{}", (repo: String, counter: &'static str); Rate, Sum),
}

/// A commit's mapped Hg changeset and its root tree.
#[derive(Clone, Copy)]
struct MappedHgRoot {
    hg_cs_id: HgChangesetId,
    manifest_id: HgManifestId,
}

async fn lookup_mapped_hg_roots(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    csids: Vec<ChangesetId>,
) -> Result<HashMap<ChangesetId, MappedHgRoot>> {
    if csids.is_empty() {
        return Ok(HashMap::new());
    }

    let mappings = derivation_ctx
        .bonsai_hg_mapping()?
        .get(ctx, csids.into())
        .await?;
    let blobstore = Arc::clone(derivation_ctx.blobstore());

    stream::iter(mappings)
        .map(|entry| {
            let blobstore = Arc::clone(&blobstore);
            async move {
                let manifest_id = entry
                    .hg_cs_id
                    .load(ctx, &blobstore)
                    .await
                    .with_context(|| {
                        format!(
                            "failed loading mapped HgChangeset {} for {}",
                            entry.hg_cs_id, entry.bcs_id,
                        )
                    })?
                    .manifestid();
                let root = MappedHgRoot {
                    hg_cs_id: entry.hg_cs_id,
                    manifest_id,
                };
                Ok((entry.bcs_id, root))
            }
        })
        .buffer_unordered(100)
        .try_collect()
        .await
}

pub(crate) async fn lookup_mapped_root_hg_manifest_id(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    csid: ChangesetId,
) -> Result<Option<HgManifestId>> {
    Ok(lookup_mapped_hg_roots(ctx, derivation_ctx, vec![csid])
        .await?
        .remove(&csid)
        .map(|root| root.manifest_id))
}

/// Builds a commit's augmented manifest from its mapped Hg manifest, for
/// `mononoke_admin derived-data verify-aug-direct`. Its reads carry no access
/// label, so the Hg manifest read check still reports them.
pub(crate) async fn derive_from_mapped_hg_manifest(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: &BonsaiChangeset,
    hg_manifest_id: HgManifestId,
    aug_parents: Vec<HgAugmentedManifestId>,
) -> Result<HgAugmentedManifestId> {
    let acl_root_overlay = async {
        let acl_root = derivation_ctx
            .fetch_dependency::<RootAclManifestId>(ctx, bonsai.get_changeset_id())
            .await?;
        normalize_acl_root(&acl_root)
    };
    derive_from_hg_manifest(
        ctx,
        derivation_ctx,
        bonsai,
        hg_manifest_id,
        aug_parents,
        acl_root_overlay,
    )
    .await
}

async fn derive_from_hg_manifest(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: &BonsaiChangeset,
    hg_manifest_id: HgManifestId,
    aug_parents: Vec<HgAugmentedManifestId>,
    acl_root_overlay: impl Future<Output = Result<Option<AclManifestId>>>,
) -> Result<HgAugmentedManifestId> {
    let blobstore = derivation_ctx.blobstore();
    let content_ids = bonsai
        .file_changes()
        .filter_map(|(_path, change)| change.simplify().map(|change| change.content_id()))
        .collect::<HashSet<_>>();
    let (content_metadata, acl_root_overlay) = future::try_join(
        prefetch_content_metadata(ctx, blobstore, content_ids),
        acl_root_overlay,
    )
    .await?;

    crate::derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
        ctx,
        blobstore,
        hg_manifest_id,
        aug_parents,
        &content_metadata,
        &derivation_ctx.restricted_paths(),
        acl_root_overlay,
    )
    .await
}

/// Why a stored record that decodes is not the commit's tree.
#[derive(Debug, Error)]
enum Rejection {
    #[error("its entries and parents hash to {recomputed}")]
    HgId { recomputed: HgNodeHash },
    #[error("it points to ACL manifest {stored:?}, not the commit's {expected:?}")]
    Acl {
        stored: Option<AclManifestId>,
        expected: Option<AclManifestId>,
    },
}

impl Rejection {
    fn check(&self) -> Check {
        match self {
            Self::HgId { .. } => Check::HgId,
            Self::Acl { .. } => Check::Acl,
        }
    }
}

/// Which tree a stored root record is.
#[derive(Debug)]
enum StoredTree {
    /// Its entries and parents hash to the root tree id.
    Content,
    /// Tree upload stores a client's root under the client's id, which for
    /// mirror uploads and converted flat manifests is not the hash of its
    /// content. Such a record is correct, as the client's Hg manifest is: its
    /// entries and parents hash to its stored content id.
    ClientId,
    Rejected(Rejection),
    /// Not known, because the record did not decode, or one of its own shards
    /// is missing, does not decode or could not be read.
    Unchecked,
}

/// What stopped a stored record from being checked in full.
#[derive(Debug, Error)]
enum Problem {
    /// Records it adds over its first parent, records of that parent, or a
    /// shard of its own record are missing, as a failed tree upload can leave
    /// them. Building from Hg stores them. A missing shard of its own leaves
    /// its Hg id unchecked, so that root is repaired and stays alarmed: as
    /// unverified once repaired, as incomplete if the repair fails.
    #[error("records are missing: {0:#}")]
    MissingRecords(anyhow::Error),
    /// It, or a directory record below it or its first parent, does not
    /// decode or holds another tree's record. A shard that does not decode
    /// counts as `Unverified`.
    #[error("a record does not decode: {0:#}")]
    Corrupt(anyhow::Error),
    /// Checking failed for another reason, such as a storage error.
    #[error("checking failed: {0:#}")]
    Unverified(anyhow::Error),
}

impl Problem {
    fn from_error(err: anyhow::Error) -> Self {
        if is_missing(&err) {
            Self::MissingRecords(err)
        } else if is_corrupt(&err) {
            Self::Corrupt(err)
        } else {
            Self::Unverified(err)
        }
    }
}

/// What is stored under a mapped commit's root tree id.
#[derive(Debug)]
enum StoredRoot {
    Missing,
    /// Reading the root key failed, so whether a record is stored is unknown.
    ReadFailed(anyhow::Error),
    Found {
        tree: StoredTree,
        problem: Option<Problem>,
    },
}

/// What derivation does with what is stored.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// Map the stored record.
    Map,
    /// Build from Hg to store the records the stored root lacks, then map the
    /// stored record whatever the build gives.
    Repair,
    /// Build from Hg, as nothing is known to be stored.
    BuildFromHg,
}

/// Why a mapped record is counted in `stored_root_rejected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    HgId,
    Acl,
    Corrupt,
    Unverified,
    Incomplete,
}

/// The counters derivation adds to.
#[derive(Debug, PartialEq, Eq)]
enum Count {
    SuppliedId,
    Incomplete,
    Rejected(Check),
    CheckFailed,
}

impl Count {
    /// The counter's name, under
    /// `mononoke.derived_data.hg_augmented_manifests.<repo>`.
    fn name(&self) -> &'static str {
        match self {
            Self::SuppliedId => "stored_root_supplied_id",
            Self::Incomplete => "stored_root_incomplete",
            Self::CheckFailed => "stored_root_check_failed",
            Self::Rejected(Check::HgId) => "stored_root_rejected.hg_id",
            Self::Rejected(Check::Acl) => "stored_root_rejected.acl",
            Self::Rejected(Check::Corrupt) => "stored_root_rejected.corrupt",
            Self::Rejected(Check::Unverified) => "stored_root_rejected.unverified",
            Self::Rejected(Check::Incomplete) => "stored_root_rejected.incomplete",
        }
    }
}

impl StoredRoot {
    /// What derivation does with this result, and what it counts first.
    fn plan(&self) -> (Next, Vec<Count>) {
        let (tree, problem) = match self {
            Self::Missing => return (Next::BuildFromHg, vec![]),
            Self::ReadFailed(_) => return (Next::BuildFromHg, vec![Count::CheckFailed]),
            Self::Found { tree, problem } => (tree, problem),
        };
        let tree_count = match tree {
            StoredTree::Content | StoredTree::Unchecked => None,
            StoredTree::ClientId => problem.is_none().then_some(Count::SuppliedId),
            StoredTree::Rejected(rejection) => Some(Count::Rejected(rejection.check())),
        };
        // A rejected root is not repaired: the build would put its own root
        // under the key, which the caches keep while storage keeps this one.
        // An unchecked root with missing records is repaired, as it may be
        // the commit's own tree, and stays alarmed after the repair.
        let rejected = matches!(tree, StoredTree::Rejected(_));
        let checked = matches!(tree, StoredTree::Content | StoredTree::ClientId);
        let (next, problem_count) = match problem {
            None => (Next::Map, None),
            Some(Problem::MissingRecords(_)) if rejected => {
                (Next::Map, Some(Count::Rejected(Check::Incomplete)))
            }
            Some(Problem::MissingRecords(_)) => (Next::Repair, Some(Count::Incomplete)),
            Some(Problem::Corrupt(_)) => (Next::Map, Some(Count::Rejected(Check::Corrupt))),
            // Counted even when the repair succeeds: a shard that does not
            // decode is also unverified, and the repair cannot overwrite it.
            Some(Problem::Unverified(_)) if checked => {
                (Next::Repair, Some(Count::Rejected(Check::Unverified)))
            }
            Some(Problem::Unverified(_)) => (Next::Map, Some(Count::Rejected(Check::Unverified))),
        };
        (next, tree_count.into_iter().chain(problem_count).collect())
    }

    /// What derivation counts after a repair. A failed repair maps the record
    /// without the records it lacks. A record whose Hg id could not be checked
    /// is still alarmed after a repair, as nothing checks it again. An
    /// unverified record was counted before the repair.
    fn repair_counts(&self, repaired: bool) -> Vec<Count> {
        match self {
            Self::Found {
                problem: Some(Problem::Unverified(_)),
                ..
            } => vec![],
            _ if !repaired => vec![Count::Rejected(Check::Incomplete)],
            Self::Found {
                tree: StoredTree::Unchecked,
                ..
            } => vec![Count::Rejected(Check::Unverified)],
            _ => vec![],
        }
    }

    fn log(&self, csid: ChangesetId, root: HgAugmentedManifestId) {
        match self {
            Self::Found { tree, problem } => {
                if let StoredTree::Rejected(rejection) = tree {
                    warn!(
                        "Mapping {csid} to the record stored under root tree {root}, although {rejection}, because storage does not overwrite it"
                    );
                }
                if let Some(problem) = problem {
                    warn!(
                        "The record stored under root tree {root} of {csid} is mapped, but {problem}"
                    );
                }
            }
            Self::ReadFailed(err) => warn!(
                "Building the augmented manifest of {csid} from its Hg manifest, because reading root tree {root} failed: {err:#}"
            ),
            Self::Missing => {}
        }
    }
}

fn add_counts(repo: &str, counts: &[Count]) {
    for count in counts {
        STATS::stored_root.add_value(1, (repo.to_string(), count.name()));
    }
}

fn is_missing(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<LoadableError>(),
            Some(LoadableError::Missing(_))
        )
    })
}

/// Whether the error is from a manifest record that does not decode, or that
/// holds another tree's record.
fn is_corrupt(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<MononokeHgBlobError>(),
        Some(MononokeHgBlobError::ManifestDeserializeFailed(_))
    ) || matches!(
        err.downcast_ref::<LoadableError>(),
        Some(LoadableError::Error(inner)) if is_corrupt(inner)
    )
}

/// Checks the record stored under `root`: that it points to the commit's ACL
/// manifest, that it is the tree `root` names, by hashing its entries and
/// parents as Hg does, and that every directory record it adds over `p1`
/// exists.
async fn check_stored_root(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    root: HgAugmentedManifestId,
    p1: Option<HgAugmentedManifestId>,
    acl_root_overlay: Option<AclManifestId>,
) -> StoredRoot {
    let manifest = match root.load(ctx, blobstore).await {
        Ok(envelope) => envelope.augmented_manifest,
        Err(LoadableError::Missing(_)) => return StoredRoot::Missing,
        Err(LoadableError::Error(err)) if is_corrupt(&err) => {
            return StoredRoot::Found {
                tree: StoredTree::Unchecked,
                problem: Some(Problem::Corrupt(err)),
            };
        }
        Err(LoadableError::Error(err)) => return StoredRoot::ReadFailed(err),
    };

    let computed_node_id = manifest.computed_node_id;
    let tree = if manifest.acl_manifest_directory_id != acl_root_overlay {
        StoredTree::Rejected(Rejection::Acl {
            stored: manifest.acl_manifest_directory_id,
            expected: acl_root_overlay,
        })
    } else if root.into_nodehash() == NULL_HASH {
        // An uploaded commit with no files has the null root, Hg's id for the
        // empty tree, which is not the hash of its entries. Derivation stores
        // that record from the synthetic null manifest.
        StoredTree::Content
    } else {
        let hg_parents = manifest.hg_parents();
        match compute_hg_node_id(manifest.subentries, ctx, blobstore, &hg_parents).await {
            Ok(id) if id == root.into_nodehash() => StoredTree::Content,
            Ok(id) if id == computed_node_id => StoredTree::ClientId,
            Ok(recomputed) => StoredTree::Rejected(Rejection::HgId { recomputed }),
            Err(err) => {
                return StoredRoot::Found {
                    tree: StoredTree::Unchecked,
                    problem: Some(Problem::from_error(err)),
                };
            }
        }
    };

    // Loading every directory that is new over the first parent proves that
    // its record exists. Directories the first parent shares were stored with
    // the parent.
    let store = Arc::clone(blobstore);
    let new_directories = match p1 {
        Some(p1) => p1.filtered_diff(
            ctx.clone(),
            store.clone(),
            root,
            store,
            |_| None::<()>,
            |diff| !matches!(diff, Diff::Removed(..)),
            HashMap::new(),
        ),
        None => root
            .list_all_entries(ctx.clone(), store)
            .map_ok(|_| ())
            .boxed(),
    };
    let problem = new_directories
        .try_for_each(|()| future::ok(()))
        .await
        .err()
        .map(Problem::from_error);
    StoredRoot::Found { tree, problem }
}

/// A commit with a Bonsai-to-Hg mapping must get the tree its Hg changeset
/// names, because an uploaded Hg changeset can differ from the one Mononoke
/// would derive, and the root a commit maps to is permanent. Whenever a
/// record is stored under that tree's id, the commit maps to it, even one that
/// fails a check: storage never overwrites the key, and failing derivation
/// would stall the warm bookmark cache and the derived data pipeline for the
/// whole repo. Such records are counted in `stored_root_rejected` and logged,
/// to be repaired by hand. Derivation builds from Hg to repair directory
/// records a root lacks, or when no record can be read. That build also puts
/// the root, which the cache layers keep even though storage keeps the old
/// record, so an alarmed record that derivation repaired, like any record
/// repaired by hand, needs its cache entries invalidated.
async fn derive_from_mapped_hg_root(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: &BonsaiChangeset,
    mapped: MappedHgRoot,
    aug_parents: Vec<HgAugmentedManifestId>,
) -> Result<HgAugmentedManifestId> {
    let csid = bonsai.get_changeset_id();
    let repo = derivation_ctx.repo_name();
    let blobstore = derivation_ctx.blobstore();
    let acl_root = derivation_ctx
        .fetch_dependency::<RootAclManifestId>(ctx, csid)
        .await?;
    let acl_root_overlay = normalize_acl_root(&acl_root)?;
    let root = HgAugmentedManifestId::from(mapped.manifest_id);
    // Derivation records the restricted roots of the envelopes it stores, and
    // a stored root's were stored outside derivation. Finding them is best
    // effort, because a mapped record may not reach them. A failed write is
    // returned when its JustKnob says so, as for the other writers.
    let restricted_paths = derivation_ctx.restricted_paths();
    let track_restricted_paths = || async {
        let entries = match find_restricted_path_entries(
            ctx,
            &restricted_paths,
            root,
            Arc::clone(blobstore),
        )
        .await
        {
            Ok(entries) => entries,
            Err(err) => {
                warn!(
                    "Could not find the restricted directories of root tree {root} of {csid}: {err:#}"
                );
                // A missing or undecodable record is the stored root's, and
                // failing on it would stall the repo.
                return match Problem::from_error(err) {
                    Problem::Unverified(err) => maybe_propagate_manifest_id_store_write_error(
                        err,
                        ManifestIdStoreWriteCallsite::TrackAllRestrictedPaths,
                    ),
                    Problem::MissingRecords(_) | Problem::Corrupt(_) => Ok(()),
                };
            }
        };
        store_restricted_path_entries(ctx, &restricted_paths, mapped.hg_cs_id, &entries).await
    };

    let stored = check_stored_root(
        ctx,
        blobstore,
        root,
        aug_parents.first().copied(),
        acl_root_overlay,
    )
    .await;
    let (next, counts) = stored.plan();
    add_counts(repo, &counts);
    stored.log(csid, root);

    let hg_ctx = ctx.with_access_label(access_labels::AUGMENTED_DERIVATION_FROM_HG);
    let derive_from_hg = || {
        derive_from_hg_manifest(
            &hg_ctx,
            derivation_ctx,
            bonsai,
            mapped.manifest_id,
            aug_parents,
            future::ok(acl_root_overlay),
        )
    };
    match next {
        Next::Map => {}
        Next::BuildFromHg => {
            return derive_from_hg().await.with_context(|| match &stored {
                StoredRoot::ReadFailed(err) => format!(
                    "cannot derive the augmented manifest of {csid}: reading root tree {root} failed ({err:#}), and building from Hg failed"
                ),
                _ => format!(
                    "cannot derive the augmented manifest of {csid}: no augmented manifest is stored under root tree {root}, and building from Hg failed"
                ),
            });
        }
        Next::Repair => {
            let repaired = derive_from_hg().await;
            add_counts(repo, &stored.repair_counts(repaired.is_ok()));
            if let Err(err) = &repaired {
                warn!(
                    "Mapping {csid} to the record stored under root tree {root} without the records it lacks, because building from Hg failed: {err:#}"
                );
            }
        }
    }
    track_restricted_paths().await?;
    Ok(root)
}

async fn derive_direct(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: &BonsaiChangeset,
    aug_parents: Vec<HgAugmentedManifestId>,
) -> Result<HgAugmentedManifestId> {
    let blobstore = derivation_ctx.blobstore();
    let csid = bonsai.get_changeset_id();

    let acl_root = derivation_ctx
        .fetch_dependency::<RootAclManifestId>(ctx, csid)
        .await?;

    let acl_root_overlay = crate::derive_hg_augmented_manifest::normalize_acl_root(&acl_root)?;

    crate::derive_hg_augmented_manifest::derive_augmented_manifest_from_bonsai_changeset(
        ctx,
        blobstore,
        bonsai,
        aug_parents,
        &derivation_ctx.restricted_paths(),
        acl_root_overlay,
    )
    .await
}

pub(crate) async fn derive_single_root(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsai: BonsaiChangeset,
    parents: Vec<RootHgAugmentedManifestId>,
) -> Result<RootHgAugmentedManifestId> {
    let csid = bonsai.get_changeset_id();
    let aug_parents = parents
        .iter()
        .map(RootHgAugmentedManifestId::hg_augmented_manifest_id)
        .collect();
    let mapped = lookup_mapped_hg_roots(ctx, derivation_ctx, vec![csid])
        .await?
        .remove(&csid);
    let root = if let Some(mapped) = mapped {
        derive_from_mapped_hg_root(ctx, derivation_ctx, &bonsai, mapped, aug_parents).await?
    } else {
        derive_direct(ctx, derivation_ctx, &bonsai, aug_parents).await?
    };
    Ok(RootHgAugmentedManifestId::new(root))
}

pub(crate) async fn derive_batch_roots(
    ctx: &CoreContext,
    derivation_ctx: &DerivationContext,
    bonsais: Vec<BonsaiChangeset>,
) -> Result<HashMap<ChangesetId, RootHgAugmentedManifestId>> {
    let mut res: HashMap<ChangesetId, RootHgAugmentedManifestId> = HashMap::new();
    let mapped_hg_roots = lookup_mapped_hg_roots(
        ctx,
        derivation_ctx,
        bonsais
            .iter()
            .map(BonsaiChangeset::get_changeset_id)
            .collect(),
    )
    .await?;
    for bonsai in &bonsais {
        let csid = bonsai.get_changeset_id();
        let aug_parents: Vec<_> = derivation_ctx
            .fetch_unknown_parents::<RootHgAugmentedManifestId>(ctx, Some(&res), bonsai)
            .await?
            .iter()
            .map(RootHgAugmentedManifestId::hg_augmented_manifest_id)
            .collect();
        let root = if let Some(mapped) = mapped_hg_roots.get(&csid).copied() {
            derive_from_mapped_hg_root(ctx, derivation_ctx, bonsai, mapped, aug_parents).await?
        } else {
            derive_direct(ctx, derivation_ctx, bonsai, aug_parents).await?
        };
        res.insert(csid, RootHgAugmentedManifestId::new(root));
    }
    Ok(res)
}

#[cfg(test)]
mod test {
    use anyhow::anyhow;
    use blobstore::BlobstoreBytes;
    use blobstore::Storable;
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bonsai_hg_mapping::BonsaiHgMappingRef;
    use bookmarks::Bookmarks;
    use bytes::Bytes;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use manifest::Entry;
    use mercurial_types::sharded_augmented_manifest::ShardedHgAugmentedManifest;
    use mononoke_macros::mononoke;
    use mononoke_types::MPath;
    use mononoke_types::hash::Blake2;
    use repo_blobstore::RepoBlobstore;
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

    /// A child commit's augmented root, stored under its root tree id the way
    /// tree upload stores it, and its parent's.
    struct StoredChildRoot {
        ctx: CoreContext,
        blobstore: Arc<dyn KeyedBlobstore>,
        parent_root: HgAugmentedManifestId,
        root: HgAugmentedManifestId,
    }

    impl StoredChildRoot {
        async fn new(fb: FacebookInit) -> Result<Self> {
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
            fixtures::store_hg_changesets_as_uploaded(&ctx, &repo, vec![parent, child]).await?;
            let manager = repo.repo_derived_data().manager();
            manager
                .derive_exactly_batch::<RootAclManifestId>(&ctx, vec![parent, child], None)
                .await?;
            manager
                .derive_exactly_batch::<RootHgAugmentedManifestId>(&ctx, vec![parent], None)
                .await?;
            let parent_root = manager
                .fetch_derived::<RootHgAugmentedManifestId>(&ctx, parent, None)
                .await?
                .context("missing parent augmented manifest")?
                .hg_augmented_manifest_id();
            let derivation_ctx = manager.derivation_context(None);
            let blobstore = Arc::clone(derivation_ctx.blobstore());
            let hg_root = repo
                .bonsai_hg_mapping()
                .get_hg_from_bonsai(&ctx, child)
                .await?
                .context("missing child Hg changeset")?
                .load(&ctx, &blobstore)
                .await?
                .manifestid();
            let root = crate::derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
                &ctx,
                &blobstore,
                hg_root,
                vec![parent_root],
                &Default::default(),
                &derivation_ctx.restricted_paths(),
                None,
            )
            .await?;
            assert_eq!(root, HgAugmentedManifestId::from(hg_root));
            Ok(Self {
                ctx,
                blobstore,
                parent_root,
                root,
            })
        }

        async fn check(&self, acl: Option<AclManifestId>) -> StoredRoot {
            check_stored_root(
                &self.ctx,
                &self.blobstore,
                self.root,
                Some(self.parent_root),
                acl,
            )
            .await
        }

        /// Replace the stored root record with a changed copy under the same
        /// key. Puts do not overwrite, so the record is removed first.
        async fn tamper(&self, tamper: impl FnOnce(&mut ShardedHgAugmentedManifest)) -> Result<()> {
            let mut envelope = self.root.load(&self.ctx, &self.blobstore).await?;
            tamper(&mut envelope.augmented_manifest);
            self.blobstore
                .unlink(&self.ctx, &self.root.blobstore_key())
                .await?;
            let key = envelope.store(&self.ctx, &self.blobstore).await?;
            assert_eq!(key, self.root, "the changed record keeps its key");
            Ok(())
        }

        async fn remove(&self, key: String) -> Result<()> {
            self.blobstore.unlink(&self.ctx, &key).await
        }

        async fn bytes(&self, key: String) -> Result<Bytes> {
            Ok(self
                .blobstore
                .get(&self.ctx, &key)
                .await?
                .with_context(|| format!("missing {key}"))?
                .into_raw_bytes())
        }

        /// Replace the blob under `key`. Puts do not overwrite, so it is
        /// removed first.
        async fn replace(&self, key: String, bytes: Bytes) -> Result<()> {
            self.remove(key.clone()).await?;
            self.blobstore
                .put(&self.ctx, key, BlobstoreBytes::from_bytes(bytes))
                .await
        }

        /// The parent's `dir`, which the child changes.
        async fn parent_dir(&self) -> Result<HgAugmentedManifestId> {
            self.parent_root
                .find_entry(
                    self.ctx.clone(),
                    Arc::clone(&self.blobstore),
                    MPath::new("dir")?,
                )
                .await?
                .and_then(Entry::into_tree)
                .context("the parent's root has dir")
        }

        async fn changed_dir(&self) -> Result<HgAugmentedManifestId> {
            self.root
                .find_entry(
                    self.ctx.clone(),
                    Arc::clone(&self.blobstore),
                    MPath::new("dir")?,
                )
                .await?
                .and_then(Entry::into_tree)
                .context("the child's root has dir")
        }
    }

    fn other_acl() -> AclManifestId {
        AclManifestId::new(Blake2::from_byte_array([1; 32]))
    }

    fn summary(stored: &StoredRoot) -> String {
        match stored {
            StoredRoot::Missing => "missing".to_string(),
            StoredRoot::ReadFailed(_) => "read failed".to_string(),
            StoredRoot::Found { tree, problem } => {
                let tree = match tree {
                    StoredTree::Content => "content".to_string(),
                    StoredTree::ClientId => "client id".to_string(),
                    StoredTree::Rejected(rejection) => {
                        Count::Rejected(rejection.check()).name().to_string()
                    }
                    StoredTree::Unchecked => "unchecked".to_string(),
                };
                let problem = match problem {
                    None => "",
                    Some(Problem::MissingRecords(_)) => ", missing records",
                    Some(Problem::Corrupt(_)) => ", corrupt",
                    Some(Problem::Unverified(_)) => ", unverified",
                };
                format!("{tree}{problem}")
            }
        }
    }

    #[mononoke::fbinit_test]
    async fn test_check_stored_root(fb: FacebookInit) -> Result<()> {
        let stored = StoredChildRoot::new(fb).await?;
        assert_eq!(summary(&stored.check(None).await), "content");
        assert_eq!(
            summary(&stored.check(Some(other_acl())).await),
            "stored_root_rejected.acl"
        );

        // Without p1, the record's entries hash to another id than its key.
        stored.tamper(|manifest| manifest.p1 = None).await?;
        let content_id = match stored.check(None).await {
            StoredRoot::Found {
                tree: StoredTree::Rejected(Rejection::HgId { recomputed }),
                problem: None,
            } => recomputed,
            result => panic!("a root hashing to another id is rejected, got {result:?}"),
        };

        // The same record, with that id as its stored content id, is a record
        // stored under the client's root id.
        stored
            .tamper(|manifest| manifest.computed_node_id = content_id)
            .await?;
        assert_eq!(summary(&stored.check(None).await), "client id");

        // It is still checked for missing directory records.
        stored
            .remove(stored.changed_dir().await?.blobstore_key())
            .await?;
        assert_eq!(
            summary(&stored.check(None).await),
            "client id, missing records"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_check_stored_null_root(fb: FacebookInit) -> Result<()> {
        // Given: the record derivation stores for the null root, which an
        // uploaded commit with no files has.
        let ctx = CoreContext::test_mock(fb);
        let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
        let blobstore = Arc::clone(derivation_ctx.blobstore());
        let root = crate::derive_hg_augmented_manifest::derive_from_hg_manifest_and_parents(
            &ctx,
            &blobstore,
            HgManifestId::new(NULL_HASH),
            vec![],
            &Default::default(),
            &derivation_ctx.restricted_paths(),
            None,
        )
        .await?;
        assert_eq!(root, HgAugmentedManifestId::new(NULL_HASH));

        // When: checking it as a mapped commit's stored root.
        let stored = check_stored_root(&ctx, &blobstore, root, None, None).await;

        // Then: it is the tree the null id names, so it is mapped with nothing
        // counted.
        assert_eq!(summary(&stored), "content");
        assert_eq!(stored.plan(), (Next::Map, vec![]));
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_check_stored_root_missing_records(fb: FacebookInit) -> Result<()> {
        // A record the root adds is missing.
        let stored = StoredChildRoot::new(fb).await?;
        stored
            .remove(stored.changed_dir().await?.blobstore_key())
            .await?;
        assert_eq!(
            summary(&stored.check(None).await),
            "content, missing records"
        );
        assert_eq!(
            summary(&stored.check(Some(other_acl())).await),
            "stored_root_rejected.acl, missing records"
        );
        stored.remove(stored.root.blobstore_key()).await?;
        assert_eq!(summary(&stored.check(None).await), "missing");

        // A record of the first parent is missing.
        let stored = StoredChildRoot::new(fb).await?;
        stored
            .remove(stored.parent_dir().await?.blobstore_key())
            .await?;
        assert_eq!(
            summary(&stored.check(None).await),
            "content, missing records"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_check_stored_root_corrupt(fb: FacebookInit) -> Result<()> {
        let stored = StoredChildRoot::new(fb).await?;
        let key = stored.root.blobstore_key();

        // A root that does not decode.
        stored
            .replace(key.clone(), Bytes::from_static(b"garbage"))
            .await?;
        assert_eq!(summary(&stored.check(None).await), "unchecked, corrupt");

        // Another tree's record under the root's key.
        let parent_bytes = stored.bytes(stored.parent_root.blobstore_key()).await?;
        stored.replace(key, parent_bytes).await?;
        assert_eq!(summary(&stored.check(None).await), "unchecked, corrupt");

        // A directory record below the root that does not decode.
        let stored = StoredChildRoot::new(fb).await?;
        stored
            .replace(
                stored.changed_dir().await?.blobstore_key(),
                Bytes::from_static(b"garbage"),
            )
            .await?;
        assert_eq!(summary(&stored.check(None).await), "content, corrupt");
        Ok(())
    }

    #[mononoke::test]
    fn test_plan() {
        let found = |tree, problem| StoredRoot::Found { tree, problem };
        let missing = || Some(Problem::MissingRecords(anyhow!("missing")));
        let unverified = || Some(Problem::Unverified(anyhow!("failed")));
        let acl = || {
            StoredTree::Rejected(Rejection::Acl {
                stored: None,
                expected: None,
            })
        };
        let names =
            |counts: Vec<Count>| -> Vec<&'static str> { counts.iter().map(Count::name).collect() };
        let cases = [
            (StoredRoot::Missing, Next::BuildFromHg, vec![]),
            (
                StoredRoot::ReadFailed(anyhow!("failed")),
                Next::BuildFromHg,
                vec!["stored_root_check_failed"],
            ),
            (found(StoredTree::Content, None), Next::Map, vec![]),
            (
                found(StoredTree::ClientId, None),
                Next::Map,
                vec!["stored_root_supplied_id"],
            ),
            (
                found(StoredTree::Content, missing()),
                Next::Repair,
                vec!["stored_root_incomplete"],
            ),
            (
                found(StoredTree::ClientId, missing()),
                Next::Repair,
                vec!["stored_root_incomplete"],
            ),
            (
                found(StoredTree::Unchecked, missing()),
                Next::Repair,
                vec!["stored_root_incomplete"],
            ),
            (
                found(acl(), None),
                Next::Map,
                vec!["stored_root_rejected.acl"],
            ),
            (
                found(acl(), missing()),
                Next::Map,
                vec![
                    "stored_root_rejected.acl",
                    "stored_root_rejected.incomplete",
                ],
            ),
            (
                found(
                    StoredTree::Rejected(Rejection::HgId {
                        recomputed: HgNodeHash::new(
                            mononoke_types::sha1_hash::Sha1::from_byte_array([1; 20]),
                        ),
                    }),
                    None,
                ),
                Next::Map,
                vec!["stored_root_rejected.hg_id"],
            ),
            (
                found(
                    StoredTree::Unchecked,
                    Some(Problem::Corrupt(anyhow!("bad"))),
                ),
                Next::Map,
                vec!["stored_root_rejected.corrupt"],
            ),
            (
                found(StoredTree::Content, Some(Problem::Corrupt(anyhow!("bad")))),
                Next::Map,
                vec!["stored_root_rejected.corrupt"],
            ),
            (
                found(StoredTree::Content, unverified()),
                Next::Repair,
                vec!["stored_root_rejected.unverified"],
            ),
            (
                found(StoredTree::ClientId, unverified()),
                Next::Repair,
                vec!["stored_root_rejected.unverified"],
            ),
            (
                found(StoredTree::Unchecked, unverified()),
                Next::Map,
                vec!["stored_root_rejected.unverified"],
            ),
            (
                found(acl(), unverified()),
                Next::Map,
                vec![
                    "stored_root_rejected.acl",
                    "stored_root_rejected.unverified",
                ],
            ),
        ];
        for (stored, next, counts) in cases {
            let (actual_next, actual_counts) = stored.plan();
            assert_eq!(
                (actual_next, names(actual_counts)),
                (next, counts),
                "{stored:?}"
            );
        }

        // After a repair.
        let repair_cases = [
            (found(StoredTree::Content, missing()), true, vec![]),
            (
                found(StoredTree::Content, missing()),
                false,
                vec!["stored_root_rejected.incomplete"],
            ),
            (
                found(StoredTree::Unchecked, missing()),
                true,
                vec!["stored_root_rejected.unverified"],
            ),
            (
                found(StoredTree::Unchecked, missing()),
                false,
                vec!["stored_root_rejected.incomplete"],
            ),
            (found(StoredTree::Content, unverified()), true, vec![]),
            (found(StoredTree::Content, unverified()), false, vec![]),
        ];
        for (stored, repaired, counts) in repair_cases {
            assert_eq!(
                names(stored.repair_counts(repaired)),
                counts,
                "{stored:?}, repaired {repaired}"
            );
        }
    }
}
