/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::time::Instant;

use anyhow::Context;
use anyhow::Error;
use async_trait::async_trait;
use blobstore::BlobstoreIsPresent;
use blobstore::KeyedBlobstore;
use bytes::Bytes;
use cloned::cloned;
use context::PerfCounterType;
use edenapi_types::AnyId;
use edenapi_types::Batch;
use edenapi_types::CheckManifestPermissionRequest;
use edenapi_types::CheckManifestPermissionResponse;
use edenapi_types::CheckPathPermissionAclEntry;
use edenapi_types::CheckPathPermissionData;
use edenapi_types::CheckPathPermissionRequest;
use edenapi_types::CheckPathPermissionResponse;
use edenapi_types::FileAuxData;
use edenapi_types::SaplingRemoteApiServerError;
use edenapi_types::SaplingRemoteApiServerErrorKind;
use edenapi_types::ServerError;
use edenapi_types::TreeAttributes;
use edenapi_types::TreeAuxData;
use edenapi_types::TreeChildEntry;
use edenapi_types::TreeEntry;
use edenapi_types::TreeRequest;
use edenapi_types::UploadToken;
use edenapi_types::UploadTreeRequest;
use edenapi_types::UploadTreeResponse;
use edenapi_types::wire::WireTreeRequest;
use futures::Future;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use gotham::state::FromState;
use gotham::state::State;
use gotham_derive::StateData;
use gotham_derive::StaticResponseExtender;
use gotham_ext::error::HttpError;
use gotham_ext::handler::SlapiCommitIdentityScheme;
use gotham_ext::middleware::request_context::RequestContext;
use gotham_ext::middleware::scuba::ScubaMiddlewareState;
use gotham_ext::response::TryIntoResponse;
use manifest::Entry;
use manifest::Manifest;
use mercurial_types::HgAugmentedManifestEntry;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestEnvelope;
use mercurial_types::HgManifestId;
use mercurial_types::HgNodeHash;
use mononoke_api::MononokeRepo;
use mononoke_api::Repo;
use mononoke_api::errors::MononokeError;
use mononoke_api_hg::DirectoryAcl;
use mononoke_api_hg::HgAugmentedTreeRestrictionContext;
use mononoke_api_hg::HgDataContext;
use mononoke_api_hg::HgDataId;
use mononoke_api_hg::HgRepoContext;
use mononoke_api_hg::HgTreeContext;
use mononoke_api_hg::UploadTreeAugmented;
use mononoke_api_hg::UploadTreeBuildError;
use mononoke_types::MPath;
use mononoke_types::MPathElement;
use permission_checker::MononokeIdentitySetExt;
use rate_limiting::Metric;
use rate_limiting::Scope;
use repo_blobstore::RepoBlobstoreRef;
use repo_identity::RepoIdentityRef;
use serde::Deserialize;
use stats::define_stats;
use stats::prelude::DynamicHistogram;
use stats::prelude::TimeseriesStatic;
use types::Key;
use types::RepoPathBuf;

use super::HandlerInfo;
use super::HandlerResult;
use super::RequestBatchSize;
use super::SaplingRemoteApiHandler;
use super::SaplingRemoteApiMethod;
use super::handler::SaplingRemoteApiContext;
use crate::context::ServerContext;
use crate::errors::SaplingRemoteApiServiceError;
use crate::handlers::git_objects::fetch_git_object;
use crate::middleware::request_dumper::RequestDumper;
use crate::utils::custom_cbor_stream;
use crate::utils::get_repo;
use crate::utils::parse_wire_request;
use crate::utils::to_hg_path_nonroot;

define_stats! {
    prefix = "mononoke.trees";
    manifests_served: timeseries(Rate, Sum),
    trees_batch_keys_requested: timeseries(Rate, Sum),
    upload_augmented_manifests_attempted: timeseries(Rate, Sum),
    upload_augmented_manifests_batches_attempted: timeseries(Rate, Sum),
    upload_augmented_manifests_batches_built: timeseries(Rate, Sum),
    upload_augmented_manifests_built: timeseries(Rate, Sum),
    upload_augmented_manifests_skipped_no_acl_work: timeseries(Rate, Sum),
    upload_augmented_manifests_failed: timeseries(Rate, Sum),
    upload_augmented_manifests_failed_missing_child: timeseries(Rate, Sum),
    upload_augmented_manifests_failed_child_not_derived: timeseries(Rate, Sum),
    upload_augmented_manifests_build_ms: dynamic_histogram(
        "{}.upload_augmented_manifests_build_ms", (repo: String);
        20, 0, 10_000, Average, Count; P 50; P 90; P 99),
    upload_augmented_manifests_batch_trees: dynamic_histogram(
        "{}.upload_augmented_manifests_batch_trees", (repo: String);
        10, 0, 1_000, Average, Count; P 50; P 90; P 99),
    upload_augmented_manifests_batch_levels: dynamic_histogram(
        "{}.upload_augmented_manifests_batch_levels", (repo: String);
        1, 0, 50, Average, Count; P 50; P 90; P 99),
}

