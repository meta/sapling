/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use acl_manifest::RootAclManifestId;
use anyhow::Context;
use anyhow::Error;
use anyhow::format_err;
use blobrepo_hg::BlobRepoHg;
use blobrepo_hg::ChangesetHandle;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::LoadableError;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bookmarks::BookmarkKey;
use bookmarks::Freshness;
use bytes::Bytes;
use cacheblob::MemWritesKeyedBlobstore;
use commit_graph::CommitGraphRef;
use context::CoreContext;
use context::PerfCounterType;
use dag_types::Location;
use derived_data_manager::BonsaiDerivable;
use edenapi_types::AnyId;
use edenapi_types::UploadToken;
use ephemeral_blobstore::Bubble;
use ephemeral_blobstore::BubbleId;
use ephemeral_blobstore::RepoEphemeralStore;
use ephemeral_blobstore::StorageLocation;
use ephemeral_blobstore::bubble_hg_mapping_enabled;
use filestore::FetchKey;
use filestore::FilestoreConfigRef;
use filestore::StoreRequest;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use futures_util::try_join;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_derivation::upload_augmented_manifest::UploadTreeAugmented;
use mercurial_derivation::upload_augmented_manifest::build_augmented_manifests_for_uploaded_trees;
use mercurial_derivation::upload_augmented_manifest::store_uploaded_tree_envelopes;
use mercurial_mutation::HgMutationEntry;
use mercurial_mutation::HgMutationStoreRef;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileEnvelopeMut;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestEnvelope;
use mercurial_types::HgManifestId;
use mercurial_types::HgNodeHash;
use mercurial_types::blobs::HgBlobChangeset;
use mercurial_types::blobs::RevlogChangeset;
use mercurial_types::blobs::UploadHgNodeHash;
use mercurial_types::blobs::UploadHgTreeEntry;
use mercurial_types::blobs::hg_manifests_read_from_augmented;
use mercurial_types::blobs::store_manifest_envelope;
use metaconfig_types::RepoConfig;
use mononoke_api::MononokeRepo;
use mononoke_api::errors::MononokeError;
use mononoke_api::repo::RepoContext;
use mononoke_types::BonsaiChangeset;
use mononoke_types::ChangesetId;
use mononoke_types::ContentId;
use mononoke_types::ContentMetadataV2;
use mononoke_types::DateTime;
use mononoke_types::RepoPath;
use mononoke_types::hash::GitSha1;
use phases::PhasesRef;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_client::find_new_draft_commits_and_derive_filenodes_for_public_roots;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use repo_update_logger::CommitInfo;
use repo_update_logger::log_new_commits;
use restricted_paths::RestrictedPathsArc;
use tracing::debug;
use unbundle::ShallowSubtreeCopyRejected;
use unbundle::upload_changeset;

use super::HgFileContext;
use super::HgTreeContext;

/// Lets the envelopes built at tree upload reach the blobstore.
const STORE_AUGMENTED_MANIFESTS_AT_TREE_UPLOAD: &str =
    "scm/mononoke:store_augmented_manifests_at_tree_upload";

/// Counts how many uploaded commits already have a stored root augmented
/// manifest, without acting on it.
const CHECK_UPLOADED_ROOT_HG_AUGMENTED_MANIFEST: &str =
    "scm/mononoke:check_uploaded_root_hg_augmented_manifest";

/// Maps uploaded commits whose root augmented manifest is stored to that root
/// instead of deriving them. Implies the check above.
const MAP_UPLOADED_ROOT_HG_AUGMENTED_MANIFEST: &str =
    "scm/mononoke:map_uploaded_root_hg_augmented_manifest";

/// Makes tree lookup require the augmented manifest as well as the hg
/// manifest.
const REQUIRE_AUGMENTED_MANIFEST_FOR_TREE_LOOKUP: &str =
    "scm/mononoke:require_augmented_manifest_for_tree_lookup";

#[derive(Clone)]
pub struct HgRepoContext<R> {
    repo_ctx: RepoContext<R>,
}

/// A commit that replaces other commits, as recorded when they are amended,
/// rebased or folded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitMutation {
    /// The new commit.
    pub successor: ChangesetId,
    /// The commits it replaces.
    pub predecessors: Vec<ChangesetId>,
    /// The operation, such as "amend", "rebase" or "fold".
    pub op: String,
    /// The user who performed the operation.
    pub user: String,
}

/// Where `get_graph_mapping` takes the draft flag of each commit from.
enum DraftClassification {
    /// The draft commits found by the getbundle walk down from the heads.
    Walked(HashSet<HgChangesetId>),
    /// The public commits among the commits being returned, from the phases.
    Phases(HashSet<ChangesetId>),
}

impl DraftClassification {
    fn is_draft(&self, cs_id: ChangesetId, hg_id: HgChangesetId) -> bool {
        match self {
            DraftClassification::Walked(draft_commits) => draft_commits.contains(&hg_id),
            DraftClassification::Phases(public_commits) => !public_commits.contains(&cs_id),
        }
    }
}

impl<R: MononokeRepo> HgRepoContext<R> {
    pub(crate) fn new(repo_ctx: RepoContext<R>) -> Self {
        Self { repo_ctx }
    }

    /// The `CoreContext` for this query.
    pub fn ctx(&self) -> &CoreContext {
        self.repo_ctx.ctx()
    }

    /// The `RepoContext` for this query.
    pub fn repo_ctx(&self) -> &RepoContext<R> {
        &self.repo_ctx
    }

    /// The underlying Mononoke `Repo` backing this `HgRepoContext`.
    pub fn repo(&self) -> &R {
        self.repo_ctx().repo()
    }

    /// The configuration for the repository.
    pub(crate) fn config(&self) -> &RepoConfig {
        self.repo_ctx.config()
    }

    /// Create bubble and return its id
    pub async fn create_bubble(
        &self,
        custom_duration: Option<Duration>,
        labels: Vec<String>,
    ) -> Result<Bubble, MononokeError> {
        Ok(self
            .repo_ctx()
            .repo_ephemeral_store_arc()
            .create_bubble(self.ctx(), custom_duration, labels)
            .await?)
    }

    pub fn ephemeral_store(&self) -> Arc<RepoEphemeralStore> {
        self.repo_ctx().repo_ephemeral_store_arc()
    }

    /// Load bubble from id
    pub async fn open_bubble(&self, bubble_id: BubbleId) -> Result<Bubble, MononokeError> {
        self.repo_ctx.open_bubble(bubble_id).await
    }

    /// Get blobstore. If bubble id is present, this is the ephemeral blobstore
    pub async fn bubble_blobstore(
        &self,
        bubble_id: Option<BubbleId>,
    ) -> Result<RepoBlobstore, MononokeError> {
        let main_blobstore = self.repo().repo_blobstore().clone();
        Ok(match bubble_id {
            Some(id) => self
                .repo_ctx
                .open_bubble(id)
                .await?
                .wrap_repo_blobstore(main_blobstore),
            None => main_blobstore,
        })
    }

    /// Get changeset id from hg changeset id
    pub async fn get_bonsai_from_hg(
        &self,
        hgid: HgChangesetId,
    ) -> Result<Option<ChangesetId>, MononokeError> {
        Ok(self
            .repo()
            .bonsai_hg_mapping()
            .get_bonsai_from_hg(self.ctx(), hgid)
            .await?)
    }

    /// Fetch file content size, fails if it doesn't exist.
    pub async fn fetch_file_content_size(
        &self,
        content_id: ContentId,
        bubble_id: Option<BubbleId>,
    ) -> Result<u64, MononokeError> {
        Ok(filestore::get_metadata(
            &self.bubble_blobstore(bubble_id).await?,
            self.ctx(),
            &FetchKey::Canonical(content_id),
        )
        .await?
        .ok_or_else(|| {
            MononokeError::InvalidRequest(format!(
                "failed to fetch or rebuild metadata for ContentId('{content_id}'), file content must be prior uploaded"
            ))
        })?
        .total_size)
    }

