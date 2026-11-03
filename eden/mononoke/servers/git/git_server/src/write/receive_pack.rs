/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use bonsai_git_mapping::BonsaiGitMappingArc;
use bonsai_tag_mapping::BonsaiTagMappingEntry;
use bytes::Bytes;
use cloned::cloned;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use futures_stats::TimedTryFutureExt;
use gotham::helpers::http::Body;
use gotham::mime;
use gotham::state::FromState;
use gotham::state::State;
use gotham_ext::error::HttpError;
use gotham_ext::middleware::ScubaMiddlewareState;
use gotham_ext::response::BytesBody;
use gotham_ext::response::TryIntoResponse;
use http::Response;
use import_tools::GitImportLfs;
use import_tools::LfsServerUrlFormat;
use metaconfig_types::RepoConfigRef;
use mononoke_macros::mononoke;
use packetline::encode::flush_to_write;
use packetline::encode::write_text_packetline;
use protocol::pack_processor::parse_pack;
use repo_blobstore::RepoBlobstoreArc;
use repo_identity::RepoIdentityRef;
use scuba_ext::FutureStatsScubaExt;
use sharding_observability::WeightTracker;
use tracing::info;
use weight_observer::WeightObserver;

use crate::command::Command;
use crate::command::PushArgs;
use crate::command::RefUpdate;
use crate::command::RequestCommand;
use crate::model::GitMethodInfo;
use crate::model::GitServerContext;
use crate::model::PushData;
use crate::model::PushValidationErrors;
use crate::model::RepositoryParams;
use crate::model::RepositoryRequestContext;
use crate::scuba::scuba_from_state;
use crate::service::GitMappingsStore;
use crate::service::GitObjectStore;
use crate::service::UploadedObjects;
use crate::service::set_ref;
use crate::service::set_refs;
use crate::service::upload_objects;
use crate::util::empty_body;
use crate::util::get_body;
use crate::util::mononoke_source_of_truth;

const PACK_OK: &[u8] = b"unpack ok";
const REF_OK: &str = "ok";
const REF_ERR: &str = "ng";
const REF_UPDATE_CONCURRENCY: usize = 20;
const MAX_PACKETLINE_TEXT: usize = 65_000;

pub async fn receive_pack(state: &mut State) -> Result<Response<Body>, HttpError> {
    let repo_name = RepositoryParams::borrow_from(state).repo_name();
    ScubaMiddlewareState::try_borrow_add(state, "repo", repo_name.as_str());
    ScubaMiddlewareState::try_borrow_add(state, "method", "push");
    let body_bytes = get_body(state).await?;
    // We got a flush line packet to keep the connection alive. Just return Ok.
    if body_bytes == packetline::FLUSH_LINE {
        return empty_body(state);
    }
    let request_command =
        RequestCommand::parse_from_packetline(body_bytes).map_err(HttpError::e400)?;
    push(state, request_command).await.map_err(HttpError::e500)
}