// The size is optimized for the batching settings in EdenFs.
const MAX_CONCURRENT_TREE_FETCHES_PER_REQUEST: usize = 128;
const MAX_CONCURRENT_METADATA_FETCHES_PER_TREE_FETCH: usize = 100;
const MAX_CONCURRENT_UPLOAD_TREES_PER_REQUEST: usize = 100;
const LARGE_TREE_METADATA_LIMIT: usize = 25000;

const ROUTE_ORIGINAL_TO_AUGMENTED_HG_MANIFEST: &str =
    "scm/mononoke:route_original_to_augmented_hg_manifest";
const BUILD_AUGMENTED_MANIFESTS_AT_TREE_UPLOAD: &str =
    "scm/mononoke:build_augmented_manifests_at_tree_upload";

#[derive(Debug, Deserialize, StateData, StaticResponseExtender)]
pub struct TreeParams {
    repo: String,
}

/// Fetch the tree nodes requested by the client.
pub async fn trees(state: &mut State) -> Result<impl TryIntoResponse + use<>, HttpError> {
    let params = TreeParams::take_from(state);

    state.put(HandlerInfo::new(
        &params.repo,
        SaplingRemoteApiMethod::Trees,
    ));

    let rctx = RequestContext::borrow_from(state).clone();
    let sctx = ServerContext::borrow_from(state);
    let slapi_flavour = SlapiCommitIdentityScheme::borrow_from(state).clone();
    let repo: HgRepoContext<Repo> =
        get_repo(sctx, &rctx, &params.repo, Metric::TotalManifests).await?;
    let request = parse_wire_request::<WireTreeRequest>(state).await?;
    state.put(RequestBatchSize(request.keys.len() as f64));
    if let Some(rd) = RequestDumper::try_borrow_mut_from(state) {
        rd.add_request(&request);
    };

    ScubaMiddlewareState::try_set_sampling_rate(state, nonzero_ext::nonzero!(1024_u64));

    Ok(custom_cbor_stream(
        super::monitor_request(state, fetch_all_trees(repo, request, slapi_flavour)),
        |tree_entry| tree_entry.as_ref().err(),
    ))
}

/// Fetch trees for all of the requested keys concurrently.
fn fetch_all_trees<R: MononokeRepo>(
    repo: HgRepoContext<R>,
    request: TreeRequest,
    flavour: SlapiCommitIdentityScheme,
) -> impl Stream<Item = Result<TreeEntry, SaplingRemoteApiServerError>> {
    let ctx = repo.ctx().clone();

    STATS::trees_batch_keys_requested.add_value(request.keys.len() as i64);

    let fetches = request.keys.into_iter().map(move |key| match flavour {
        SlapiCommitIdentityScheme::Git => fetch_git_object_as_tree(key.clone(), repo.clone())
            .map(|r| r.map_err(|e| SaplingRemoteApiServerError::with_key(key, e)))
            .left_future(),
        SlapiCommitIdentityScheme::Hg => fetch_tree(repo.clone(), key.clone(), request.attributes)
            .map(|r| r.map_err(|e| tree_fetch_error_to_slapi_error(key, e)))
            .right_future(),
    });

    stream::iter(fetches)
        .buffer_unordered(MAX_CONCURRENT_TREE_FETCHES_PER_REQUEST)
        .inspect_ok(move |_| {
            ctx.session()
                .bump_load(Metric::TotalManifests, Scope::Regional, 1.0);
            STATS::manifests_served.add_value(1);
        })
}

fn tree_fetch_error_to_slapi_error(key: Key, err: Error) -> SaplingRemoteApiServerError {
    let manifest_denial =
        err.chain()
            .find_map(|cause| match cause.downcast_ref::<MononokeError>() {
                Some(MononokeError::RestrictedPathsAuthorizationError(err))
                    if err.is_manifest_access() =>
                {
                    Some((
                        err.permission_request_group().to_string(),
                        err.denial_message().map(str::to_string),
                    ))
                }
                _ => None,
            });

    if let Some((permission_request_group, denial_message)) = manifest_denial {
        SaplingRemoteApiServerError {
            err: SaplingRemoteApiServerErrorKind::PermissionDenied {
                tree_id: key.hgid,
                request_acl: permission_request_group,
                denial_message,
            },
            key: Some(key),
        }
    } else {
        SaplingRemoteApiServerError::with_key(key, err)
    }
}