    async fn is_key_present_in_blobstore(
        &self,
        key: &str,
        bubble_id: Option<BubbleId>,
    ) -> Result<bool, MononokeError> {
        async move {
            self.bubble_blobstore(bubble_id)
                .await?
                .is_present(self.ctx(), key)
                .await
                .map(|is_present| {
                    // if we can't resolve the presence (some blobstores failed, some returned None)
                    // we can re-upload the blob
                    is_present.assume_not_found_if_unsure()
                })
        }
        .await
        .map_err(MononokeError::from)
    }

    /// Look up in blobstore by `ContentId`
    pub async fn is_file_present(
        &self,
        hash: impl Into<FetchKey>,
        bubble_id: Option<BubbleId>,
    ) -> Result<bool, MononokeError> {
        self.is_key_present_in_blobstore(&hash.into().blobstore_key(), bubble_id)
            .await
    }

    /// Convert given hash to canonical ContentId
    pub async fn convert_file_to_content_id<H: Into<FetchKey> + Copy + std::fmt::Debug>(
        &self,
        hash: H,
        bubble_id: Option<BubbleId>,
    ) -> Result<Option<ContentId>, MononokeError> {
        match hash
            .into()
            .load(self.ctx(), &self.bubble_blobstore(bubble_id).await?)
            .await
        {
            Ok(cid) => Ok(Some(cid)),
            Err(LoadableError::Missing(_)) => Ok(None),
            Err(LoadableError::Error(err)) => {
                Err(err).with_context(|| format_err!("While fetching ContentId for {hash:?}"))?
            }
        }
    }

    /// Store file into blobstore. When `bypass_redaction` is true, redaction
    /// remains logged but is not enforced for the filestore operation.
    pub async fn store_file(
        &self,
        key: impl Into<FetchKey>,
        size: u64,
        data: impl Stream<Item = Result<Bytes, Error>> + Send,
        bubble_id: Option<BubbleId>,
        bypass_redaction: bool,
    ) -> Result<ContentMetadataV2, MononokeError> {
        let blobstore = self.bubble_blobstore(bubble_id).await?;
        let blobstore = if bypass_redaction {
            blobstore.with_log_only_redaction()
        } else {
            blobstore
        };
        filestore::store(
            &blobstore,
            *self.repo().filestore_config(),
            self.ctx(),
            &StoreRequest::with_fetch_key(size, key.into()),
            data,
        )
        .await
        .map_err(MononokeError::from)
    }

    /// Download file contents
    pub async fn download_file(
        &self,
        upload_token: UploadToken,
    ) -> Result<Option<impl Stream<Item = Result<Bytes, Error>> + 'static + use<R>>, MononokeError>
    {
        Ok(filestore::fetch(
            self.bubble_blobstore(upload_token.data.bubble_id.map(BubbleId::new))
                .await?,
            self.ctx(),
            &match upload_token.data.id {
                AnyId::AnyFileContentId(file_id) => file_id.into(),
                e => {
                    return Err(MononokeError::from(format_err!(
                        "Id is not of a file: {e:?}"
                    )));
                }
            },
        )
        .await?)
    }

    /// Test whether a Mercurial changeset exists.
    pub async fn hg_changeset_exists(
        &self,
        hg_changeset_id: HgChangesetId,
    ) -> Result<bool, MononokeError> {
        self.repo()
            .hg_changeset_exists(self.ctx().clone(), hg_changeset_id)
            .await
            .map_err(MononokeError::from)
    }

    /// Test whether a changeset exists in a particular storage location.
    pub async fn changeset_exists(
        &self,
        changeset_id: ChangesetId,
        storage_location: StorageLocation,
    ) -> Result<bool, MononokeError> {
        self.repo_ctx
            .changeset_exists(changeset_id, storage_location)
            .await
    }

    /// Look up in blobstore by `HgFileNodeId`
    pub async fn filenode_exists(&self, filenode_id: HgFileNodeId) -> Result<bool, MononokeError> {
        self.is_key_present_in_blobstore(&filenode_id.blobstore_key(), None)
            .await
    }

    /// Whether the tree is stored: its augmented manifest once client
    /// operations read augmented manifests, its Hg manifest otherwise.
    pub async fn tree_exists(&self, manifest_id: HgManifestId) -> Result<bool, MononokeError> {
        let key = if self.reads_augmented_manifests() {
            HgAugmentedManifestId::new(manifest_id.into_nodehash()).blobstore_key()
        } else {
            manifest_id.blobstore_key()
        };
        self.is_key_present_in_blobstore(&key, None).await
    }

    /// Whether lookup should tell the client that a tree is already uploaded.
    ///
    /// Lookup normally trusts the hg manifest alone, so a tree whose augmented
    /// manifest is missing is never resent and never gets one. With
    /// `scm/mononoke:require_augmented_manifest_for_tree_lookup` on, the tree
    /// counts only when both manifests are stored, so the client resends it
    /// and tree upload stores the missing envelope. Once client operations
    /// read augmented manifests, the augmented manifest alone decides.
    pub async fn is_tree_uploaded(&self, manifest_id: HgManifestId) -> Result<bool, MononokeError> {
        if self.reads_augmented_manifests() || !self.requires_augmented_manifest_for_tree_lookup() {
            return self.tree_exists(manifest_id).await;
        }
        let augmented_key = HgAugmentedManifestId::new(manifest_id.into_nodehash()).blobstore_key();
        let hg_key = manifest_id.blobstore_key();
        let (augmented_exists, hg_exists) = try_join!(
            self.is_key_present_in_blobstore(&augmented_key, None),
            self.is_key_present_in_blobstore(&hg_key, None),
        )?;
        // Only trees resent because of the augmented manifest, not new ones.
        if hg_exists && !augmented_exists {
            self.ctx()
                .perf_counters()
                .increment_counter(PerfCounterType::TreeLookupAugmentedMissing);
        }
        Ok(augmented_exists && hg_exists)
    }

    fn reads_augmented_manifests(&self) -> bool {
        hg_manifests_read_from_augmented(self.repo().repo_identity().name())
    }

    fn requires_augmented_manifest_for_tree_lookup(&self) -> bool {
        // A repo without augmented manifests would never see a tree as uploaded.
        justknobs::eval(
            REQUIRE_AUGMENTED_MANIFEST_FOR_TREE_LOOKUP,
            None,
            Some(self.repo().repo_identity().name()),
        ) && self
            .repo()
            .repo_derived_data()
            .config()
            .is_enabled(RootHgAugmentedManifestId::VARIANT)
    }

    /// Look up a file in the repo by `HgFileNodeId`.
    pub async fn file(
        &self,
        filenode_id: HgFileNodeId,
    ) -> Result<Option<HgFileContext<R>>, MononokeError> {
        HgFileContext::new_check_exists(self.clone(), filenode_id).await
    }

    /// Look up a tree in the repo by `HgManifestId`.
    pub async fn tree(
        &self,
        manifest_id: HgManifestId,
    ) -> Result<Option<HgTreeContext<R>>, MononokeError> {
        HgTreeContext::new_check_exists(self.clone(), manifest_id).await
    }

    /// Store HgFilenode into blobstore
    pub async fn store_hg_filenode(
        &self,
        filenode_id: HgFileNodeId,
        p1: Option<HgFileNodeId>,
        p2: Option<HgFileNodeId>,
        content_id: ContentId,
        content_size: u64,
        metadata: Bytes,
    ) -> Result<(), MononokeError> {
        let envelope = HgFileEnvelopeMut {
            node_id: filenode_id,
            p1,
            p2,
            content_id,
            content_size,
            metadata,
        };

        self.repo()
            .repo_blobstore()
            .put(
                self.ctx(),
                filenode_id.blobstore_key(),
                envelope.freeze().into_blob().into(),
            )
            .await
            .map_err(MononokeError::from)?;
        Ok(())
    }

