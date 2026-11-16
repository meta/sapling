/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(trait_alias)]

mod errors;

use std::collections::HashSet;
use std::fmt::Display;
use std::sync::Arc;

use atomic_counter::AtomicCounter;
use atomic_counter::RelaxedCounter;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bytesize::ByteSize;
use cas_client::CasClient;
use cloned::cloned;
use context::CoreContext;
pub use errors::CasChangesetUploaderError;
use futures::FutureExt;
use futures::future;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use futures::try_join;
use futures_watchdog::WatchdogExt;
use manifest::Diff;
use manifest::Entry;
use manifest::ManifestOps;
use mercurial_types::HgAugmentedManifestEntry;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgChangesetId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::HgPreloadedAugmentedManifest;
use mercurial_types::sharded_augmented_manifest::hg_manifest_diff;
use mercurial_types::sharded_augmented_manifest::list_all_hg_entries;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use mononoke_types::MononokeDigest;
use mononoke_types::NonRootMPath;
use repo_blobstore::RepoBlobstoreArc;
use repo_identity::RepoIdentityRef;
use restricted_paths::ManifestType;
use restricted_paths::RestrictedManifestId;
use restricted_paths::RestrictedPathsRef;
use scm_client::ScmCasClient;
use scm_client::UploadOutcome;
use stats::prelude::*;
use tracing::debug;
use tracing::info;
use tracing::warn;

const MAX_CONCURRENT_MANIFESTS: usize = 50;
const MAX_CONCURRENT_MANIFESTS_TREES_ONLY: usize = 500;
const MAX_CONCURRENT_FILES_PER_MANIFEST: usize = 20;
const MAX_CONCURRENT_RESTRICTION_CHECKS: usize = 20;
const DEBUG_LOG_INTERVAL: usize = 10000;

async fn load_preloaded_manifest(
    ctx: &CoreContext,
    repo: &impl Repo,
    manifest_id: HgAugmentedManifestId,
) -> anyhow::Result<HgPreloadedAugmentedManifest> {
    let envelope = manifest_id.load(ctx, repo.repo_blobstore()).await?;
    HgPreloadedAugmentedManifest::load_from_sharded(envelope, ctx, repo.repo_blobstore()).await
}

#[derive(Default, Debug)]
pub struct UploadCounters {
    // Trees and blobs
    uploaded: RelaxedCounter,
    // Trees and blobs
    uploaded_bytes: RelaxedCounter,
    // Trees and blobs
    already_present: RelaxedCounter,
    // Trees and blobs
    largest_uploaded_blob_bytes: RelaxedCounter,
    // Only file content blobs
    uploaded_files: RelaxedCounter,
    // Only file content blobs
    already_present_files: RelaxedCounter,
    // Only file content blobs
    redacted_files: RelaxedCounter,
}

#[derive(Debug, PartialEq, Clone)]
pub enum UploadPolicy {
    All,
    BlobsOnly,
    TreesOnly,
}

#[derive(Debug, PartialEq, Clone)]
pub enum PriorLookupPolicy {
    All,
    BlobsOnly,
    TreesOnly,
    None,
}

impl PriorLookupPolicy {
    pub fn enabled_trees(&self) -> bool {
        matches!(self, PriorLookupPolicy::TreesOnly) || matches!(self, PriorLookupPolicy::All)
    }
    pub fn enabled_blobs(&self) -> bool {
        matches!(self, PriorLookupPolicy::BlobsOnly) || matches!(self, PriorLookupPolicy::All)
    }
}

impl Display for UploadCounters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(
            f,
            "uploaded digests: {}, already present digests: {}, uploaded bytes: {}, the largest uploaded blob: {}",
            self.uploaded.get(),
            self.already_present.get(),
            ByteSize::b(self.uploaded_bytes.get() as u64)
                .display()
                .iec(),
            ByteSize::b(self.largest_uploaded_blob_bytes.get() as u64)
                .display()
                .iec(),
        )
    }
}

pub type UploadStats = Arc<UploadCounters>;

impl UploadCounters {
    pub fn sum(&self) -> usize {
        self.uploaded.get() + self.already_present.get()
    }

    pub fn add(&self, other: &UploadCounters) {
        self.uploaded.add(other.uploaded.get());
        self.already_present.add(other.already_present.get());
        self.uploaded_bytes.add(other.uploaded_bytes.get());
        if self.largest_uploaded_blob_bytes.get() < other.largest_uploaded_blob_bytes.get() {
            self.largest_uploaded_blob_bytes.add(
                other.largest_uploaded_blob_bytes.get() - self.largest_uploaded_blob_bytes.get(),
            );
        }
        self.uploaded_files.add(other.uploaded_files.get());
        self.already_present_files
            .add(other.already_present_files.get());
        self.redacted_files.add(other.redacted_files.get());
    }