// Sapling wants to use trees the same way for Hg and Git, so shaping somehow
// the git object to fit within the defined TreeEntry structure.
async fn fetch_git_object_as_tree<R: MononokeRepo>(
    key: Key,
    repo: HgRepoContext<R>,
) -> Result<TreeEntry, Error> {
    let git_object = fetch_git_object(key.hgid, &repo).await;

    Ok(TreeEntry {
        key: key.clone(),
        data: git_object.ok().map(|o| o.bytes.into()),
        parents: None,
        children: None,
        tree_aux_data: None,
        // Path ACLs are not supported in Git
        has_acl: None,
    })
}

/// Fetch requested tree for a single key.
/// Note that this function consumes the repo context in order
/// to construct a tree context for the requested blob.
async fn fetch_tree<R: MononokeRepo>(
    repo: HgRepoContext<R>,
    key: Key,
    attributes: TreeAttributes,
) -> Result<TreeEntry, Error> {
    let mut entry = TreeEntry::new(key.clone());
    let route_to_augmented = justknobs::eval(
        ROUTE_ORIGINAL_TO_AUGMENTED_HG_MANIFEST,
        repo.ctx()
            .metadata()
            .client_request_info()
            .map(|c| c.correlator.as_str()),
        Some(repo.repo().repo_identity().name()),
    );

    // The augmented manifest's content is byte-identical to the original Hg
    // manifest blob, so serving it for an original request still hash-verifies
    // on the client.
    if route_to_augmented || attributes.augmented_trees {
        let id = HgAugmentedManifestId::new(HgNodeHash::from(key.hgid));
        let perf_counter = if attributes.augmented_trees {
            PerfCounterType::EdenapiAugmentedTrees
        } else {
            PerfCounterType::EdenapiOriginalRoutedToAugmented
        };
        repo.ctx().perf_counters().increment_counter(perf_counter);

        let maybe_ctx = id
            .context(repo.clone())
            .await
            .with_context(|| SaplingRemoteApiServiceError::TreeFetchFailed(key.clone()))?;

        if let Some(ctx) = maybe_ctx {
            let populate_all_metadata = route_to_augmented;

            entry.with_tree_aux_data(TreeAuxData {
                augmented_manifest_id: ctx.augmented_manifest_id().clone().into(),
                augmented_manifest_size: ctx.augmented_manifest_size(),
            });
            entry.with_has_acl(ctx.is_restricted().await?);

            if attributes.parents || populate_all_metadata {
                entry.with_parents(Some(ctx.hg_parents().into()));
            }

            if attributes.child_metadata || populate_all_metadata {
                repo.ctx()
                    .perf_counters()
                    .increment_counter(PerfCounterType::EdenapiTreesAuxData);

                let child_restrictions = ctx
                    .children_restrictions(MAX_CONCURRENT_METADATA_FETCHES_PER_TREE_FETCH)
                    .await?;
                let children = ctx
                    .augmented_children_entries()
                    .map(|(path, augmented_entry)| {
                        fetch_augmented_child_metadata(
                            &key,
                            path,
                            augmented_entry,
                            child_restrictions.get(path).copied().unwrap_or(false),
                        )
                    })
                    .collect();

                entry.with_children(Some(children));
            }

            if attributes.manifest_blob {
                let (data, _) = ctx
                    .content()
                    .await
                    .with_context(|| SaplingRemoteApiServiceError::TreeFetchFailed(key.clone()))?;

                entry.with_data(Some(data.into()));
            }

            return Ok(entry);
        } else if route_to_augmented {
            // Fail closed rather than falling back to the unprotected original
            // manifest. The augmented manifest cannot be derived on demand here
            // (derivation is per-changeset and needs the ACL overlay, but the
            // trees endpoint only has a manifest id), so a missing augmented
            // manifest must be backfilled out of band.
            repo.ctx()
                .perf_counters()
                .increment_counter(PerfCounterType::EdenapiAugmentedManifestMissUnderRoute);
            let repo_name = repo.repo().repo_identity().name();
            // Log the specific manifest id that needs backfilling so a miss is
            // actionable from scuba (the aggregate counter has no per-key id).
            repo.ctx()
                .scuba()
                .clone()
                .add("repo", repo_name)
                .add("hg_manifest_id", key.hgid.to_string())
                .log_with_msg(
                    "Augmented Hg manifest unavailable (route_original_to_augmented_hg_manifest)",
                    None,
                );
            return Err(MononokeError::NotAvailable(format!(
                "augmented Hg manifest unavailable for tented repo {repo_name}; \
                 the original (non-augmented) manifest is disabled"
            ))
            .into());
        } else {
            // If we don't have an augmented tree, fallback to the old way of fetching trees
            // Log the fallback to scuba
            repo.ctx()
                .perf_counters()
                .increment_counter(PerfCounterType::EdenapiAugmentedTreesFallback);
        }
    }

    let id = <HgManifestId as HgDataId<R>>::from_node_hash(HgNodeHash::from(key.hgid));

    let ctx = id
        .context(repo.clone())
        .await
        .with_context(|| SaplingRemoteApiServiceError::TreeFetchFailed(key.clone()))?
        .with_context(|| SaplingRemoteApiServiceError::KeyDoesNotExist(key.clone()))?;

    if attributes.manifest_blob {
        repo.ctx()
            .perf_counters()
            .increment_counter(PerfCounterType::EdenapiTrees);

        let (data, _) = ctx
            .content()
            .await
            .with_context(|| SaplingRemoteApiServiceError::TreeFetchFailed(key.clone()))?;

        entry.with_data(Some(data.into()));
    }

    if attributes.parents {
        entry.with_parents(Some(ctx.hg_parents().into()));
    }

    if attributes.child_metadata {
        repo.ctx()
            .perf_counters()
            .increment_counter(PerfCounterType::EdenapiTreesAuxData);

        if let Some(entries) = fetch_child_file_metadata_entries(&repo, &ctx).await? {
            let children: Vec<Result<TreeChildEntry, SaplingRemoteApiServerError>> = entries
                .buffer_unordered(MAX_CONCURRENT_METADATA_FETCHES_PER_TREE_FETCH)
                .map(|r| r.map_err(|e| SaplingRemoteApiServerError::with_key(key.clone(), e)))
                .collect()
                .await;

            entry.with_children(Some(children));
        }
    }

    Ok(entry)
}