async fn push(
    state: &mut State,
    request_command: RequestCommand,
) -> anyhow::Result<Response<Body>> {
    let repo_name = RepositoryParams::borrow_from(state).repo_name();
    let request_context = Arc::new(
        RepositoryRequestContext::instantiate(
            state,
            GitMethodInfo::from_command(&request_command.command, repo_name.clone()),
        )
        .await?,
    );
    let mut output = vec![];
    if let Command::Push(push_args) = request_command.command {
        let PushArgs {
            settings,
            pack_file,
            ref_updates,
            shallow: _,
        } = push_args;
        let (ctx, blobstore) = (
            &request_context.ctx.clone(),
            request_context.repo.repo_blobstore_arc().clone(),
        );
        PushData::inject_in_state(state, pack_file.get_ref().len());
        let mut scuba = scuba_from_state(ctx, state);
        if request_context.pushvars.unsampled_perf_logging() {
            scuba.sampled(1.try_into()?);
        }
        // If Mononoke is not the source of truth for this repo, then we need to prevent the push
        if !mononoke_source_of_truth(&request_context.ctx, request_context.repo.clone()).await? {
            return reject_non_sot_push(repo_name.as_str(), state, &ref_updates).await;
        }
        // If the pushed packfile is too large, then we need to prevent the push
        let max_request_size = justknobs::get_as::<usize>(
            "scm/mononoke:git_server_max_packfile_size",
            Some(repo_name.as_str()),
        );

        let packfile_size = pack_file.get_ref().len();
        if packfile_size > max_request_size {
            return reject_too_large_push(
                repo_name.as_str(),
                state,
                &ref_updates,
                packfile_size,
                max_request_size,
            )
            .await;
        }
        let concurrency = request_context.pushvars.concurrency();

        // Create a WeightTracker for push operations so ShardManager can see
        // push memory pressure via estimated_memory_bytes counter.
        let weight_observer: Option<Arc<dyn WeightObserver>> = if justknobs::eval(
            "scm/mononoke:git_server_enable_push_memory_tracking",
            None,
            Some(repo_name.as_str()),
        ) {
            let main_client_id = request_context
                .ctx
                .metadata()
                .client_info()
                .and_then(|ci| ci.request_info.as_ref())
                .and_then(|ri| ri.main_id.clone());
            Some(WeightTracker::new(
                request_context.ctx.fb,
                repo_name.clone(),
                main_client_id.as_deref(),
                request_context.ctx.metadata().identities(),
            ))
        } else {
            None
        };

        // Parse the packfile provided as part of the push and verify that its valid
        let (parsed_objects, _weight_guard) = parse_pack(
            pack_file.split().1,
            ctx,
            blobstore.clone(),
            concurrency,
            weight_observer,
        )
        .try_timed()
        .await?
        .log_future_stats(
            scuba.clone(),
            "Parsed complete Packfile",
            "Push".to_string(),
        );
        drop(pack_file);

        // Generate the GitObjectStore using the parsed objects
        let object_store = Arc::new(GitObjectStore::new(parsed_objects, ctx, blobstore.clone()));
        // Instantiate the LFS configuration. `git_ctx.internal_lfs()` is the
        // effective decision the CLI computed — it's true whenever the user
        // either passed `--internal-lfs` or did not pass
        // `--upstream-lfs-server` (so the default of "no LFS flags" is
        // internal mode).
        //
        // The `x-git-allow-dangling-lfs-pointers` pushvar relaxes both modes:
        // when set, an LFS push still succeeds even if the pointer's content
        // can't be found (in the filestore for internal mode, or at the
        // upstream LFS server for upstream mode), in which case the pointer
        // bytes themselves are stored as the file content and the file is
        // marked as `GitLfs::FullContent`. When the content *is* present, this
        // flag changes nothing — the pointer is still resolved and the file is
        // stored as a `GitLfsPointer` referencing the resolved bytes.
        let git_ctx = GitServerContext::borrow_from(state);
        let allow_dangling_lfs_pointers = request_context.pushvars.allow_dangling_lfs_pointers();
        let lfs = if request_context
            .repo
            .repo_config()
            .git_configs
            .git_lfs_interpret_pointers
        {
            if git_ctx.internal_lfs() {
                GitImportLfs::new_internal(
                    request_context.repo.repo_blobstore_arc().boxed(),
                    allow_dangling_lfs_pointers,
                )
            } else {
                let max_lfs_tries =
                    justknobs::get_as::<u32>("scm/mononoke:git_server_lfs_max_retries", None);
                let url_format = match git_ctx.upstream_lfs_url_format() {
                    crate::UpstreamLfsUrlFormat::Dewey => LfsServerUrlFormat::LegacyDewey,
                    crate::UpstreamLfsUrlFormat::MononokeGitLfs => {
                        LfsServerUrlFormat::MononokeGitLfs {
                            repo_name: request_context.repo.repo_identity().name().to_string(),
                        }
                    }
                };
                GitImportLfs::new(
                    git_ctx
                        .upstream_lfs_server()?
                        .ok_or_else(|| anyhow::anyhow!("No upstream LFS server specified"))?,
                    url_format,
                    allow_dangling_lfs_pointers,
                    max_lfs_tries, // max attempts
                    Some(50),      // conn_limit
                    git_ctx.tls_args()?,
                )?
            }
        } else {
            GitImportLfs::new_disabled()
        };
        // Evaluate once and thread downstream so a mid-push flip can't desync
        // the upload and bookmark-move phases.
        let atomic_tag_mapping = justknobs::eval(
            "scm/mononoke:git_atomic_tag_mapping",
            None,
            Some(repo_name.as_str()),
        );
        // Upload the objects corresponding to the push to the underlying store
        let upload_objects_result = upload_objects(
            ctx,
            request_context.repo.clone(),
            object_store.clone(),
            &ref_updates,
            lfs,
            concurrency,
            git_ctx.persist_partial_mappings(),
            atomic_tag_mapping,
        )
        .try_timed()
        .await;
        let UploadedObjects {
            ref_map,
            ref_updates,
            tag_mapping_entries,
        } = match upload_objects_result {
            Ok(result) => result.log_future_stats(
                scuba.clone(),
                "GitImport, Derivation and Bonsai creation completed",
                "Push".to_string(),
            ),
            Err(e) => {
                let err_msg = format!("{e:?}");
                if err_msg.contains("find_file_changes")
                    && (err_msg.contains("status: 404")
                        || err_msg.contains("not found in internal filestore"))
                {
                    return reject_push_with_message(
                        state,
                        &ref_updates,
                        format!("LFS files missing in Git LFS server. Please upload before pushing. Error:\n {err_msg}"),
                        true /* with_unpack_error */
                    )
                    .await;
                } else {
                    return Err(e);
                }
            }
        };

        // We were successful in parsing the pack and uploading the objects to underlying store. Indicate this to the client
        write_text_packetline(PACK_OK, &mut output)
            .try_timed()
            .await?
            .log_future_stats(scuba.clone(), "Sent Packfile OK", "Push".to_string());
        // Create bonsai_git_mapping store to enable mapping lookup during bookmark movement
        let git_bonsai_mapping_store = Arc::new(GitMappingsStore::new(
            ctx,
            request_context.repo.bonsai_git_mapping_arc(),
            ref_map,
        ));

        // Get the RL land service address if configured (for push diversion)
        let multi_repo_land_service_address = git_ctx.multi_repo_land_service_address();

        // Extract ACL provider while we have access to State (State is not
        // Sync so it cannot be passed to refs_update across await points).
        let acl_provider = git_ctx.acl_provider();

        // Some(map) activates the atomic path (keyed by tags/<tag> name); None
        // keeps the old behavior.
        let tag_mapping_entries = if atomic_tag_mapping {
            Some(
                tag_mapping_entries
                    .into_iter()
                    .map(|entry| (entry.tag_name.clone(), entry))
                    .collect::<HashMap<String, BonsaiTagMappingEntry>>(),
            )
        } else {
            None
        };

        let updated_refs = refs_update(
            ref_updates,
            request_context.clone(),
            git_bonsai_mapping_store.clone(),
            object_store.clone(),
            settings.atomic,
            multi_repo_land_service_address,
            acl_provider,
            tag_mapping_entries,
        )
        .try_timed()
        .await?
        .log_future_stats(
            scuba.clone(),
            "Bookmark movement completed",
            "Push".to_string(),
        );

        let mut validation_errors = PushValidationErrors::default();
        // For each ref, update the status as ok or ng based on the result of the bookmark set operation
        for (updated_ref, result) in updated_refs {
            match result {
                Ok(_) => {
                    write_text_packetline(
                        format!("{} {}", REF_OK, updated_ref.ref_name).as_bytes(),
                        &mut output,
                    )
                    .await?;
                }
                Err(e) => {
                    validation_errors
                        .add_error(updated_ref.ref_name.clone(), e.root_cause().to_string());
                    write_text_packetline(
                        format!(
                            "{} {} {}",
                            REF_ERR,
                            updated_ref.ref_name,
                            packetline_truncated_string(e.root_cause().to_string())
                        )
                        .as_bytes(),
                        &mut output,
                    )
                    .await?;
                }
            }
        }
        if !validation_errors.is_empty() {
            state.put(validation_errors);
        }
        flush_to_write(&mut output).await?;
    }
    BytesBody::new(Bytes::from(output), mime::TEXT_PLAIN).try_into_response(state)
}