    /// Store Tree into blobstore, returning the envelope it was stored as.
    ///
    /// Take the envelope from here rather than rebuilding it from the raw
    /// bytes: `upload_node_id` and `computed_node_id` legitimately differ for a
    /// mirror upload, and a second construction site loses that distinction.
    pub async fn store_tree(
        &self,
        upload_node_id: HgNodeHash,
        p1: Option<HgNodeHash>,
        p2: Option<HgNodeHash>,
        contents: Bytes,
        computed_node_id: Option<HgNodeHash>,
    ) -> Result<HgManifestEnvelope, MononokeError> {
        let envelope = self
            .verify_tree(upload_node_id, p1, p2, contents, computed_node_id)
            .await?;
        self.store_verified_tree(envelope.clone()).await?;
        Ok(envelope)
    }

    /// Check an uploaded tree against its node id and return the envelope
    /// `store_verified_tree` would store, writing nothing.
    pub async fn verify_tree(
        &self,
        upload_node_id: HgNodeHash,
        p1: Option<HgNodeHash>,
        p2: Option<HgNodeHash>,
        contents: Bytes,
        computed_node_id: Option<HgNodeHash>,
    ) -> Result<HgManifestEnvelope, MononokeError> {
        if computed_node_id.is_some() {
            self.repo_ctx
                .authorization_context()
                .require_mirror_upload_operations(self.ctx(), self.repo())
                .await?;
        }
        let entry = UploadHgTreeEntry {
            upload_node_id: UploadHgNodeHash::Checked(upload_node_id),
            contents,
            p1,
            p2,
            path: RepoPath::RootPath, // only used for logging
            computed_node_id,
        };
        let (envelope, _path) = entry.into_verified_envelope()?;
        Ok(envelope)
    }

    /// Store a tree envelope that `verify_tree` returned.
    pub async fn store_verified_tree(
        &self,
        envelope: HgManifestEnvelope,
    ) -> Result<(), MononokeError> {
        store_manifest_envelope(self.ctx(), self.repo().repo_blobstore(), envelope)
            .await
            .map_err(MononokeError::from)?;
        Ok(())
    }

    /// Whether `build_and_store_augmented_manifests_for_uploaded_trees` writes
    /// the envelopes it builds, rather than discarding them.
    pub fn stores_augmented_manifests_at_tree_upload(&self) -> bool {
        justknobs::eval(
            STORE_AUGMENTED_MANIFESTS_AT_TREE_UPLOAD,
            None,
            Some(self.repo().repo_identity().name()),
        )
    }

    /// Build the augmented manifest envelopes for a batch of already-stored
    /// trees and, when the store knob is on, store them so they exist before
    /// the changeset that references them is uploaded.
    ///
    /// A tree whose child is neither in the batch nor already derived is an
    /// error: the batch builds completely or not at all.
    ///
    /// Everything the build writes (large directory shards, ACL nodes) and the
    /// envelopes themselves go to an in-memory overlay above the repo
    /// blobstore. Until `scm/mononoke:store_augmented_manifests_at_tree_upload`
    /// is on for the repo the overlay dies with this call; once it is on it is
    /// written out in one concurrent pass.
    ///
    /// Once it is on the writes are permanent: the key is the hg manifest id
    /// alone and the put is if-absent, so a wrong envelope cannot be corrected
    /// later. Turning the knob off stops new writes, it does not repair old
    /// ones.
    pub async fn build_and_store_augmented_manifests_for_uploaded_trees(
        &self,
        trees: Vec<HgManifestEnvelope>,
    ) -> Result<Vec<UploadTreeAugmented>, Error> {
        let store = self.stores_augmented_manifests_at_tree_upload();
        // Above the repo blobstore, so a buffered write is a plain insert: the
        // if-absent check happens once, in the real store, when it is persisted.
        let overlay = Arc::new(MemWritesKeyedBlobstore::new(
            self.repo().repo_blobstore().clone(),
        ));
        let blobstore: Arc<dyn KeyedBlobstore> = overlay.clone();
        let restricted_paths = self.repo().restricted_paths_arc();
        let built = build_augmented_manifests_for_uploaded_trees(
            self.ctx(),
            &blobstore,
            restricted_paths.config_based(),
            trees,
        )
        .await?;
        if store {
            store_uploaded_tree_envelopes(self.ctx(), &blobstore, &built).await?;
            overlay.persist(self.ctx()).await?;
        }
        Ok(built)
    }

    /// Store HgChangeset. The function also generates bonsai changeset and stores all necessary mappings.
    pub async fn store_hg_changesets(
        &self,
        changesets: Vec<(HgChangesetId, RevlogChangeset, Option<BonsaiChangeset>)>,
        mutations: Vec<HgMutationEntry>,
    ) -> Result<Vec<Result<(HgBlobChangeset, BonsaiChangeset), MononokeError>>, MononokeError> {
        let mut uploaded_changesets: HashMap<HgChangesetId, ChangesetHandle> = HashMap::new();
        let filelogs = HashMap::new();
        let manifests = HashMap::new();
        for (node, revlog_cs, bonsai) in changesets {
            uploaded_changesets = upload_changeset(
                self.ctx().clone(),
                self.repo().clone(),
                self.ctx().scuba().clone(),
                node,
                &revlog_cs,
                uploaded_changesets,
                &filelogs,
                &manifests,
                bonsai,
            )
            .await
            .map_err(|e| match e.downcast_ref::<ShallowSubtreeCopyRejected>() {
                Some(rejected) => MononokeError::InvalidRequest(rejected.to_string()),
                None => MononokeError::from(e),
            })?;
        }
        let mut results = Vec::new();
        let mut hg_changesets = HashSet::new();
        let mut commits_to_log = Vec::new();
        for (hg_cs_id, handle) in uploaded_changesets {
            let result = match handle.get_completed_changeset().await {
                Ok((bonsai, hg_cs)) => {
                    hg_changesets.insert(hg_cs_id);
                    commits_to_log.push(CommitInfo::new(&bonsai, None));
                    Ok((hg_cs, bonsai))
                }
                Err(e) => Err(MononokeError::from(Error::from(e))),
            };
            results.push(result);
        }
        log_new_commits(self.ctx(), self.repo_ctx().repo(), None, commits_to_log).await;

        if !mutations.is_empty() {
            self.repo()
                .hg_mutation_store()
                .add_entries(self.ctx(), hg_changesets, mutations)
                .await
                .map_err(MononokeError::from)?;
        }

        Ok(results)
    }

    /// Best-effort derive the augmented manifest for each uploaded commit so the
    /// routed `trees` endpoint can serve it later without a serve-time derivation
    /// chokepoint. Gated by the per-repo creation-time JustKnob; concurrency is
    /// bounded because the batch is client-controlled (S493741).
    ///
    /// MUST run only AFTER the bonsai<->hg mapping is committed: with no stored
    /// mapping, `RootHgAugmentedManifestId` derives from Bonsai and produces
    /// Mononoke's own hg tree, not a caller-supplied divergent one (e.g.
    /// null/empty-tree mirror commits from `UploadIdenticalChangesets`).
    pub async fn ensure_uploaded_augmented_manifests_derived(
        &self,
        results: &[Result<(HgBlobChangeset, BonsaiChangeset), MononokeError>],
    ) {
        let uploaded = results.iter().filter_map(|result| result.as_ref().ok());
        let repo_name = self.repo().repo_identity().name();
        // Mapping cannot bring augmented manifests to a repo that has them off.
        let map_roots = justknobs::eval(
            MAP_UPLOADED_ROOT_HG_AUGMENTED_MANIFEST,
            None,
            Some(repo_name),
        ) && self
            .repo()
            .repo_derived_data()
            .config()
            .is_enabled(RootHgAugmentedManifestId::VARIANT);
        let in_bubble = self
            .repo()
            .repo_derived_data()
            .manager()
            .bubble_id()
            .is_some();
        let check_roots = (map_roots
            || justknobs::eval(
                CHECK_UPLOADED_ROOT_HG_AUGMENTED_MANIFEST,
                None,
                Some(repo_name),
            ))
            && (!in_bubble || bubble_hg_mapping_enabled(repo_name));
        // A commit mapped to its stored root is not derived.
        let stored: Vec<&BonsaiChangeset> = if check_roots {
            let checks: Vec<_> = uploaded
                .map(|(hg_cs, bcs)| async move {
                    let mapped = self.check_uploaded_root(hg_cs, bcs, map_roots).await;
                    (!mapped).then_some(bcs)
                })
                .collect();
            stream::iter(checks)
                .buffer_unordered(100)
                .filter_map(future::ready)
                .collect()
                .await
        } else {
            uploaded.map(|(_, bcs)| bcs).collect()
        };

        // Derive only the heads of the uploaded batch: deriving a head derives
        // all its ancestors, so this covers every uploaded commit without the
        // redundant per-commit fan-out. A commit is a head iff no other uploaded
        // commit lists it as a parent. Computed by set difference, not by order:
        // `results` is NOT topologically sorted (store_hg_changesets builds it
        // from a HashMap) and one request may carry multiple independent stacks.
        let parents_of_batch: HashSet<ChangesetId> =
            stored.iter().flat_map(|bcs| bcs.parents()).collect();
        let heads: Vec<ChangesetId> = stored
            .iter()
            .map(|bcs| bcs.get_changeset_id())
            .filter(|cs_id| !parents_of_batch.contains(cs_id))
            .collect();

        stream::iter(heads)
            .for_each_concurrent(20, |cs_id| {
                self.repo_ctx()
                    .ensure_hg_augmented_manifest_derived_at_creation(cs_id)
            })
            .await;
    }