/// Builds child metadata from preloaded augmented manifest entries.
fn fetch_augmented_child_metadata(
    key: &Key,
    path: &MPathElement,
    augmented_entry: &HgAugmentedManifestEntry,
    directory_has_acl: bool,
) -> Result<TreeChildEntry, SaplingRemoteApiServerError> {
    match augmented_entry {
        HgAugmentedManifestEntry::FileNode(file) => Ok(TreeChildEntry::new_file_entry(
            Key {
                hgid: file.filenode.into(),
                path: RepoPathBuf::from_string(path.to_string())
                    .map_err(|e| SaplingRemoteApiServerError::with_key(key.clone(), e))?,
            },
            FileAuxData {
                blake3: file.content_blake3.clone().into(),
                sha1: file.content_sha1.clone().into(),
                total_size: file.total_size.clone(),
                file_header_metadata: Some(
                    file.file_header_metadata
                        .clone()
                        .unwrap_or(Bytes::new())
                        .into(),
                ),
            }
            .into(),
        )),
        HgAugmentedManifestEntry::DirectoryNode(tree) => Ok(TreeChildEntry::new_directory_entry(
            Key {
                hgid: tree.treenode.into(),
                path: RepoPathBuf::from_string(path.to_string())
                    .map_err(|e| SaplingRemoteApiServerError::with_key(key.clone(), e))?,
            },
            TreeAuxData {
                augmented_manifest_id: tree.augmented_manifest_id.clone().into(),
                augmented_manifest_size: tree.augmented_manifest_size.clone(),
            },
            Some(directory_has_acl),
        )),
    }
}

async fn fetch_child_file_metadata_entries<'a, R: MononokeRepo>(
    repo: &'a HgRepoContext<R>,
    ctx: &'a HgTreeContext<R>,
) -> Result<
    Option<impl Stream<Item = impl Future<Output = Result<TreeChildEntry, Error>> + 'a> + 'a>,
    Error,
> {
    let manifest = ctx.clone().into_blob_manifest()?;
    if manifest.content().files.len() > LARGE_TREE_METADATA_LIMIT {
        return Ok(None);
    }
    let file_entries = manifest
        .list(repo.ctx(), repo.repo().repo_blobstore())
        .await?
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .filter_map(|(name, entry)| {
            if let Entry::Leaf((_, child_id)) = entry {
                Some((name, child_id))
            } else {
                None
            }
        });

    Ok(Some(
        stream::iter(file_entries)
            // .entries iterator is not `Send`
            .map({
                move |(name, child_id)| async move {
                    let name = RepoPathBuf::from_string(name.to_string())?;
                    let child_key = Key::new(name, child_id.into_nodehash().into());
                    fetch_child_file_metadata(repo, child_key.clone()).await
                }
            }),
    ))
}