    fn tick(&self, ctx: &CoreContext, outcome: UploadOutcome) {
        match outcome {
            UploadOutcome::Uploaded(size) => {
                let size = size as usize;
                self.uploaded.inc();
                self.uploaded_bytes.add(size);
                if self.largest_uploaded_blob_bytes.get() < size {
                    self.largest_uploaded_blob_bytes
                        .add(size - self.largest_uploaded_blob_bytes.get());
                }
            }
            UploadOutcome::AlreadyPresent => {
                self.already_present.inc();
            }
            UploadOutcome::Redacted => {}
        }
        self.maybe_log(ctx, DEBUG_LOG_INTERVAL);
    }

    pub fn tick_files(&self, ctx: &CoreContext, outcome: UploadOutcome) {
        self.tick(ctx, outcome);
        match outcome {
            UploadOutcome::Uploaded(_) => {
                self.uploaded_files.inc();
            }
            UploadOutcome::AlreadyPresent => {
                self.already_present_files.inc();
            }
            UploadOutcome::Redacted => {
                self.redacted_files.inc();
            }
        }
    }

    pub fn tick_trees(&self, ctx: &CoreContext, outcome: UploadOutcome) {
        self.tick(ctx, outcome);
    }

    pub fn maybe_log<'a>(&self, ctx: &'a CoreContext, limit: usize) {
        if self.sum().is_multiple_of(limit) {
            self.log(ctx);
        }
    }
    pub fn log<'a>(&self, _ctx: &'a CoreContext) {
        debug!("{}", *self);
    }

    pub fn uploaded_digests(&self) -> usize {
        self.uploaded.get()
    }

    pub fn uploaded_bytes(&self) -> usize {
        self.uploaded_bytes.get()
    }

    pub fn already_present_digests(&self) -> usize {
        self.already_present.get()
    }

    pub fn uploaded_files(&self) -> usize {
        self.uploaded_files.get()
    }

    pub fn uploaded_trees(&self) -> usize {
        self.uploaded.get() - self.uploaded_files.get()
    }

    pub fn already_present_files(&self) -> usize {
        self.already_present_files.get()
    }

    pub fn already_present_trees(&self) -> usize {
        self.already_present.get() - self.already_present_files.get()
    }

    pub fn redacted_files(&self) -> usize {
        self.redacted_files.get()
    }

    fn warn_skipped_redacted(&self, changeset_id: &ChangesetId) {
        let redacted_files = self.redacted_files.get();
        if redacted_files > 0 {
            warn!(
                "Skipped {} redacted files for changeset {}",
                redacted_files, changeset_id,
            );
        }
    }
}

define_stats! {
    prefix = "mononoke.cas_changesets_uploader";
    uploaded_changesets: timeseries(Rate, Sum),
    uploaded_changesets_recursive: timeseries(Rate, Sum),
}

pub trait Repo =
    BonsaiHgMappingRef + RepoBlobstoreArc + RepoIdentityRef + RestrictedPathsRef + Send + Sync;

pub struct CasChangesetsUploader<Client>
where
    Client: CasClient,
{
    pub client: ScmCasClient<Client>,
}