    /// Counts what the uploaded commit's root tree has in the augmented
    /// manifest store and, with `map_roots`, maps the commit to a stored root.
    /// Returns whether it was mapped.
    async fn check_uploaded_root(
        &self,
        hg_cs: &HgBlobChangeset,
        bcs: &BonsaiChangeset,
        map_roots: bool,
    ) -> bool {
        let root = HgAugmentedManifestId::new(hg_cs.manifestid().into_nodehash());
        let lookup = async {
            let counter = match root.load(self.ctx(), self.repo().repo_blobstore()).await {
                Err(LoadableError::Missing(_)) => PerfCounterType::UploadedRootAugmentedMissing,
                Err(err) => return Err(Error::from(err)),
                // Stored, but derivation would emit this root under another id.
                Ok(envelope)
                    if envelope.augmented_manifest.hg_node_id
                        != envelope.augmented_manifest.computed_node_id =>
                {
                    PerfCounterType::UploadedRootAugmentedUnmappable
                }
                Ok(envelope) if map_roots => {
                    let derivation_ctx = self
                        .repo()
                        .repo_derived_data()
                        .manager()
                        .changeset_derivation_context(self.ctx(), bcs.get_changeset_id(), None)
                        .await?;
                    // The augmented manifest depends on the ACL manifest, so
                    // that is mapped first.
                    RootAclManifestId::from_augmented_root_pointer(
                        envelope.augmented_manifest.acl_manifest_directory_id,
                    )
                    .store_mapping(self.ctx(), &derivation_ctx, bcs.get_changeset_id())
                    .await?;
                    RootHgAugmentedManifestId::new(root)
                        .store_prebuilt_mapping(
                            self.ctx(),
                            &derivation_ctx,
                            hg_cs.get_changeset_id(),
                            bcs.get_changeset_id(),
                        )
                        .await?;
                    PerfCounterType::UploadedRootAugmentedMapped
                }
                Ok(_) => PerfCounterType::UploadedRootAugmentedStored,
            };
            Ok(counter)
        };
        let counter = lookup.await.unwrap_or_else(|err| {
            self.ctx().scuba().clone().log_with_msg(
                "Checking or mapping uploaded root augmented manifest failed",
                Some(format!(
                    "hg_cs_id: {}, err: {err:?}",
                    hg_cs.get_changeset_id()
                )),
            );
            PerfCounterType::UploadedRootAugmentedFailed
        });
        self.ctx().perf_counters().increment_counter(counter);
        matches!(counter, PerfCounterType::UploadedRootAugmentedMapped)
    }

    pub async fn fetch_mutations(
        &self,
        hg_changesets: HashSet<HgChangesetId>,
    ) -> Result<Vec<HgMutationEntry>, MononokeError> {
        Ok(self
            .repo()
            .hg_mutation_store()
            .all_predecessors(self.ctx(), hg_changesets)
            .await?)
    }

    /// Record mutations in the mutation store, timestamped with the current
    /// time. The Mercurial changesets of the successors and predecessors are
    /// derived if necessary.
    pub async fn record_hg_mutations(
        &self,
        mutations: Vec<CommitMutation>,
    ) -> Result<(), MononokeError> {
        let cs_ids: HashSet<ChangesetId> = mutations
            .iter()
            .flat_map(|mutation| {
                std::iter::once(mutation.successor).chain(mutation.predecessors.iter().copied())
            })
            .collect();
        let hg_ids: HashMap<ChangesetId, HgChangesetId> = stream::iter(cs_ids)
            .map(|cs_id| async move {
                let hg_id = self.repo().derive_hg_changeset(self.ctx(), cs_id).await?;
                Ok::<_, MononokeError>((cs_id, hg_id))
            })
            .buffer_unordered(10)
            .try_collect()
            .await?;

        let timestamp = DateTime::now().timestamp_secs();
        let entries: Vec<HgMutationEntry> = mutations
            .into_iter()
            .map(|mutation| {
                HgMutationEntry::new(
                    hg_ids[&mutation.successor],
                    mutation
                        .predecessors
                        .iter()
                        .map(|cs_id| hg_ids[cs_id])
                        .collect(),
                    Vec::new(),
                    mutation.op,
                    mutation.user,
                    timestamp,
                    0,
                    Vec::new(),
                )
            })
            .collect();
        let successors = entries.iter().map(|entry| *entry.successor()).collect();
        self.repo()
            .hg_mutation_store()
            .add_entries(self.ctx(), successors, entries)
            .await?;
        Ok(())
    }

    // TODO(mbthomas): get_hg_from_bonsai -> derive_hg_changeset
    pub async fn get_hg_from_bonsai(
        &self,
        cs_id: ChangesetId,
    ) -> Result<HgChangesetId, MononokeError> {
        let hg_cs_id = self.repo().derive_hg_changeset(self.ctx(), cs_id).await?;
        self.repo_ctx()
            .ensure_hg_augmented_manifests_derived(Some(cs_id))
            .await?;
        Ok(hg_cs_id)
    }

    /// This provides the same functionality as
    /// `mononoke_api::RepoContext::location_to_changeset_id`. It just wraps the request and
    /// response using Mercurial specific types.
    pub async fn location_to_hg_changeset_id(
        &self,
        location: Location<HgChangesetId>,
        count: u64,
    ) -> Result<Vec<HgChangesetId>, MononokeError> {
        let cs_location = location
            .and_then_descendant(|descendant| async move {
                self.repo()
                    .bonsai_hg_mapping()
                    .get_bonsai_from_hg(self.ctx(), descendant)
                    .await?
                    .ok_or_else(|| {
                        MononokeError::InvalidRequest(format!(
                            "hg changeset {} not found",
                            location.descendant
                        ))
                    })
            })
            .await?;
        let result_csids = self
            .repo_ctx()
            .location_to_changeset_id(cs_location, count)
            .await?;
        stream::iter(result_csids)
            .map(|bcs_id| async move {
                let hg_cs_id = self
                    .repo()
                    .derive_hg_changeset(self.ctx(), bcs_id)
                    .await
                    .map_err(MononokeError::from)?;
                self.repo_ctx()
                    .ensure_hg_augmented_manifests_derived(Some(bcs_id))
                    .await?;
                Ok(hg_cs_id)
            })
            .buffered(100)
            .try_collect()
            .await
    }