async fn fetch_child_file_metadata<R: MononokeRepo>(
    repo: &HgRepoContext<R>,
    child_key: Key,
) -> Result<TreeChildEntry, Error> {
    let ctx = repo
        .file(HgFileNodeId::new(child_key.hgid.into()))
        .await?
        .ok_or_else(|| SaplingRemoteApiServiceError::FileFetchFailed(child_key.clone()))?;

    let metadata = ctx.content_metadata().await?;
    Ok(TreeChildEntry::new_file_entry(
        child_key,
        FileAuxData {
            total_size: metadata.total_size,
            sha1: metadata.sha1.into(),
            blake3: metadata.seeded_blake3.into(),
            file_header_metadata: Some(ctx.file_header_metadata().into()),
        }
        .into(),
    ))
}

/// Store the content of a single tree, returning its token and its envelope.
async fn store_tree<R: MononokeRepo>(
    repo: HgRepoContext<R>,
    item: UploadTreeRequest,
) -> Result<(UploadTreeResponse, HgManifestEnvelope), Error> {
    let (token, envelope) = verify_tree(&repo, item).await?;
    repo.store_verified_tree(envelope.clone()).await?;
    Ok((token, envelope))
}

/// Check a single tree against its node id, returning its token and the
/// envelope to store, without storing anything.
async fn verify_tree<R: MononokeRepo>(
    repo: &HgRepoContext<R>,
    item: UploadTreeRequest,
) -> Result<(UploadTreeResponse, HgManifestEnvelope), Error> {
    let upload_node_id = HgNodeHash::from(item.entry.node_id);
    let contents = Bytes::from(item.entry.data);
    let p1 = item.entry.parents.p1().cloned().map(HgNodeHash::from);
    let p2 = item.entry.parents.p2().cloned().map(HgNodeHash::from);
    let computed_node_id = item.entry.computed_node_id.map(HgNodeHash::from);
    let envelope = repo
        .verify_tree(upload_node_id, p1, p2, contents, computed_node_id)
        .await?;
    Ok((
        UploadTreeResponse {
            token: UploadToken::new_fake_token(AnyId::HgTreeId(item.entry.node_id), None),
        },
        envelope,
    ))
}

/// Why a batch did not build, as counted in ODS and logged as `failure_kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuildFailureKind {
    /// The client's fault: a child the batch names is neither in it nor
    /// stored, or the batch could not be ordered.
    MissingChild,
    /// The child tree is stored, typically by a concurrent upload of the same
    /// commit whose changeset has not arrived yet, but its augmented manifest
    /// is not derived. Expected, and not the client's fault.
    ChildNotDerived,
    Server,
}

impl BuildFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::MissingChild => "missing_child",
            Self::ChildNotDerived => "child_not_derived",
            Self::Server => "server",
        }
    }
}

fn missing_child(err: &Error) -> Option<&UploadTreeBuildError> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<UploadTreeBuildError>())
}

/// Only a complete batch can tell the client's mistake from our own, and only
/// the store can tell a child that was never sent from one that is merely not
/// derived yet.
async fn classify_build_failure<R: MononokeRepo>(
    repo: &HgRepoContext<R>,
    err: &Error,
    batch: &HashSet<HgNodeHash>,
    complete_batch: bool,
) -> BuildFailureKind {
    let Some(UploadTreeBuildError::MissingChild { child, .. }) = missing_child(err) else {
        return BuildFailureKind::Server;
    };
    if !complete_batch {
        return BuildFailureKind::Server;
    }
    if batch.contains(child) {
        return BuildFailureKind::MissingChild;
    }
    let key = HgManifestId::new(*child).blobstore_key();
    match repo
        .repo()
        .repo_blobstore()
        .is_present(repo.ctx(), &key)
        .await
    {
        Ok(BlobstoreIsPresent::Present) => BuildFailureKind::ChildNotDerived,
        _ => BuildFailureKind::MissingChild,
    }
}

/// Bump the counters for one batch of augmented-manifest builds.
///
/// A batch builds completely or fails, so the only thing that varies per tree
/// is whether it provably had no ACL work to do.
fn record_augmented_manifest_outcomes(built: &[UploadTreeAugmented]) {
    STATS::upload_augmented_manifests_built.add_value(built.len() as i64);
    let no_acl_work = built
        .iter()
        .filter(|tree| matches!(tree.acl, DirectoryAcl::NotNeeded))
        .count();
    STATS::upload_augmented_manifests_skipped_no_acl_work.add_value(no_acl_work as i64);
}