impl<Client> CasChangesetsUploader<Client>
where
    Client: CasClient,
{
    pub fn new(client: Client) -> Self {
        Self {
            client: ScmCasClient::new(client),
        }
    }

    async fn get_manifest_id_from_changeset<'a>(
        &self,
        ctx: &'a CoreContext,
        repo: &impl Repo,
        changeset_id: &ChangesetId,
    ) -> Result<(HgChangesetId, HgManifestId), CasChangesetUploaderError> {
        let hg_cs_id = repo
            .bonsai_hg_mapping()
            .get_hg_from_bonsai(ctx, *changeset_id)
            .await
            .map(|cs| {
                cs.ok_or(CasChangesetUploaderError::InvalidChangeset(
                    changeset_id.clone(),
                ))
            })??;
        let hg_manifest = hg_cs_id
            .load(ctx, &repo.repo_blobstore())
            .await
            .map_err(|e| CasChangesetUploaderError::Error(e.into()))?
            .manifestid();
        Ok((hg_cs_id, hg_manifest))
    }

    /// Upload a given Changeset to a CAS backend.
    /// The implementation assumes that if hg manifest is derived, then augmented manifest is also derived.
    ///
    /// Trees under restricted paths are never uploaded, nor are trees with a restriction root as
    /// a direct child (see `has_restricted_child`). Files are always uploaded.
    pub async fn upload_single_changeset<'a>(
        &self,
        ctx: &'a CoreContext,
        repo: &impl Repo,
        changeset_id: &ChangesetId,
        upload_policy: UploadPolicy,
        prior_lookup_policy: PriorLookupPolicy,
    ) -> Result<UploadStats, CasChangesetUploaderError> {
        let hg_cs_id = repo
            .bonsai_hg_mapping()
            .get_hg_from_bonsai(ctx, *changeset_id)
            .await
            .map(|cs| {
                cs.ok_or(CasChangesetUploaderError::InvalidChangeset(
                    changeset_id.clone(),
                ))
            })??;

        let blobstore = repo.repo_blobstore_arc();
        let hg_cs = hg_cs_id
            .load(ctx, &blobstore)
            .await
            .map_err(|e| CasChangesetUploaderError::Error(e.into()))?;

        let hg_root_manifest_id = HgAugmentedManifestId::new(hg_cs.manifestid().into_nodehash());

        // Diff hg manifest with parents
        let diff_stream = match (
            hg_cs.p1().map(HgChangesetId::new),
            hg_cs.p2().map(HgChangesetId::new),
        ) {
            // If there is a merge commit, there is no guarantee that the p2 parent is uploaded to CAS
            // (since the sync follows p1 chain)
            // Therefore, we need to diff fully with the p1 parent only.
            (Some(p), None) | (None, Some(p)) | (Some(p), Some(_)) => {
                let blobstore = repo.repo_blobstore();
                let parent_hg_cs = p.load(ctx, &blobstore);
                let hg_cs = hg_cs_id.load(ctx, &blobstore);
                let (parent_hg_cs, hg_cs) = try_join!(parent_hg_cs, hg_cs)
                    .map_err(|e| CasChangesetUploaderError::Error(e.into()))?;

                hg_manifest_diff(
                    ctx,
                    repo.repo_blobstore_arc(),
                    repo.repo_identity().name(),
                    parent_hg_cs.manifestid(),
                    hg_cs.manifestid(),
                )
                .map_ok(move |diff| match diff {
                    Diff::Added(path, entry) => Some((path, entry)),
                    Diff::Removed(..) => None,
                    Diff::Changed(path, _, entry) => Some((path, entry)),
                })
                .try_filter_map(future::ok)
                .map_err(CasChangesetUploaderError::DiffChangesetFailed)
                .boxed()
            }

            (None, None) => {
                let hg_cs = hg_cs_id
                    .load(ctx, &repo.repo_blobstore())
                    .await
                    .map_err(|e| CasChangesetUploaderError::Error(e.into()))?;
                list_all_hg_entries(
                    ctx,
                    repo.repo_blobstore_arc(),
                    repo.repo_identity().name(),
                    hg_cs.manifestid(),
                )
                .map_err(CasChangesetUploaderError::DiffChangesetFailed)
                .boxed()
            }
        }
        .try_collect::<Vec<_>>()
        .watched()
        .await?;

        // A merge can bring in trees identical to p2, which are therefore absent from
        // `file_changes` while the p1 diff above still carries them. Classify the paths
        // actually being uploaded so the two sets cannot diverge.
        let tree_paths: Vec<NonRootMPath> = diff_stream
            .iter()
            .filter(|(_, entry)| matches!(entry, Entry::Tree(_)))
            .filter_map(|(path, _)| path.clone().into_optional_non_root_path())
            .collect();

        let restricted_path_roots: Vec<NonRootMPath> =
            if tree_paths.is_empty() || !repo.restricted_paths().may_have_restricted_paths() {
                Vec::new()
            } else {
                repo.restricted_paths()
                    .get_path_restriction_info(ctx, Some(*changeset_id), &tree_paths)
                    .await?
                    .into_iter()
                    .map(|info| info.restriction_root)
                    .collect()
            };

        let total_before_filter = diff_stream.len();
        let diff_stream: Vec<_> = if restricted_path_roots.is_empty() {
            diff_stream
        } else {
            diff_stream
                .into_iter()
                .filter(|(path, entry)| {
                    !matches!(entry, Entry::Tree(_))
                        || !restricted_path_roots
                            .iter()
                            .any(|root| root.is_prefix_of(path))
                })
                .collect()
        };

        let mut manifests_list = diff_stream
            .iter()
            .filter_map(|(_, elem)| {
                if let Entry::Tree(treeid) = elem {
                    Some(treeid)
                } else {
                    None
                }
            })
            .map(|treeid| (HgAugmentedManifestId::new(treeid.into_nodehash()), None))
            .collect::<Vec<_>>();

        // The root manifest is checked even when unchanged, because UploadPolicy::All uploads it
        // regardless.
        let restriction_root_parents = if matches!(upload_policy, UploadPolicy::BlobsOnly) {
            HashSet::new()
        } else {
            let trees = manifests_list
                .iter()
                .map(|(treeid, _)| *treeid)
                .chain([hg_root_manifest_id])
                .collect();
            self.parents_of_restriction_roots(ctx, repo, trees).await?
        };
        manifests_list.retain(|(treeid, _)| !restriction_root_parents.contains(treeid));

        // No await between these, so the sync job's concurrent uploads keep each changeset's
        // lines together.
        if !restricted_path_roots.is_empty() {
            info!(
                "Found {} restricted path roots for changeset {}: {:?}",
                restricted_path_roots.len(),
                changeset_id,
                restricted_path_roots,
            );
        }
        if diff_stream.len() < total_before_filter {
            info!(
                "Filtered out {} of {} entries (trees under restricted paths) for changeset {}",
                total_before_filter - diff_stream.len(),
                total_before_filter,
                changeset_id,
            );
        }
        if !restriction_root_parents.is_empty() {
            info!(
                "Skipped {} parent trees of restriction roots for changeset {}",
                restriction_root_parents.len(),
                changeset_id,
            );
        }

        let files_list = diff_stream
            .iter()
            .filter_map(|(_, elem)| {
                if let Entry::Leaf(leaf) = elem {
                    Some(leaf.1)
                } else {
                    None
                }
            })
            .map(|fileid| (fileid, None))
            .collect::<Vec<_>>();

        let start_time = std::time::Instant::now();
        let blobstore = repo.repo_blobstore_arc();

        debug!(
            "Uploading data for changeset id: {}, hg changeset id: {}, number of manifests: {}, number of files: {}",
            changeset_id,
            hg_cs_id,
            manifests_list.len(),
            files_list.len(),
        );

        let upload_counter: Arc<UploadCounters> = Arc::new(Default::default());

        let blobs_lookup = prior_lookup_policy.enabled_blobs();
        let trees_lookup = prior_lookup_policy.enabled_trees();

        match upload_policy {
            UploadPolicy::BlobsOnly => {
                self.client
                    .ensure_upload_file_contents(ctx, &blobstore, files_list, blobs_lookup)
                    .watched()
                    .await?
                    .into_iter()
                    .for_each(|(_, outcome)| {
                        upload_counter.tick_files(ctx, outcome);
                    });
            }
            UploadPolicy::TreesOnly => {
                self.client
                    .ensure_upload_augmented_trees(ctx, &blobstore, manifests_list, trees_lookup)
                    .watched()
                    .await?
                    .into_iter()
                    .for_each(|(_, outcome)| {
                        upload_counter.tick_trees(ctx, outcome);
                    });
            }
            UploadPolicy::All => {
                // Ensure the root manifest is uploaded last, so that we can use it to derive if entire changeset is already present in CAS
                let (outcomes_trees, outcomes_files) = try_join!(
                    self.client
                        .ensure_upload_augmented_trees(
                            ctx,
                            &blobstore,
                            manifests_list
                                .into_iter()
                                .filter(|(treeid, _)| *treeid != hg_root_manifest_id),
                            trees_lookup
                        )
                        .watched(),
                    self.client
                        .ensure_upload_file_contents(ctx, &blobstore, files_list, blobs_lookup)
                        .watched()
                )?;

                outcomes_trees.into_iter().for_each(|(_, outcome)| {
                    upload_counter.tick_trees(ctx, outcome);
                });

                outcomes_files.into_iter().for_each(|(_, outcome)| {
                    upload_counter.tick_files(ctx, outcome);
                });

                // Upload the root manifest last
                if !restriction_root_parents.contains(&hg_root_manifest_id) {
                    let outcome_root = self
                        .client
                        .upload_augmented_tree(
                            ctx,
                            &blobstore,
                            &hg_root_manifest_id,
                            None,
                            trees_lookup,
                        )
                        .watched()
                        .await?;
                    upload_counter.tick_trees(ctx, outcome_root);
                }
            }
        }
        upload_counter.warn_skipped_redacted(changeset_id);

        debug!(
            "Upload of (bonsai) changeset {} to CAS took {} seconds, corresponding hg changeset is {}",
            changeset_id,
            start_time.elapsed().as_secs_f64(),
            hg_cs_id,
        );

        STATS::uploaded_changesets.add_value(1);
        Ok(upload_counter)
    }

    /// Upload a given Changeset to a CAS backend recursively.
    /// Upload can be limited to a specific path if provided.
    /// The implementation assumes that if hg manifest is derived, then augmented manifest is also derived.
    ///
    /// The walk reads augmented manifests, which carry the CAS digest of every child, so each
    /// directory costs one manifest load and one batched CAS lookup for all of its children.
    /// Content is fetched from the blobstore only for digests that CAS reports missing.
    ///
    /// Restriction root trees and trees with a restriction root as a direct child are never
    /// uploaded; everything else is. Each tree is checked with
    /// `RestrictedPaths::is_restricted_manifest`, so the sources selected by `AclManifestMode`
    /// decide. A single file given via `path` is uploaded regardless.
    pub async fn upload_single_changeset_recursively<'a>(
        &self,
        ctx: &'a CoreContext,
        repo: &impl Repo,
        changeset_id: &ChangesetId,
        path: Option<MPath>,
        upload_policy: UploadPolicy,
        prior_lookup_policy: PriorLookupPolicy,
    ) -> Result<UploadStats, CasChangesetUploaderError> {
        let start_time = std::time::Instant::now();
        let upload_counter: Arc<UploadCounters> = Arc::new(Default::default());
        let (hg_cs_id, hg_manifest_id) = self
            .get_manifest_id_from_changeset(ctx, repo, changeset_id)
            .await?;

        let hg_augmented_manifest_id: HgAugmentedManifestId =
            HgAugmentedManifestId::new(hg_manifest_id.into_nodehash());

        let max_concurrent_manifests = if matches!(upload_policy, UploadPolicy::TreesOnly) {
            MAX_CONCURRENT_MANIFESTS_TREES_ONLY
        } else {
            MAX_CONCURRENT_MANIFESTS
        };

        let blobs_lookup = prior_lookup_policy.enabled_blobs();
        let trees_lookup = prior_lookup_policy.enabled_trees();

        let mut start_manifest_id = hg_augmented_manifest_id;

        if let Some(ref path) = path {
            let entry = hg_augmented_manifest_id
                .find_entry(ctx.clone(), repo.repo_blobstore_arc(), path.clone())
                .await?;
            match entry {
                // Start the traversal below from this directory instead of the root.
                Some(Entry::Tree(treeid)) => {
                    start_manifest_id = treeid;
                }
                // Upload just this single file and exit.
                Some(Entry::Leaf(leaf)) => {
                    if !matches!(upload_policy, UploadPolicy::TreesOnly) {
                        let filenode_id = HgFileNodeId::new(leaf.filenode);
                        debug!(
                            "Upload single file '{}' for changeset id: {}, hg changeset id: {}",
                            path, changeset_id, hg_cs_id,
                        );
                        let outcome = self
                            .client
                            .upload_file_content(
                                ctx,
                                repo.repo_blobstore(),
                                &filenode_id,
                                Some(&MononokeDigest(leaf.content_blake3, leaf.total_size)),
                                blobs_lookup,
                            )
                            .await
                            .map_err(|error| {
                                CasChangesetUploaderError::FileUploadFailedWithFullPath(
                                    filenode_id,
                                    path.clone(),
                                    error,
                                )
                            })?;
                        upload_counter.tick_files(ctx, outcome);
                        upload_counter.warn_skipped_redacted(changeset_id);
                        debug!(
                            "Upload completed for '{}' in {:.2} seconds",
                            path,
                            start_time.elapsed().as_secs_f64()
                        );
                    }
                    return Ok(upload_counter);
                }
                None => {
                    return Err(CasChangesetUploaderError::PathNotFound(path.clone()));
                }
            }
        }

        let start_path = path.clone().unwrap_or(MPath::ROOT);
        debug!(
            "Uploading data recursively for [root augmented manifest: {}, changeset id: {}, hg changeset id: {}, repo path: '{}']",
            hg_augmented_manifest_id, changeset_id, hg_cs_id, &start_path,
        );

        let skipped_restricted_trees = RelaxedCounter::new(0);
        let skipped_parent_trees = RelaxedCounter::new(0);

        bounded_traversal::bounded_traversal_stream(
            max_concurrent_manifests,
            Some(start_manifest_id),
            |manifest_id: HgAugmentedManifestId| {
                cloned!(upload_counter);
                let upload_policy = upload_policy.clone();
                let skipped_restricted_trees = &skipped_restricted_trees;
                let skipped_parent_trees = &skipped_parent_trees;
                async move {
                    let upload_trees = !matches!(upload_policy, UploadPolicy::BlobsOnly);
                    let upload_files = !matches!(upload_policy, UploadPolicy::TreesOnly);

                    let manifest = load_preloaded_manifest(ctx, repo, manifest_id).await?;
                    let tree_digest = MononokeDigest(
                        manifest.augmented_manifest_id,
                        manifest.augmented_manifest_size,
                    );

                    // Only tree uploads are restricted, so BlobsOnly skips the checks.
                    let restricted = upload_trees
                        && self
                            .is_restricted_tree(ctx, repo, &manifest_id, manifest.is_restricted)
                            .await?;
                    let parent_of_restricted = upload_trees
                        && !restricted
                        && self.has_restricted_child(ctx, repo, &manifest).await?;
                    if restricted {
                        skipped_restricted_trees.inc();
                    } else if parent_of_restricted {
                        skipped_parent_trees.inc();
                    }

                    let mut files = Vec::new();
                    let mut children = Vec::new();
                    for (elem, entry) in manifest.children_augmented_metadata {
                        match entry {
                            HgAugmentedManifestEntry::FileNode(file) => {
                                if upload_files {
                                    let digest =
                                        MononokeDigest(file.content_blake3, file.total_size);
                                    files.push((elem, HgFileNodeId::new(file.filenode), digest));
                                }
                            }
                            HgAugmentedManifestEntry::DirectoryNode(dir) => {
                                children.push(HgAugmentedManifestId::new(dir.treenode));
                            }
                        }
                    }

                    // One batched lookup covers this directory's own tree and its files.
                    let upload_tree = upload_trees && !restricted && !parent_of_restricted;
                    let to_lookup: Vec<MononokeDigest> = (upload_tree && trees_lookup)
                        .then_some(tree_digest)
                        .into_iter()
                        .chain(
                            files
                                .iter()
                                .filter(|_| blobs_lookup)
                                .map(|(_, _, digest)| *digest),
                        )
                        .collect();
                    let missing = if to_lookup.is_empty() {
                        HashSet::new()
                    } else {
                        self.client.missing_digests(&to_lookup).await?
                    };

                    stream::iter(files)
                        .map(|(elem, filenode_id, digest)| {
                            cloned!(upload_counter);
                            let missing = &missing;
                            async move {
                                if blobs_lookup && !missing.contains(&digest) {
                                    upload_counter.tick_files(ctx, UploadOutcome::AlreadyPresent);
                                    return Ok(());
                                }
                                let outcome = self
                                    .client
                                    .upload_file_content(
                                        ctx,
                                        repo.repo_blobstore(),
                                        &filenode_id,
                                        Some(&digest),
                                        false,
                                    )
                                    .await
                                    .map_err(|error| {
                                        CasChangesetUploaderError::FileUploadFailed(
                                            filenode_id,
                                            elem,
                                            error,
                                        )
                                    })?;
                                upload_counter.tick_files(ctx, outcome);
                                anyhow::Ok(())
                            }
                        })
                        .buffer_unordered(MAX_CONCURRENT_FILES_PER_MANIFEST)
                        .try_collect::<()>()
                        .await?;

                    // Upload the tree after its files, so a tree present in CAS implies its files are too.
                    if upload_tree {
                        let outcome = if trees_lookup && !missing.contains(&tree_digest) {
                            UploadOutcome::AlreadyPresent
                        } else {
                            self.client
                                .upload_augmented_tree(
                                    ctx,
                                    repo.repo_blobstore(),
                                    &manifest_id,
                                    Some(&tree_digest),
                                    false,
                                )
                                .await
                                .map_err(|error| {
                                    CasChangesetUploaderError::TreeUploadFailed(manifest_id, error)
                                })?
                        };
                        upload_counter.tick_trees(ctx, outcome);
                    }

                    anyhow::Ok(((), children))
                }
                .boxed()
            },
        )
        .try_collect::<Vec<()>>()
        .await?;

        let skipped_restricted_trees = skipped_restricted_trees.get();
        if skipped_restricted_trees > 0 {
            info!(
                "Skipped {} restriction root trees for changeset {}",
                skipped_restricted_trees, changeset_id,
            );
        }
        let skipped_parent_trees = skipped_parent_trees.get();
        if skipped_parent_trees > 0 {
            info!(
                "Skipped {} parent trees of restriction roots for changeset {}",
                skipped_parent_trees, changeset_id,
            );
        }
        upload_counter.warn_skipped_redacted(changeset_id);

        upload_counter.log(ctx);
        debug!(
            "Upload of (bonsai) changeset {} to CAS (recursively) for path: '{}' took {:.2} seconds, corresponding hg changeset is {}. Upload included {}.",
            changeset_id,
            &start_path,
            start_time.elapsed().as_secs_f64(),
            hg_cs_id,
            match upload_policy {
                UploadPolicy::BlobsOnly => "blobs only",
                UploadPolicy::TreesOnly => "augmented trees only",
                UploadPolicy::All => "augmented trees and blobs",
            }
        );
        STATS::uploaded_changesets_recursive.add_value(1);
        Ok(upload_counter)
    }

    /// Whether this tree is a restriction root, according to the restriction sources the repo is
    /// configured with. `acl_manifest_is_restricted` is the AclManifest bit loaded with the tree.
    async fn is_restricted_tree<'a>(
        &self,
        ctx: &'a CoreContext,
        repo: &impl Repo,
        manifest_id: &HgAugmentedManifestId,
        acl_manifest_is_restricted: bool,
    ) -> Result<bool, CasChangesetUploaderError> {
        let restricted_paths = repo.restricted_paths();
        if !restricted_paths.may_have_restricted_paths() {
            return Ok(false);
        }
        Ok(restricted_paths
            .is_restricted_manifest(
                ctx,
                &RestrictedManifestId::new(manifest_id.as_bytes().into()),
                &ManifestType::HgAugmented,
                acl_manifest_is_restricted,
            )
            .await?)
    }

    /// Whether a directory child of this tree is a restriction root. SLAPI marks such a child
    /// with `has_acl`, which CAS trees cannot carry, so a client reading this tree from CAS would
    /// see the child as unrestricted.
    async fn has_restricted_child<'a>(
        &self,
        ctx: &'a CoreContext,
        repo: &impl Repo,
        manifest: &HgPreloadedAugmentedManifest,
    ) -> Result<bool, CasChangesetUploaderError> {
        let children: Vec<_> = manifest
            .children_augmented_metadata
            .iter()
            .filter_map(|(name, entry)| match entry {
                HgAugmentedManifestEntry::DirectoryNode(dir) => Some((
                    HgAugmentedManifestId::new(dir.treenode),
                    manifest
                        .child_restriction_map
                        .get(name)
                        .copied()
                        .unwrap_or(false),
                )),
                HgAugmentedManifestEntry::FileNode(_) => None,
            })
            .collect();
        let mut restricted_children = stream::iter(children)
            .map(|(child_id, acl_manifest_is_restricted)| async move {
                self.is_restricted_tree(ctx, repo, &child_id, acl_manifest_is_restricted)
                    .await
            })
            .buffer_unordered(MAX_CONCURRENT_RESTRICTION_CHECKS);
        while let Some(restricted) = restricted_children.try_next().await? {
            if restricted {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The trees among `trees` that have a restriction root as a direct child.
    async fn parents_of_restriction_roots<'a>(
        &self,
        ctx: &'a CoreContext,
        repo: &impl Repo,
        trees: HashSet<HgAugmentedManifestId>,
    ) -> Result<HashSet<HgAugmentedManifestId>, CasChangesetUploaderError> {
        if !repo.restricted_paths().may_have_restricted_paths() {
            return Ok(HashSet::new());
        }
        stream::iter(trees)
            .map(|tree_id| async move {
                let manifest = load_preloaded_manifest(ctx, repo, tree_id).await?;
                let is_parent = self.has_restricted_child(ctx, repo, &manifest).await?;
                Ok::<_, CasChangesetUploaderError>(is_parent.then_some(tree_id))
            })
            .buffer_unordered(MAX_CONCURRENT_MANIFESTS)
            .try_filter_map(future::ok)
            .try_collect()
            .await
    }

    /// Check if a given Changeset is already uploaded to a CAS backend by validating the presence of the hg root augmented manifest.
    /// This is not full scan check but the best approximation we can do.
    /// The lookup shouldn't be relied on if a changeset was uploaded with TreeOnly policy, the mode that isn't used in production.
    /// It is always false when a top-level directory is a restriction root, as the root manifest
    /// is then never uploaded.
    pub async fn is_changeset_uploaded<'a>(
        &self,
        ctx: &'a CoreContext,
        repo: &impl Repo,
        changeset_id: &ChangesetId,
    ) -> Result<bool, CasChangesetUploaderError> {
        let (_, hg_root_manifest_id) = self
            .get_manifest_id_from_changeset(ctx, repo, changeset_id)
            .await?;

        let hg_root_augmented_manifest_id: HgAugmentedManifestId =
            HgAugmentedManifestId::new(hg_root_manifest_id.into_nodehash());

        self.client
            .is_augmented_tree_uploaded(ctx, repo.repo_blobstore(), &hg_root_augmented_manifest_id)
            .await
            .map_err(CasChangesetUploaderError::Error)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use bonsai_hg_mapping::BonsaiHgMapping;
    use bookmarks::Bookmarks;
    use cas_client::DummyCasClient;
    use commit_graph::CommitGraph;
    use commit_graph::CommitGraphWriter;
    use derivation_queue_thrift::DerivationPriority;
    use fbinit::FacebookInit;
    use filestore::FilestoreConfig;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs_async;
    use maplit::hashmap;
    use memblob::Memblob;
    use mercurial_derivation::DeriveHgChangeset;
    use mercurial_derivation::RootHgAugmentedManifestId;
    use mononoke_macros::mononoke;
    use repo_blobstore::RepoBlobstore;
    use repo_derived_data::RepoDerivedData;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_identity::RepoIdentity;
    use restricted_paths::RestrictedPaths;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::CreateCommitContext;
    use tests_utils::hg_manifest_reads::HgManifestReadBlocker;

    use super::*;

    #[facet::container]
    struct TestRepo(
        dyn BonsaiHgMapping,
        dyn Bookmarks,
        CommitGraph,
        dyn CommitGraphWriter,
        FilestoreConfig,
        RepoBlobstore,
        RepoDerivedData,
        RepoIdentity,
        RestrictedPaths,
    );

    /// Hg changesets are derived without their augmented manifests, so that
    /// deriving one never reads Hg manifests.
    fn knobs(read_from_augmented: bool) -> JustKnobsInMemory {
        JustKnobsInMemory::new(hashmap! {
            "scm/mononoke:hgmanifest_read_from_augmented".to_string() => KnobVal::Bool(read_from_augmented),
            "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset".to_string() => KnobVal::Bool(false),
        })
    }

    struct Fixture {
        repo: TestRepo,
        blocker: Arc<HgManifestReadBlocker>,
        root: ChangesetId,
        child: ChangesetId,
    }

    /// A root commit and a child that changes a file and adds a directory,
    /// with hg changesets but no augmented manifests. Hg manifest reads fail
    /// once the blocker is armed.
    async fn fixture(ctx: &CoreContext) -> Result<Fixture> {
        let (blobstore, blocker) = HgManifestReadBlocker::wrap(Memblob::default());
        let repo: TestRepo = TestRepoFactory::new(ctx.fb)?
            .with_blobstore(blobstore)
            .build()
            .await?;
        let root = CreateCommitContext::new_root(ctx, &repo)
            .add_file("a", "a")
            .add_file("dir/b", "b")
            .commit()
            .await?;
        let child = CreateCommitContext::new(ctx, &repo, vec![root])
            .add_file("dir/b", "b2")
            .add_file("new/c", "c")
            .commit()
            .await?;
        // Derivation would also derive augmented manifests, so store the Hg
        // changesets as an upload does.
        fixtures::store_hg_changesets_as_uploaded(ctx, &repo, vec![root, child]).await?;
        for cs_id in [root, child] {
            with_just_knobs_async(knobs(false), repo.derive_hg_changeset(ctx, cs_id).boxed())
                .await?;
        }
        Ok(Fixture {
            repo,
            blocker,
            root,
            child,
        })
    }

    /// The upload counts of the child, which is diffed with its parent, and of
    /// the root, which has no parent so all its entries are listed. Each goes
    /// into an empty CAS store.
    async fn uploads(ctx: &CoreContext, f: &Fixture) -> Result<Vec<String>> {
        let mut uploads = Vec::new();
        for cs_id in [f.child, f.root] {
            let store = tempfile::tempdir()?;
            let uploader = CasChangesetsUploader::new(DummyCasClient::new_with_storage_path(
                ctx.clone(),
                "repo",
                store.path(),
            )?);
            let stats = uploader
                .upload_single_changeset(
                    ctx,
                    &f.repo,
                    &cs_id,
                    UploadPolicy::All,
                    PriorLookupPolicy::All,
                )
                .await?;
            uploads.push(stats.to_string());
        }
        Ok(uploads)
    }

    #[mononoke::fbinit_test]
    async fn test_uploads_read_from_augmented(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let f = fixture(&ctx).await?;
        for cs_id in [f.root, f.child] {
            f.repo
                .repo_derived_data()
                .derive::<RootHgAugmentedManifestId>(&ctx, cs_id, DerivationPriority::LOW)
                .await?;
        }

        // Given: the uploads when reading Hg manifests.
        let from_hg = with_just_knobs_async(knobs(false), uploads(&ctx, &f).boxed()).await?;

        // When: Hg manifests can no longer be read.
        f.blocker.arm();

        // Then: reading augmented manifests uploads the same.
        let from_augmented = with_just_knobs_async(knobs(true), uploads(&ctx, &f).boxed()).await?;
        assert_eq!(from_augmented, from_hg);
        assert_eq!(
            f.blocker.reads(),
            0,
            "no Hg manifest is read with the knob on"
        );

        // And: with the knob off, the Hg manifests are still read.
        let knob_off = with_just_knobs_async(knobs(false), uploads(&ctx, &f).boxed()).await;
        assert!(
            knob_off.is_err(),
            "the knob off reads the blocked Hg manifests"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_uploads_missing_augmented(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let f = fixture(&ctx).await?;
        f.blocker.arm();

        let err = with_just_knobs_async(knobs(true), uploads(&ctx, &f).boxed())
            .await
            .expect_err("a missing augmented manifest is an error");
        assert!(
            format!("{err:#}").contains("hgaugmentedmanifest.sha1."),
            "the error names the missing augmented manifest: {err:#}"
        );
        assert_eq!(
            f.blocker.reads(),
            0,
            "there is no fallback to the Hg manifest"
        );
        Ok(())
    }
}