/// Function responsible for updating the refs in the repo
async fn refs_update(
    ref_updates: Vec<RefUpdate>,
    request_context: Arc<RepositoryRequestContext>,
    git_bonsai_mapping_store: Arc<GitMappingsStore>,
    object_store: Arc<GitObjectStore>,
    atomic_update: bool,
    _multi_repo_land_service_address: Option<String>,
    acl_provider: Arc<dyn permission_checker::AclProvider>,
    tag_mapping_entries: Option<HashMap<String, BonsaiTagMappingEntry>>,
) -> anyhow::Result<Vec<(RefUpdate, anyhow::Result<()>)>> {
    use super::push_diversion::PushDiversionMode;

    // Share the entries across the concurrent per-ref set_ref calls.
    let tag_mapping_entries = tag_mapping_entries.map(Arc::new);

    #[cfg_attr(
        not(fbcode_build),
        expect(unused_variables, reason = "Only internal builds divert pushes")
    )]
    let diversion_mode = PushDiversionMode::resolve(&request_context, &acl_provider).await?;

    // Diversion: one synchronous submit_manifest_land to Multi-Repo Land.
    // Branch creates/moves are diverted; other refs (deletes, tags, etc.)
    // are handled by the normal git server path below.
    // NOTE: When diversion is active and the push contains both divertable and
    // non-divertable refs, atomicity is not preserved across the two paths.
    // In practice, diverted repos only have branch pushes, so this is acceptable.
    #[cfg(fbcode_build)]
    if matches!(diversion_mode, PushDiversionMode::RlLandServiceDiversion) {
        let mut diversion = super::rl_land_service_diversion::divert_to_rl_land_service(
            ref_updates,
            request_context.clone(),
            git_bonsai_mapping_store.clone(),
            object_store.clone(),
            _multi_repo_land_service_address,
        )
        .await?;

        // Process remaining refs (deletes, tags, etc.) through the normal path.
        if !diversion.remaining.is_empty() {
            let remaining_results = if atomic_update {
                atomic_refs_update(
                    diversion.remaining,
                    request_context,
                    git_bonsai_mapping_store,
                    object_store,
                    tag_mapping_entries,
                )
                .await?
            } else {
                non_atomic_refs_update(
                    diversion.remaining,
                    request_context,
                    git_bonsai_mapping_store,
                    object_store,
                    tag_mapping_entries,
                )
                .await?
            };
            diversion.diverted.extend(remaining_results);
        }

        return Ok(diversion.diverted);
    }

    // Normal git server path: process all refs directly.
    // Used for non-diverted repos AND emergency pushes.
    let results = if atomic_update {
        atomic_refs_update(
            ref_updates,
            request_context.clone(),
            git_bonsai_mapping_store,
            object_store,
            tag_mapping_entries,
        )
        .await?
    } else {
        non_atomic_refs_update(
            ref_updates,
            request_context.clone(),
            git_bonsai_mapping_store,
            object_store,
            tag_mapping_entries,
        )
        .await?
    };

    // An emergency push leaves manifest pins stale; the reconciler re-pins
    // within its interval.
    Ok(results)
}