/// Build augmented manifests for the trees one request uploads.
///
/// Runs while the trees themselves are stored, and a failure here must not
/// fail the upload.
async fn build_and_record_augmented_manifests<R: MononokeRepo>(
    repo: &HgRepoContext<R>,
    trees: Vec<HgManifestEnvelope>,
    batch_len: usize,
) {
    // A tree that failed verification is dropped here, so a parent still in
    // the batch can look like it is missing a child the client did send. Only
    // a complete batch can tell the client's mistake from our own.
    let complete_batch = trees.len() == batch_len;
    let batch: HashSet<HgNodeHash> = trees.iter().map(|tree| tree.node_id()).collect();
    let repo_name = repo.repo().repo_identity().name().to_string();
    STATS::upload_augmented_manifests_attempted.add_value(trees.len() as i64);
    STATS::upload_augmented_manifests_batches_attempted.add_value(1);
    repo.ctx().perf_counters().add_to_counter(
        PerfCounterType::EdenapiUploadTreesAugmentedTrees,
        trees.len() as i64,
    );
    STATS::upload_augmented_manifests_batch_trees
        .add_value(trees.len() as i64, (repo_name.clone(),));
    // Runs before the response stream ends, alongside storing the trees. With
    // the store knob on it includes writing the envelopes, which the metric
    // name predates.
    let started = Instant::now();
    let result = repo
        .build_and_store_augmented_manifests_for_uploaded_trees(trees)
        .await;
    let elapsed = started.elapsed();
    STATS::upload_augmented_manifests_build_ms
        .add_value(elapsed.as_millis() as i64, (repo_name.clone(),));
    // The per-repo histogram is sparse per task, so averaging it across tasks
    // reads far too low; the request row carries the exact figure.
    repo.ctx()
        .perf_counters()
        .add_duration_us(PerfCounterType::EdenapiUploadTreesAugmentedBuildUs, elapsed);
    match result {
        Ok(built) => {
            record_augmented_manifest_outcomes(&built);
            STATS::upload_augmented_manifests_batches_built.add_value(1);
            // A failure counter that never moved is never exported, and a
            // monitor on it then reads no data rather than zero.
            STATS::upload_augmented_manifests_failed.add_value(0);
            STATS::upload_augmented_manifests_failed_missing_child.add_value(0);
            STATS::upload_augmented_manifests_failed_child_not_derived.add_value(0);
            let levels = built.iter().map(|tree| tree.level + 1).max().unwrap_or(0);
            STATS::upload_augmented_manifests_batch_levels.add_value(levels as i64, (repo_name,));
        }
        Err(err) => {
            let failure_kind = classify_build_failure(repo, &err, &batch, complete_batch).await;
            match failure_kind {
                BuildFailureKind::MissingChild => {
                    STATS::upload_augmented_manifests_failed_missing_child.add_value(1)
                }
                BuildFailureKind::ChildNotDerived => {
                    STATS::upload_augmented_manifests_failed_child_not_derived.add_value(1)
                }
                BuildFailureKind::Server => STATS::upload_augmented_manifests_failed.add_value(1),
            }
            repo.ctx()
                .scuba()
                .clone()
                .add("repo", repo.repo().repo_identity().name())
                .add("failure_kind", failure_kind.as_str())
                .log_with_msg(
                    "Failed to build augmented Hg manifests at tree upload",
                    Some(format!("{err:#}")),
                );
        }
    }
}

/// Upload list of trees requested by the client (batch request).
pub struct UploadTreesHandler;

#[async_trait]
impl SaplingRemoteApiHandler for UploadTreesHandler {
    type Request = Batch<UploadTreeRequest>;
    type Response = UploadTreeResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::UploadTrees;
    const ENDPOINT: &'static str = "/upload/trees";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        let build_augmented_manifests = justknobs::eval(
            BUILD_AUGMENTED_MANIFESTS_AT_TREE_UPLOAD,
            repo.ctx()
                .metadata()
                .client_request_info()
                .map(|c| c.correlator.as_str()),
            Some(repo.repo().repo_identity().name()),
        );

        let batch_len = request.batch.len();
        if !build_augmented_manifests {
            let stored = stream::iter(request.batch.into_iter().map({
                cloned!(repo);
                move |item| store_tree(repo.clone(), item)
            }))
            .buffer_unordered(MAX_CONCURRENT_UPLOAD_TREES_PER_REQUEST);
            return Ok(stored.map_ok(|(token, _tree)| token).boxed());
        }

        // The build reads none of the batch's hg manifests, so it and its
        // writes run while they are stored rather than after the last token.
        let verified: Vec<_> = stream::iter(
            request
                .batch
                .into_iter()
                .map(|item| verify_tree(&repo, item)),
        )
        .buffered(MAX_CONCURRENT_UPLOAD_TREES_PER_REQUEST)
        .collect()
        .await;
        let trees: Vec<HgManifestEnvelope> = verified
            .iter()
            .filter_map(|result| result.as_ref().ok().map(|(_, tree)| tree.clone()))
            .collect();
        let tokens = stream::iter(verified.into_iter().map({
            cloned!(repo);
            move |result| {
                cloned!(repo);
                async move {
                    let (token, tree) = result?;
                    repo.store_verified_tree(tree).await?;
                    anyhow::Ok(token)
                }
            }
        }))
        .buffer_unordered(MAX_CONCURRENT_UPLOAD_TREES_PER_REQUEST);
        let build = stream::once(async move {
            build_and_record_augmented_manifests(&repo, trees, batch_len).await;
            None::<Result<UploadTreeResponse, Error>>
        })
        .filter_map(future::ready);

        Ok(stream::select(tokens, build).boxed())
    }
}