    /// This provides the same functionality as
    /// `mononoke_api::RepoContext::many_changeset_ids_to_locations`. It just translates to
    /// and from Mercurial types.
    pub async fn many_hg_changeset_ids_to_locations(
        &self,
        hg_master_heads: Vec<HgChangesetId>,
        hg_ids: Vec<HgChangesetId>,
    ) -> Result<HashMap<HgChangesetId, Result<Location<HgChangesetId>, MononokeError>>, MononokeError>
    {
        let all_hg_ids: Vec<_> = hg_ids
            .iter()
            .cloned()
            .chain(hg_master_heads.clone())
            .collect();
        let hg_to_bonsai: HashMap<HgChangesetId, ChangesetId> = self
            .repo()
            .get_hg_bonsai_mapping(self.ctx().clone(), all_hg_ids)
            .await?
            .into_iter()
            .collect();
        let master_heads = hg_master_heads
            .iter()
            .map(|master_id| {
                hg_to_bonsai.get(master_id).cloned().ok_or_else(|| {
                    MononokeError::InvalidRequest(format!(
                        "failed to find bonsai equivalent for client head {master_id}"
                    ))
                })
            })
            .collect::<Result<Vec<_>, MononokeError>>()?;

        // We should treat hg_ids as being absolutely any hash. It is perfectly valid for the
        // server to have not encountered the hash that it was given to convert. Filter out the
        // hashes that we could not convert to bonsai.
        let cs_ids = hg_ids
            .iter()
            .filter_map(|hg_id| hg_to_bonsai.get(hg_id).cloned())
            .collect::<Vec<ChangesetId>>();

        // `hash_to_location` resolves a commit by raw hash (checkout/pull of a
        // commit not on a bookmark, or a snapshot parent during restore), bypassing
        // the other resolution chokepoints, so materialize the augmented manifest
        // here too — otherwise the subsequent `trees` fetch fails closed.
        self.repo_ctx()
            .ensure_hg_augmented_manifests_derived(cs_ids.iter().copied())
            .await?;

        let cs_to_blocations = self
            .repo_ctx()
            .many_changeset_ids_to_locations(master_heads, cs_ids)
            .await?;

        let bonsai_to_hg: HashMap<ChangesetId, HgChangesetId> = self
            .repo()
            .get_hg_bonsai_mapping(
                self.ctx().clone(),
                cs_to_blocations
                    .values()
                    .filter_map(|result| match result {
                        Ok(l) => Some(l.descendant),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            )
            .await?
            .into_iter()
            .map(|(hg_id, cs_id)| (cs_id, hg_id))
            .collect();
        let response = hg_ids
            .into_iter()
            .filter_map(|hg_id| hg_to_bonsai.get(&hg_id).map(|cs_id| (hg_id, cs_id)))
            .filter_map(|(hg_id, cs_id)| {
                cs_to_blocations
                    .get(cs_id)
                    .map(|cs_result| (hg_id, cs_result.clone()))
            })
            .map(|(hg_id, cs_result)| {
                let cs_result = match cs_result {
                    Ok(cs_location) => cs_location.try_map_descendant(|descendant| {
                        bonsai_to_hg.get(&descendant).cloned().ok_or_else(|| {
                            MononokeError::InvalidRequest(format!(
                                "failed to find hg equivalent for bonsai {descendant}"
                            ))
                        })
                    }),
                    Err(e) => Err(e),
                };
                (hg_id, cs_result)
            })
            .collect::<HashMap<HgChangesetId, Result<Location<HgChangesetId>, MononokeError>>>();

        Ok(response)
    }

    pub async fn revlog_commit_data(
        &self,
        hg_cs_id: HgChangesetId,
    ) -> Result<Option<Bytes>, MononokeError> {
        let ctx = self.ctx();
        let blobstore = self.repo().repo_blobstore();
        let revlog_cs = RevlogChangeset::load(ctx, blobstore, hg_cs_id)
            .await
            .map_err(MononokeError::from)?;
        let revlog_cs = match revlog_cs {
            None => return Ok(None),
            Some(x) => x,
        };

        let mut buffer = Vec::new();
        revlog_cs
            .generate_for_hash_verification(&mut buffer)
            .map_err(MononokeError::from)?;
        Ok(Some(buffer.into()))
    }

    /// resolve a bookmark name to an Hg Changeset
    pub async fn resolve_bookmark(
        &self,
        bookmark: impl AsRef<str>,
        freshness: Freshness,
    ) -> Result<Option<HgChangesetId>, MononokeError> {
        match self
            .repo_ctx
            .resolve_bookmark(&BookmarkKey::new(bookmark)?, freshness)
            .await?
        {
            Some(c) => c.hg_id().await,
            None => Ok(None),
        }
    }

    /// resolve a bookmark name to an Hg Changeset
    pub async fn resolve_bookmark_git(
        &self,
        bookmark: impl AsRef<str>,
        freshness: Freshness,
    ) -> Result<Option<GitSha1>, MononokeError> {
        match self
            .repo_ctx
            .resolve_bookmark(&BookmarkKey::new(bookmark)?, freshness)
            .await?
        {
            Some(c) => c.git_sha1().await,
            None => Ok(None),
        }
    }

    /// Return (at most 10) HgChangesetIds in the range described by the low and high parameters.
    pub async fn get_hg_in_range(
        &self,
        low: HgChangesetId,
        high: HgChangesetId,
    ) -> Result<Vec<HgChangesetId>, MononokeError> {
        const LIMIT: usize = 10;
        let bonsai_hg_mapping = self.repo().bonsai_hg_mapping();
        bonsai_hg_mapping
            .get_hg_in_range(self.ctx(), low, high, LIMIT)
            .await
            .map_err(|e| e.into())
    }

    /// Convert a list of hg changesets to a list of bonsai changesets.
    pub(crate) async fn convert_changeset_ids(
        &self,
        changesets: Vec<HgChangesetId>,
    ) -> Result<Vec<ChangesetId>, MononokeError> {
        Ok(self
            .repo()
            .get_hg_bonsai_mapping(self.ctx().clone(), changesets.to_vec())
            .await
            .context("error fetching hg bonsai mapping")?
            .iter()
            .map(|(_, bcs_id)| *bcs_id)
            .collect())
    }

    /// Check if all changesets in the list are public.
    /// This may treat commits that "recently" became public as draft.
    pub async fn is_all_public(&self, changesets: &[HgChangesetId]) -> Result<bool, MononokeError> {
        let len = changesets.len();
        let public_phases = self
            .repo()
            .phases()
            .get_cached_public(
                self.ctx(),
                self.convert_changeset_ids(changesets.to_vec()).await?,
            )
            .await?;
        Ok(len == public_phases.len())
    }

    /// Return a mapping of commits to their parents that are in the segment of
    /// of the commit graph bounded by common and heads.
    ///
    /// This should be used for public fetches only, since for draft commits we need to
    /// make sure filenodes are derived before sending.
    pub async fn get_graph_mapping_stream(
        &self,
        common: Vec<HgChangesetId>,
        heads: Vec<HgChangesetId>,
    ) -> Result<
        impl Stream<Item = Result<(HgChangesetId, Vec<HgChangesetId>), MononokeError>>
        + 'static
        + use<R>,
        MononokeError,
    > {
        let ctx = self.ctx().clone();
        let repo = self.repo().clone();
        let (bonsai_common, bonsai_heads) = try_join!(
            self.convert_changeset_ids(common),
            self.convert_changeset_ids(heads),
        )?;
        debug!("Streaming Commit Graph...");
        let commit_graph_stream = self
            .repo_ctx()
            .repo()
            .commit_graph()
            .ancestors_difference_stream(&ctx, bonsai_heads, bonsai_common)
            .await?
            .map_err(MononokeError::from)
            .map_ok(move |bcs_id| {
                let ctx = ctx.clone();
                let repo = repo.clone();
                async move {
                    repo.get_hg_changeset_and_parents_from_bonsai(ctx, bcs_id)
                        .await
                        .map_err(MononokeError::from)
                }
            })
            .try_buffered(100);
        Ok(commit_graph_stream)
    }

    /// Return a mapping of commits to their parents that are in the segment of
    /// of the commit graph bounded by common and heads, along with whether
    /// each commit is a draft.
    pub async fn get_graph_mapping(
        &self,
        common: Vec<HgChangesetId>,
        heads: Vec<HgChangesetId>,
    ) -> Result<Vec<(HgChangesetId, (Vec<HgChangesetId>, bool))>, MononokeError> {
        let ctx = self.ctx().clone();
        let repo = self.repo();
        let phases = repo.phases();

        let common_set: HashSet<_> = common.iter().cloned().collect();
        let heads_vec: Vec<_> = heads.to_vec();

        debug!("Calculating Commit Graph...");
        let (bonsai_common, bonsai_heads) = try_join!(
            self.convert_changeset_ids(common),
            self.convert_changeset_ids(heads),
        )?;

        // Getbundle walks the draft commits down from the heads to derive
        // filenodes for their public roots, because it sends filenodes along
        // with the commits, and classifies drafts from that walk.  The commit
        // graph doesn't include filenodes, so with the pull optimizations on
        // the walk is skipped and drafts are classified from the phases of
        // the commits being returned instead.
        let walk_drafts = !justknobs::eval(
            "scm/mononoke:commit_graph_pull_optimizations",
            ctx.client_request_info().map(|c| c.correlator.as_str()),
            Some(self.repo_ctx().name()),
        );
        let walked_draft_commits = async {
            if !walk_drafts {
                return anyhow::Ok(None);
            }
            Ok(Some(
                find_new_draft_commits_and_derive_filenodes_for_public_roots(
                    &ctx,
                    repo,
                    &common_set,
                    &heads_vec,
                    phases,
                )
                .await?,
            ))
        };
        let (walked_draft_commits, missing_commits) = try_join!(
            walked_draft_commits,
            self.repo_ctx().repo().commit_graph().ancestors_difference(
                &ctx,
                bonsai_heads,
                bonsai_common,
            )
        )?;

        let classification = async {
            match walked_draft_commits {
                Some(draft_commits) => anyhow::Ok(DraftClassification::Walked(draft_commits)),
                None => {
                    let classify_start = Instant::now();
                    let public = stream::iter(missing_commits.clone())
                        .chunks(100)
                        .map(|chunk| phases.get_cached_public(&ctx, chunk))
                        .buffered(25)
                        .try_concat()
                        .await?;
                    ctx.perf_counters().add_duration_us(
                        PerfCounterType::PhasesSectionUs,
                        classify_start.elapsed(),
                    );
                    Ok(DraftClassification::Phases(public))
                }
            }
        };

        let cs_parent_mapping = stream::iter(missing_commits.clone())
            .map(move |cs_id| async move {
                let parents = repo
                    .commit_graph()
                    .changeset_parents(self.ctx(), cs_id)
                    .await?;
                Ok::<_, Error>((cs_id, parents))
            })
            .buffered(100)
            .try_collect::<Vec<_>>();

        let (classification, cs_parent_mapping) = try_join!(classification, cs_parent_mapping)?;

        let all_cs_ids = cs_parent_mapping
            .clone()
            .into_iter()
            .flat_map(|(_, parents)| parents)
            .chain(missing_commits)
            .collect::<HashSet<_>>();

        let map_chunk_size = 100;

        let bonsai_hg_mapping = stream::iter(all_cs_ids)
            .chunks(map_chunk_size)
            .map(move |chunk| async move {
                let mapping = self
                    .repo()
                    .get_hg_bonsai_mapping(self.ctx().clone(), chunk.to_vec())
                    .await
                    .context("error fetching hg bonsai mapping")?;
                Ok::<_, Error>(mapping)
            })
            .buffered(25)
            .try_collect::<Vec<Vec<(HgChangesetId, ChangesetId)>>>()
            .await?
            .into_iter()
            .flatten()
            .map(|(hgid, csid)| (csid, hgid))
            .collect::<HashMap<_, _>>();

        let get_hg_id_fn = |cs_id| {
            bonsai_hg_mapping
                .get(&cs_id)
                .cloned()
                .with_context(|| format_err!("failed to find bonsai '{cs_id}' mapping to hg"))
        };

        let hg_parent_mapping = cs_parent_mapping
            .into_iter()
            .map(|(cs_id, cs_parents)| {
                let hg_id = get_hg_id_fn(cs_id)?;
                let hg_parents = cs_parents
                    .into_iter()
                    .map(get_hg_id_fn)
                    .collect::<Result<Vec<HgChangesetId>, Error>>()
                    .map_err(MononokeError::from)?;
                let is_draft = classification.is_draft(cs_id, hg_id);
                Ok((hg_id, (hg_parents, is_draft)))
            })
            .collect::<Result<Vec<_>, MononokeError>>()?;

        Ok(hg_parent_mapping)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use blobstore::BlobstoreGetData;
    use blobstore::BlobstoreIsPresent;
    use bonsai_hg_mapping::BonsaiHgMappingEntry;
    use ephemeral_blobstore::RepoEphemeralStoreRef;
    use fbinit::FacebookInit;
    use futures::FutureExt;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs_async;
    use maplit::hashmap;
    use memblob::Memblob;
    use mercurial_types::HgParents;
    use mercurial_types::blobs::ChangesetMetadata;
    use mercurial_types::blobs::HgChangesetContent;
    use mercurial_types::calculate_hg_node_id;
    use mononoke_api::ChangesetSpecifier;
    use mononoke_api::repo::Repo;
    use mononoke_api::repo::RepoContext;
    use mononoke_api::repo::RepoWithBubble;
    use mononoke_macros::mononoke;
    use samplingblob::SamplingBlobstore;
    use samplingblob::SamplingHandler;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;

    use super::*;
    use crate::RepoContextHgExt;

    #[mononoke::fbinit_test]
    async fn test_new_hg_context(fb: FacebookInit) -> Result<(), MononokeError> {
        let ctx = CoreContext::test_mock(fb);

        let repo: Repo = test_repo_factory::build_empty(ctx.fb).await?;
        let repo_ctx = RepoContext::new_test(ctx, Arc::new(repo)).await?;

        let hg = repo_ctx.hg();
        assert_eq!(hg.repo_ctx().name(), "repo");

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_record_hg_mutations(fb: FacebookInit) -> Result<(), MononokeError> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(ctx.fb).await?;

        // B1 amends B, and C1 and D1 rebase C and D onto B1.
        let a = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a", "a")
            .commit()
            .await?;
        let b = CreateCommitContext::new(&ctx, &repo, vec![a])
            .add_file("b", "b")
            .commit()
            .await?;
        let c = CreateCommitContext::new(&ctx, &repo, vec![b])
            .add_file("c", "c")
            .commit()
            .await?;
        let b1 = CreateCommitContext::new(&ctx, &repo, vec![a])
            .add_file("b", "b1")
            .commit()
            .await?;
        let c1 = CreateCommitContext::new(&ctx, &repo, vec![b1])
            .add_file("c", "c")
            .commit()
            .await?;

        let repo_ctx = RepoContext::new_test(ctx, Arc::new(repo)).await?;
        let hg = repo_ctx.hg();
        hg.record_hg_mutations(vec![
            CommitMutation {
                successor: b1,
                predecessors: vec![b],
                op: "amend".to_string(),
                user: "alice".to_string(),
            },
            CommitMutation {
                successor: c1,
                predecessors: vec![c],
                op: "rebase".to_string(),
                user: "bob".to_string(),
            },
        ])
        .await?;

        let hg_b = hg.get_hg_from_bonsai(b).await?;
        let hg_c = hg.get_hg_from_bonsai(c).await?;
        let hg_b1 = hg.get_hg_from_bonsai(b1).await?;
        let hg_c1 = hg.get_hg_from_bonsai(c1).await?;
        let summary = |entries: Vec<HgMutationEntry>| {
            entries
                .iter()
                .map(|entry| {
                    (
                        *entry.successor(),
                        entry.predecessors().copied().collect::<Vec<_>>(),
                        entry.op().to_string(),
                        entry.user().to_string(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            summary(hg.fetch_mutations(HashSet::from([hg_b1])).await?),
            vec![(hg_b1, vec![hg_b], "amend".to_string(), "alice".to_string())],
        );
        assert_eq!(
            summary(hg.fetch_mutations(HashSet::from([hg_c1])).await?),
            vec![(hg_c1, vec![hg_c], "rebase".to_string(), "bob".to_string())],
        );
        assert!(
            hg.fetch_mutations(HashSet::from([hg_b])).await?.is_empty(),
            "the replaced commit has no mutation history of its own",
        );

        Ok(())
    }

    async fn assert_graph_mapping_marks_drafts(fb: FacebookInit) -> Result<(), MononokeError> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(ctx.fb).await?;

        // A and B are public, C and D are drafts on top of them, and only A
        // is common, so B is returned as a public commit among the drafts.
        let a = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("a", "a")
            .commit()
            .await?;
        let b = CreateCommitContext::new(&ctx, &repo, vec![a])
            .add_file("b", "b")
            .commit()
            .await?;
        let c = CreateCommitContext::new(&ctx, &repo, vec![a])
            .add_file("c", "c")
            .commit()
            .await?;
        let d = CreateCommitContext::new(&ctx, &repo, vec![b])
            .add_file("d", "d")
            .commit()
            .await?;
        repo.phases().add_reachable_as_public(&ctx, vec![b]).await?;

        let repo_ctx = RepoContext::new_test(ctx, Arc::new(repo)).await?;
        let hg = repo_ctx.hg();
        let hg_a = hg.get_hg_from_bonsai(a).await?;
        let hg_b = hg.get_hg_from_bonsai(b).await?;
        let hg_c = hg.get_hg_from_bonsai(c).await?;
        let hg_d = hg.get_hg_from_bonsai(d).await?;

        let mapping: HashMap<_, _> = hg
            .get_graph_mapping(vec![hg_a], vec![hg_c, hg_d])
            .await?
            .into_iter()
            .collect();
        assert_eq!(
            mapping,
            hashmap! {
                hg_d => (vec![hg_b], true),
                hg_b => (vec![hg_a], false),
                hg_c => (vec![hg_a], true),
            }
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_get_graph_mapping_marks_drafts(fb: FacebookInit) -> Result<(), MononokeError> {
        // Drafts are classified from the phases with the knob on and by the
        // getbundle draft walk with it off.
        for knob_on in [true, false] {
            with_just_knobs_async(
                JustKnobsInMemory::new(hashmap! {
                    "scm/mononoke:commit_graph_pull_optimizations".to_string() => KnobVal::Bool(knob_on),
                }),
                assert_graph_mapping_marks_drafts(fb).boxed(),
            )
            .await?;
        }
        Ok(())
    }

    async fn verify_directory(
        hg: &HgRepoContext<Repo>,
        contents: String,
    ) -> Result<HgManifestEnvelope, MononokeError> {
        let contents = Bytes::from(contents);
        let node_id = calculate_hg_node_id(&contents, &HgParents::new(None, None));
        hg.verify_tree(node_id, None, None, contents, None).await
    }

    async fn assert_augmented_manifests_build_without_hg_manifests(
        fb: FacebookInit,
    ) -> anyhow::Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let hg = RepoContext::new_test(ctx, Arc::new(repo)).await?.hg();

        // Root first, so the build has to order the batch itself.
        let leaf = verify_directory(&hg, String::new()).await?;
        let middle = verify_directory(&hg, format!("d\0{}t\n", leaf.node_id())).await?;
        let root = verify_directory(&hg, format!("m\0{}t\n", middle.node_id())).await?;
        let trees = vec![root, middle, leaf];
        hg.build_and_store_augmented_manifests_for_uploaded_trees(trees.clone())
            .await?;

        for tree in &trees {
            let node_id = tree.node_id();
            let augmented_key = HgAugmentedManifestId::new(node_id).blobstore_key();
            assert!(
                hg.is_key_present_in_blobstore(&augmented_key, None).await?,
                "the augmented manifest of {node_id} should be stored"
            );
            assert!(
                !hg.tree_exists(HgManifestId::new(node_id)).await?,
                "the hg manifest of {node_id} should not be stored yet"
            );
        }
        Ok(())
    }

    async fn assert_tree_uploaded(fb: FacebookInit, required: bool) -> anyhow::Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let hg = RepoContext::new_test(ctx, Arc::new(repo)).await?.hg();

        // `leaf` gets both manifests, `middle` only its augmented manifest and
        // `root` only its hg manifest.
        let leaf = verify_directory(&hg, String::new()).await?;
        let middle = verify_directory(&hg, format!("d\0{}t\n", leaf.node_id())).await?;
        let root = verify_directory(&hg, format!("m\0{}t\n", middle.node_id())).await?;
        hg.build_and_store_augmented_manifests_for_uploaded_trees(vec![
            leaf.clone(),
            middle.clone(),
        ])
        .await?;
        hg.store_verified_tree(leaf.clone()).await?;
        hg.store_verified_tree(root.clone()).await?;

        let uploaded = |tree: &HgManifestEnvelope| {
            let manifest_id = HgManifestId::new(tree.node_id());
            let hg = hg.clone();
            async move { hg.is_tree_uploaded(manifest_id).await }
        };
        assert!(
            uploaded(&leaf).await?,
            "a tree with both manifests is uploaded"
        );
        assert!(
            !uploaded(&middle).await?,
            "a tree without its hg manifest is never uploaded"
        );
        assert_eq!(
            uploaded(&root).await?,
            !required,
            "a tree without its augmented manifest is uploaded only while lookup does not require it"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_tree_uploaded(fb: FacebookInit) -> anyhow::Result<()> {
        for required in [true, false] {
            with_just_knobs_async(
                JustKnobsInMemory::new(hashmap! {
                    STORE_AUGMENTED_MANIFESTS_AT_TREE_UPLOAD.to_string() => KnobVal::Bool(true),
                    REQUIRE_AUGMENTED_MANIFEST_FOR_TREE_LOOKUP.to_string() => KnobVal::Bool(required),
                }),
                assert_tree_uploaded(fb, required).boxed(),
            )
            .await?;
        }
        Ok(())
    }

    /// Fails every read of an Hg manifest blob, standing in for a store where
    /// they have been deleted.
    #[derive(Debug)]
    struct DenyHgManifestReads;

    impl DenyHgManifestReads {
        fn check(key: &str) -> anyhow::Result<()> {
            anyhow::ensure!(
                !key.contains("hgmanifest."),
                "unexpected read of Hg manifest {key}"
            );
            Ok(())
        }
    }

    impl SamplingHandler for DenyHgManifestReads {
        fn sample_get(
            &self,
            _ctx: &CoreContext,
            key: &str,
            _value: Option<&BlobstoreGetData>,
        ) -> anyhow::Result<()> {
            Self::check(key)
        }

        fn sample_is_present(
            &self,
            _ctx: &CoreContext,
            key: &str,
            _value: &BlobstoreIsPresent,
        ) -> anyhow::Result<()> {
            Self::check(key)
        }
    }

    async fn assert_tree_uploaded_read_from_augmented(fb: FacebookInit) -> anyhow::Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_blobstore(Arc::new(SamplingBlobstore::new(
                Memblob::default(),
                Arc::new(DenyHgManifestReads),
            )))
            .build()
            .await?;
        let hg = RepoContext::new_test(ctx, Arc::new(repo)).await?.hg();

        // `augmented` gets only its augmented manifest, `hg_only` only its hg
        // manifest, and `missing` neither.
        let augmented = verify_directory(&hg, String::new()).await?;
        let hg_only = verify_directory(&hg, format!("a\0{}t\n", augmented.node_id())).await?;
        let missing = verify_directory(&hg, format!("b\0{}t\n", augmented.node_id())).await?;
        hg.build_and_store_augmented_manifests_for_uploaded_trees(vec![augmented.clone()])
            .await?;
        hg.store_verified_tree(hg_only.clone()).await?;

        let uploaded = |tree: &HgManifestEnvelope| {
            let manifest_id = HgManifestId::new(tree.node_id());
            let hg = hg.clone();
            async move { hg.is_tree_uploaded(manifest_id).await }
        };
        assert!(
            uploaded(&augmented).await?,
            "a tree with only its augmented manifest is uploaded"
        );
        assert!(
            !uploaded(&hg_only).await?,
            "a tree without its augmented manifest is not uploaded"
        );
        assert!(!uploaded(&missing).await?, "a missing tree is not uploaded");
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_tree_uploaded_read_from_augmented(fb: FacebookInit) -> anyhow::Result<()> {
        for required in [true, false] {
            with_just_knobs_async(
                JustKnobsInMemory::new(hashmap! {
                    STORE_AUGMENTED_MANIFESTS_AT_TREE_UPLOAD.to_string() => KnobVal::Bool(true),
                    REQUIRE_AUGMENTED_MANIFEST_FOR_TREE_LOOKUP.to_string() => KnobVal::Bool(required),
                    "scm/mononoke:hgmanifest_read_from_augmented".to_string() => KnobVal::Bool(true),
                }),
                assert_tree_uploaded_read_from_augmented(fb).boxed(),
            )
            .await?;
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_augmented_manifests_build_without_hg_manifests(
        fb: FacebookInit,
    ) -> anyhow::Result<()> {
        with_just_knobs_async(
            JustKnobsInMemory::new(hashmap! {
                STORE_AUGMENTED_MANIFESTS_AT_TREE_UPLOAD.to_string() => KnobVal::Bool(true),
            }),
            assert_augmented_manifests_build_without_hg_manifests(fb).boxed(),
        )
        .await
    }

    struct UploadedCommit {
        hg_cs: HgBlobChangeset,
        bcs: BonsaiChangeset,
        root: HgAugmentedManifestId,
    }

    async fn upload_commit_with_augmented_root(
        hg: &HgRepoContext<Repo>,
    ) -> anyhow::Result<UploadedCommit> {
        let ctx = hg.ctx();
        let repo = hg.repo();
        let tree = verify_directory(hg, String::new()).await?;
        hg.build_and_store_augmented_manifests_for_uploaded_trees(vec![tree.clone()])
            .await?;
        let cs_id = CreateCommitContext::new_root(ctx, repo).commit().await?;
        let bcs = cs_id.load(ctx, repo.repo_blobstore()).await?;
        let hg_cs = HgBlobChangeset::new(HgChangesetContent::new_from_parts(
            HgParents::new(None, None),
            HgManifestId::new(tree.node_id()),
            ChangesetMetadata {
                user: "test".to_string(),
                time: DateTime::from_timestamp(0, 0)?,
                extra: BTreeMap::new(),
                message: "uploaded".to_string(),
            },
            vec![],
        ))?;
        hg_cs.save(ctx, repo.repo_blobstore()).await?;
        Ok(UploadedCommit {
            hg_cs,
            bcs,
            root: HgAugmentedManifestId::new(tree.node_id()),
        })
    }

    async fn bubble_hg_context(
        ctx: &CoreContext,
        repo: &Repo,
    ) -> anyhow::Result<HgRepoContext<Repo>> {
        let bubble = repo
            .repo_ephemeral_store()
            .create_bubble(ctx, None, vec![])
            .await?;
        Ok(
            RepoContext::new_test(ctx.clone(), Arc::new(repo.with_bubble(bubble)))
                .await?
                .hg(),
        )
    }

    async fn mapped_root(
        ctx: &CoreContext,
        repo: &Repo,
        cs_id: ChangesetId,
    ) -> anyhow::Result<Option<HgAugmentedManifestId>> {
        Ok(repo
            .repo_derived_data()
            .fetch_derived::<RootHgAugmentedManifestId>(ctx, cs_id)
            .await?
            .map(|root| root.hg_augmented_manifest_id()))
    }

    async fn assert_uploaded_root_mapped_in_bubble(fb: FacebookInit) -> anyhow::Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_ephemeral_store()
            .build()
            .await?;
        let hg = bubble_hg_context(&ctx, &repo).await?;
        let uploaded = upload_commit_with_augmented_root(&hg).await?;
        let cs_id = uploaded.bcs.get_changeset_id();
        let hg_cs_id = uploaded.hg_cs.get_changeset_id();
        hg.repo()
            .bonsai_hg_mapping()
            .add(
                &ctx,
                BonsaiHgMappingEntry {
                    hg_cs_id,
                    bcs_id: cs_id,
                },
            )
            .await?;

        hg.ensure_uploaded_augmented_manifests_derived(&[Ok((uploaded.hg_cs, uploaded.bcs))])
            .await;

        assert_eq!(
            mapped_root(&ctx, hg.repo(), cs_id).await?,
            Some(uploaded.root),
            "the bubble maps the commit to its uploaded root"
        );
        assert_eq!(
            mapped_root(&ctx, &repo, cs_id).await?,
            None,
            "nothing is mapped in the permanent store"
        );
        let changeset = hg
            .repo_ctx()
            .changeset(ChangesetSpecifier::Bonsai(cs_id))
            .await?
            .expect("the uploaded commit is in the bubble");
        assert_eq!(
            changeset.hg_id().await?,
            Some(hg_cs_id),
            "the commit keeps the uploaded hg id"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_uploaded_root_mapped_in_bubble(fb: FacebookInit) -> anyhow::Result<()> {
        with_just_knobs_async(
            JustKnobsInMemory::new(hashmap! {
                STORE_AUGMENTED_MANIFESTS_AT_TREE_UPLOAD.to_string() => KnobVal::Bool(true),
                MAP_UPLOADED_ROOT_HG_AUGMENTED_MANIFEST.to_string() => KnobVal::Bool(true),
                "scm/mononoke:enable_bubble_hg_mapping".to_string() => KnobVal::Bool(true),
            }),
            assert_uploaded_root_mapped_in_bubble(fb).boxed(),
        )
        .await
    }

    async fn assert_uploaded_root_not_mapped_in_bubble_when_disabled(
        fb: FacebookInit,
    ) -> anyhow::Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_ephemeral_store()
            .build()
            .await?;
        let hg = bubble_hg_context(&ctx, &repo).await?;
        let uploaded = upload_commit_with_augmented_root(&hg).await?;
        let cs_id = uploaded.bcs.get_changeset_id();

        hg.ensure_uploaded_augmented_manifests_derived(&[Ok((uploaded.hg_cs, uploaded.bcs))])
            .await;

        assert_eq!(
            mapped_root(&ctx, hg.repo(), cs_id).await?,
            None,
            "with the bubble hg mapping off, bubbles are left alone as before"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_uploaded_root_not_mapped_in_bubble_when_disabled(
        fb: FacebookInit,
    ) -> anyhow::Result<()> {
        with_just_knobs_async(
            JustKnobsInMemory::new(hashmap! {
                STORE_AUGMENTED_MANIFESTS_AT_TREE_UPLOAD.to_string() => KnobVal::Bool(true),
                MAP_UPLOADED_ROOT_HG_AUGMENTED_MANIFEST.to_string() => KnobVal::Bool(true),
                "scm/mononoke:enable_bubble_hg_mapping".to_string() => KnobVal::Bool(false),
            }),
            assert_uploaded_root_not_mapped_in_bubble_when_disabled(fb).boxed(),
        )
        .await
    }

    async fn assert_uploaded_root_mapped_in_permanent_repo(fb: FacebookInit) -> anyhow::Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let repo: Repo = test_repo_factory::build_empty(fb).await?;
        let hg = RepoContext::new_test(ctx.clone(), Arc::new(repo.clone()))
            .await?
            .hg();
        let uploaded = upload_commit_with_augmented_root(&hg).await?;
        let cs_id = uploaded.bcs.get_changeset_id();

        hg.ensure_uploaded_augmented_manifests_derived(&[Ok((uploaded.hg_cs, uploaded.bcs))])
            .await;

        assert_eq!(
            mapped_root(&ctx, &repo, cs_id).await?,
            Some(uploaded.root),
            "a permanent commit is still mapped in the permanent store"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_uploaded_root_mapped_in_permanent_repo(fb: FacebookInit) -> anyhow::Result<()> {
        with_just_knobs_async(
            JustKnobsInMemory::new(hashmap! {
                STORE_AUGMENTED_MANIFESTS_AT_TREE_UPLOAD.to_string() => KnobVal::Bool(true),
                MAP_UPLOADED_ROOT_HG_AUGMENTED_MANIFEST.to_string() => KnobVal::Bool(true),
            }),
            assert_uploaded_root_mapped_in_permanent_repo(fb).boxed(),
        )
        .await
    }
}