/// Function responsible for updating the refs in the repo non-atomically.
async fn non_atomic_refs_update(
    ref_updates: Vec<RefUpdate>,
    request_context: Arc<RepositoryRequestContext>,
    git_bonsai_mapping_store: Arc<GitMappingsStore>,
    object_store: Arc<GitObjectStore>,
    tag_mapping_entries: Option<Arc<HashMap<String, BonsaiTagMappingEntry>>>,
) -> anyhow::Result<Vec<(RefUpdate, anyhow::Result<()>)>> {
    stream::iter(ref_updates.clone())
        .map(|ref_update| {
            cloned!(
                request_context,
                git_bonsai_mapping_store,
                object_store,
                tag_mapping_entries
            );
            async move {
                let ref_info = ref_update.clone();
                info!(
                    "Updating ref {} from {} to {}",
                    ref_info.ref_name.as_str(),
                    ref_info.from.to_hex(),
                    ref_info.to.to_hex()
                );
                let output = mononoke::spawn_task(async move {
                    set_ref(
                        request_context,
                        git_bonsai_mapping_store,
                        object_store,
                        ref_update,
                        tag_mapping_entries,
                    )
                    .await
                })
                .await?;
                info!(
                    "Updated ref {} from {} to {}",
                    ref_info.ref_name.as_str(),
                    ref_info.from.to_hex(),
                    ref_info.to.to_hex()
                );
                anyhow::Ok(output)
            }
        })
        .buffer_unordered(REF_UPDATE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await
}

/// Function responsible for updating the refs in the repo atomically.
async fn atomic_refs_update(
    ref_updates: Vec<RefUpdate>,
    request_context: Arc<RepositoryRequestContext>,
    git_bonsai_mapping_store: Arc<GitMappingsStore>,
    object_store: Arc<GitObjectStore>,
    tag_mapping_entries: Option<Arc<HashMap<String, BonsaiTagMappingEntry>>>,
) -> anyhow::Result<Vec<(RefUpdate, anyhow::Result<()>)>> {
    match set_refs(
        request_context,
        git_bonsai_mapping_store,
        object_store,
        ref_updates.clone(),
        tag_mapping_entries,
    )
    .await
    {
        Ok(_) => Ok(ref_updates
            .into_iter()
            .map(|ref_update| (ref_update, Ok(())))
            .collect()),
        Err(e) => {
            let err_str = format!(
                "Atomic bookmark update failed with error: {}",
                e.root_cause()
            );
            Ok(ref_updates
                .into_iter()
                .map(|ref_update| (ref_update, Err(anyhow::anyhow!(err_str.to_string()))))
                .collect())
        }
    }
}

async fn reject_non_sot_push(
    repo_name: &str,
    state: &mut State,
    ref_updates: &[RefUpdate],
) -> anyhow::Result<Response<Body>> {
    let error_message =
        format!("Push rejected: Mononoke is not the source of truth for repo {repo_name}");
    reject_push_with_message(
        state,
        ref_updates,
        error_message,
        true, /* with_unpack_error */
    )
    .await
}

async fn reject_too_large_push(
    repo_name: &str,
    state: &mut State,
    ref_updates: &[RefUpdate],
    pushed_packfile_size: usize,
    max_size: usize,
) -> anyhow::Result<Response<Body>> {
    let error_message = format!(
        "Push rejected: Pushed packfile is too large for repo {repo_name}. The limit is {max_size}, while the pushed packfile size is {pushed_packfile_size}. Try pushing fewer commits at once, or put large binary blobs into LFS. Only if you can't do that, consult the wiki https://fburl.com/git_push_blocked_by_size_limit."
    );
    reject_push_with_message(
        state,
        ref_updates,
        error_message,
        true, /* with_unpack_error */
    )
    .await
}

async fn reject_push_with_message(
    state: &mut State,
    ref_updates: &[RefUpdate],
    error_message: String,
    with_unpack_error: bool,
) -> anyhow::Result<Response<Body>> {
    let mut output = vec![];
    let error_message = packetline_truncated_string(error_message);
    if with_unpack_error {
        let unpack_error = format!("unpack {error_message}");
        write_text_packetline(unpack_error.as_bytes(), &mut output).await?;
    } else {
        write_text_packetline(PACK_OK, &mut output).await?;
    }
    for ref_update in ref_updates {
        write_text_packetline(
            format!("{} {} {}", REF_ERR, ref_update.ref_name, error_message).as_bytes(),
            &mut output,
        )
        .await?;
    }
    flush_to_write(&mut output).await?;
    BytesBody::new(Bytes::from(output), mime::TEXT_PLAIN).try_into_response(state)
}

fn packetline_truncated_string(s: String) -> String {
    if s.len() > MAX_PACKETLINE_TEXT {
        format!(
            "{}...truncated due to message too long",
            &s[..MAX_PACKETLINE_TEXT]
        )
    } else {
        s
    }
}