pub struct CheckManifestPermissionHandler;

#[async_trait]
impl SaplingRemoteApiHandler for CheckManifestPermissionHandler {
    type Request = CheckManifestPermissionRequest;
    type Response = CheckManifestPermissionResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::CheckManifestPermission;
    const ENDPOINT: &'static str = "/check_manifest_permission";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();

        Ok(stream::iter(request.manifest_ids)
            .map(move |manifest_id| {
                let repo = repo.clone();
                async move {
                    let hg_manifest_id = HgManifestId::new(
                        HgNodeHash::from_bytes(manifest_id.as_ref())
                            .map_err(|e| anyhow::anyhow!("Invalid manifest id: {e}"))?,
                    );

                    let restriction_ctx = HgAugmentedTreeRestrictionContext::new_check_exists(
                        repo.clone(),
                        hg_manifest_id.into(),
                    )
                    .await?;
                    let denial = restriction_ctx.check_fetch_access().await?.err();
                    let has_access = denial.is_none();

                    repo.ctx()
                        .scuba()
                        .clone()
                        .add("repo", repo.repo().repo_identity().name())
                        .add("edenapi_method", "check_manifest_permission")
                        .add_opt(
                            "edenapi_user",
                            repo.ctx()
                                .metadata()
                                .identities()
                                .username()
                                .map(ToString::to_string),
                        )
                        .add(
                            "unix_username",
                            repo.ctx()
                                .metadata()
                                .identities()
                                .username()
                                .map(ToString::to_string),
                        )
                        .add_opt(
                            "restricted_path_acl",
                            denial.as_ref().map(|d| d.repo_region_acl().to_string()),
                        )
                        .add("has_restricted_path_access", has_access)
                        .log_with_msg("Checked manifest permission", None);

                    Ok(CheckManifestPermissionResponse {
                        manifest_id,
                        has_access,
                        request_acl: denial
                            .as_ref()
                            .map(|d| d.permission_request_group().to_string()),
                        denial_message: denial
                            .as_ref()
                            .and_then(|d| d.denial_message().map(ToString::to_string)),
                    })
                }
            })
            .buffer_unordered(20)
            .boxed())
    }
}

pub struct CheckPathPermissionHandler;

#[async_trait]
impl SaplingRemoteApiHandler for CheckPathPermissionHandler {
    type Request = CheckPathPermissionRequest;
    type Response = CheckPathPermissionResponse;

    const HTTP_METHOD: http::Method = http::Method::POST;
    const API_METHOD: SaplingRemoteApiMethod = SaplingRemoteApiMethod::CheckPathPermission;
    const ENDPOINT: &'static str = "/check_path_permission";

    async fn handler(
        ectx: SaplingRemoteApiContext<Self::PathExtractor, Self::QueryStringExtractor, Repo>,
        request: Self::Request,
    ) -> HandlerResult<'async_trait, Self::Response> {
        let repo = ectx.repo();
        let repo_ctx = repo.repo_ctx().clone();
        let cs = repo_ctx
            .changeset(request.hg_cs_id)
            .await
            .context("Failed to resolve changeset")?
            .ok_or_else(|| anyhow::anyhow!("Changeset not found: {}", request.hg_cs_id))?;

        Ok(stream::iter(request.paths)
            .map(move |path| {
                let cs = cs.clone();
                async move {
                    let result = async {
                        let mpath = MPath::new(path.as_str().as_bytes())
                            .with_context(|| format!("Invalid path: {path}"))?;
                        let restriction_ctx = cs
                            .path_restriction(mpath)
                            .await
                            .with_context(|| format!("Failed to check path restriction: {path}"))?;
                        let restriction_infos = restriction_ctx.restriction_info(true).await?;

                        let restriction_entries = restriction_infos
                            .iter()
                            .map(|info| {
                                let restriction_root = to_hg_path_nonroot(info.restriction_root())
                                    .with_context(|| {
                                        format!(
                                            "Invalid restriction root: {}",
                                            info.restriction_root()
                                        )
                                    })?;
                                Ok(CheckPathPermissionAclEntry {
                                    restriction_root,
                                    repo_region_acl: info.repo_region_acl().to_string(),
                                    permission_request_group: info
                                        .permission_request_group()
                                        .to_string(),
                                })
                            })
                            .collect::<anyhow::Result<Vec<_>>>()?;

                        let has_access = restriction_infos
                            .iter()
                            .all(|info| info.has_access.unwrap_or(false));

                        Ok::<_, anyhow::Error>(CheckPathPermissionData {
                            has_access,
                            restriction_entries,
                        })
                    }
                    .await
                    .map_err(|err| ServerError::generic(format!("{err:#}")));

                    Ok(CheckPathPermissionResponse::from_result(path, result))
                }
            })
            .buffer_unordered(20)
            .boxed())
    }

    fn extract_in_band_error(response: &Self::Response) -> Option<anyhow::Error> {
        response
            .result
            .as_ref()
            .err()
            .map(|err| anyhow::format_err!("{err:?}"))
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use mononoke_macros::mononoke;
    use mononoke_types::NonRootMPath;
    use restricted_paths::DenyingRestriction;
    use restricted_paths::PermissionRequestGroup;
    use restricted_paths::RestrictedManifestId;
    use restricted_paths::RestrictedPathAccess;
    use restricted_paths::RestrictedPathsAuthorizationError;
    use types::HgId;

    use super::*;

    #[mononoke::test]
    fn test_tree_fetch_error_to_slapi_error_preserves_manifest_permission_denied() -> Result<()> {
        let key = test_key()?;
        let err = restricted_paths_error(
            RestrictedPathAccess::Manifest(RestrictedManifestId::from(
                "1111111111111111111111111111111111111111",
            )),
            "REPO_REGION:test_acl",
            None,
        )?;

        let slapi_error = tree_fetch_error_to_slapi_error(key.clone(), err);
        match slapi_error.err {
            SaplingRemoteApiServerErrorKind::PermissionDenied {
                tree_id,
                request_acl: permission_request_group,
                denial_message,
            } => {
                assert_eq!(tree_id, key.hgid);
                assert_eq!(permission_request_group, "REPO_REGION:test_acl");
                assert_eq!(denial_message, None);
            }
            err => anyhow::bail!("expected PermissionDenied, got {err:?}"),
        }
        assert_eq!(slapi_error.key, Some(key));
        Ok(())
    }

    #[mononoke::test]
    fn test_tree_fetch_error_to_slapi_error_carries_denial_message() -> Result<()> {
        let key = test_key()?;
        let err = restricted_paths_error(
            RestrictedPathAccess::Manifest(RestrictedManifestId::from(
                "1111111111111111111111111111111111111111",
            )),
            "REPO_REGION:test_acl",
            Some("Ask the repo owners for access."),
        )?;

        let slapi_error = tree_fetch_error_to_slapi_error(key, err);
        match slapi_error.err {
            SaplingRemoteApiServerErrorKind::PermissionDenied { denial_message, .. } => {
                assert_eq!(
                    denial_message.as_deref(),
                    Some("Ask the repo owners for access.")
                );
            }
            err => anyhow::bail!("expected PermissionDenied, got {err:?}"),
        }
        Ok(())
    }

    #[mononoke::test]
    fn test_tree_fetch_error_to_slapi_error_ignores_path_permission_denied() -> Result<()> {
        let key = test_key()?;
        let err = restricted_paths_error(
            RestrictedPathAccess::Path(MPath::new("restricted")?),
            "REPO_REGION:test_acl",
            None,
        )?;

        let slapi_error = tree_fetch_error_to_slapi_error(key.clone(), err);
        if let SaplingRemoteApiServerErrorKind::PermissionDenied { .. } = slapi_error.err {
            anyhow::bail!("path access denial should not be converted to tree PermissionDenied");
        }
        assert_eq!(slapi_error.key, Some(key));
        Ok(())
    }

    fn restricted_paths_error(
        access: RestrictedPathAccess,
        permission_request_group: &str,
        denial_message: Option<&str>,
    ) -> Result<Error> {
        let permission_request_group: PermissionRequestGroup = permission_request_group.parse()?;
        // Mirror production: path denials know their restriction root (the
        // traversal walked through it); manifest denials may not.
        let restriction_root = match &access {
            RestrictedPathAccess::Path(path) => NonRootMPath::try_from(path.clone()).ok(),
            RestrictedPathAccess::Manifest(_) => None,
        };
        Ok(Error::new(MononokeError::RestrictedPathsAuthorizationError(
            RestrictedPathsAuthorizationError::new(
                access,
                DenyingRestriction {
                    restriction_root,
                    repo_region_acl: permission_request_group.clone(),
                    permission_request_group,
                },
                denial_message.map(str::to_string),
            ),
        ))
        .context("failed to fetch tree"))
    }

    fn test_key() -> Result<Key> {
        Ok(Key {
            hgid: HgId::null_id().clone(),
            path: RepoPathBuf::from_string("restricted".to_string())?,
        })
    }
}
