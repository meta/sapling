/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::pin::Pin;
use std::sync::Arc;
use std::task::Context as TaskContext;
use std::task::Poll;
use std::task::ready;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use async_stream::try_stream;
use bonsai_git_mapping::BonsaiGitMappingRef;
use bonsai_git_mapping::BonsaisOrGitShas;
use bytes::Bytes;
use either::Either;
use futures::Future;
use futures::Stream;
use futures::StreamExt;
use futures::future::try_join4;
use git_env::GitHost;
use gotham::helpers::http::Body;
use gotham::mime;
use gotham::state::FromState;
use gotham::state::State;
use gotham_ext::error::HttpError;
use gotham_ext::middleware::ScubaMiddlewareState;
use gotham_ext::response::BytesBody;
use gotham_ext::response::ResponseStream;
use gotham_ext::response::ResponseTryStreamExt;
use gotham_ext::response::StreamBody;
use gotham_ext::response::TryIntoResponse;
use http::Response;
use http::StatusCode;
use mononoke_macros::mononoke;
use packetline::FLUSH_LINE;
use packetline::encode::delim_to_write;
use packetline::encode::flush_to_write;
use packetline::encode::write_binary_packetline;
use packetline::encode::write_data_channel;
use packetline::encode::write_error_channel;
use packetline::encode::write_progress_channel;
use packetline::encode::write_text_packetline;
use packfile::owned_async_writer::WrapperSender;
use packfile::pack::DeltaForm;
use packfile::pack::PackfileWriter;
use pin_project::pin_project;
use protocol::generator::fetch_response;
use protocol::generator::ls_refs_response;
use protocol::generator::shallow_info as fetch_shallow_info;
use protocol::mapping::ref_oid_mapping;
use protocol::types::FetchResponse;
use protocol::types::PackfileConcurrency;
use protocol::types::ShallowInfoResponse;
use protocol::validator::validate_shallow_fetch_without_deepen;
use repo_identity::RepoIdentityRef;
use rustc_hash::FxHashSet;
use scuba_ext::MononokeScubaSampleBuilder;
use sharding_observability::WeightTracker;
use stats::prelude::*;
use tokio::sync::mpsc;
use tokio_util::task::AbortOnDropHandle;
use weight_observer::WeightObserver;

use crate::command::Command;
use crate::command::FetchArgs;
use crate::command::LsRefsArgs;
use crate::command::RequestCommand;
use crate::model::BundleUriOutcome;
use crate::model::GitMethodInfo;
use crate::model::RepositoryParams;
use crate::model::RepositoryRequestContext;
use crate::model::ResponseType;
use crate::model::Service;
use crate::scuba::MononokeGitScubaHandler;
use crate::scuba::MononokeGitScubaKey;
use crate::util::empty_body;
use crate::util::get_body;

define_stats! {
    prefix = "mononoke.git.request";
    packfile_read_error: timeseries(Rate, Sum),
}

/// The header for the packfile section of the response
const PACKFILE_HEADER: &[u8] = b"packfile";
/// The header for the acknowledgements section of the response
const ACKNOWLEDGEMENTS_HEADER: &[u8] = b"acknowledgments";
/// The header for the wanted-refs section of the response
const WANTED_REFS_HEADER: &[u8] = b"wanted-refs";
/// The header for the shallow info section of the response
const SHALLOW_INFO_HEADER: &[u8] = b"shallow-info";
/// Acknowledgement that the object sent by the client exists on the server
const ACK: &str = "ACK";
/// Acknowledgement that the object sent by the client does not exist on the server
const NAK: &[u8] = b"NAK";

#[derive(Debug, Clone)]
struct FetchResponseHeaders {
    acknowledgements: Option<Bytes>,
    shallow_info: Option<Bytes>,
    wanted_refs: Option<Bytes>,
    packfile_uris: Option<Bytes>,
    pack_header: Option<Bytes>,
    shallow_response: Option<ShallowInfoResponse>,
}

async fn pack_header() -> Result<Bytes, Error> {
    let mut buf = Vec::with_capacity(PACKFILE_HEADER.len());
    write_text_packetline(PACKFILE_HEADER, &mut buf).await?;
    Ok(Bytes::from(buf))
}

fn concurrency(context: &RepositoryRequestContext) -> PackfileConcurrency {
    match &context.repo.repo_config.git_configs.git_concurrency {
        Some(concurrency) => PackfileConcurrency::new(
            concurrency.trees_and_blobs,
            concurrency.commits,
            concurrency.tags,
            concurrency.shallow,
            context.repo_configs.common.git_memory_upper_bound,
        ),
        None => PackfileConcurrency::standard(),
    }
}

async fn acknowledgements(
    context: Arc<RepositoryRequestContext>,
    args: Arc<FetchArgs>,
) -> Result<(Option<Bytes>, Option<Bytes>), Error> {
    if args.done {
        // The negotiation has already completed (or was not needed in the first place) so no
        // need to generate the acknowledgements section. We can straight away generate the
        // packfile based on the data sent by the client
        return Ok((None, Some(pack_header().await?)));
    }
    let git_shas = BonsaisOrGitShas::from_object_ids(args.haves().iter())?;
    let entries = context
        .repo
        .bonsai_git_mapping()
        .get(&context.ctx, git_shas)
        .await
        .with_context(|| {
            format!(
                "Failed to fetch bonsai_git_mapping for repo {}",
                context.repo.repo_identity().name()
            )
        })?;
    let mut output_buffer = vec![];
    write_text_packetline(ACKNOWLEDGEMENTS_HEADER, &mut output_buffer).await?;
    if entries.is_empty() && !args.haves().is_empty() {
        // None of the Git Shas provided by the client are recognized by the server
        write_text_packetline(NAK, &mut output_buffer).await?;
    } else {
        // Provide an acknowledgement for the Git Shas that were recognized by the server
        for entry in entries {
            write_text_packetline(
                format!("{} {}", ACK, entry.git_sha1.to_hex()).as_bytes(),
                &mut output_buffer,
            )
            .await?;
        }
    }
    Ok((Some(Bytes::from(output_buffer)), None))
}

async fn shallow_info(
    context: Arc<RepositoryRequestContext>,
    args: Arc<FetchArgs>,
) -> Result<(Option<Bytes>, Option<ShallowInfoResponse>), Error> {
    let request = args.into_shallow_request();

    // Validate shallow fetch without deepen args to prevent broken repo state
    let block_indirect_unshallow = justknobs::eval(
        "scm/mononoke:git_block_indirect_unshallow_fetch",
        None,
        None,
    );
    if block_indirect_unshallow && !request.shallow.is_empty() && request.variant.is_none() {
        validate_shallow_fetch_without_deepen(&context.ctx, &context.repo, &request).await?;
    }

    // If the client did not request a shallow clone/fetch, then we can return early
    if !request.shallow_requested() {
        return Ok((None, None));
    }
    let mut output_buffer = vec![];
    write_text_packetline(SHALLOW_INFO_HEADER, &mut output_buffer).await?;
    let response = fetch_shallow_info(
        context.ctx.clone(),
        &context.repo,
        args.into_shallow_request(),
    )
    .await?;
    for boundary_commit in response.info_commits.boundary_commits.iter() {
        write_text_packetline(
            format!("shallow {}", boundary_commit.oid().to_hex()).as_bytes(),
            &mut output_buffer,
        )
        .await?;
    }
    let git_commits = response
        .info_commits
        .commits
        .iter()
        .map(|entry| entry.oid())
        .collect::<FxHashSet<_>>();
    for client_shallow_commit in request.shallow {
        if git_commits.contains(&client_shallow_commit) {
            write_text_packetline(
                format!("unshallow {}", client_shallow_commit.to_hex()).as_bytes(),
                &mut output_buffer,
            )
            .await?;
        }
    }
    // Add a delim line to indicate the end of the shallow info section. Note that
    // the delim line will not be followed by a newline character
    delim_to_write(&mut output_buffer).await?;
    Ok((Some(Bytes::from(output_buffer)), Some(response)))
}

async fn wanted_refs(
    context: Arc<RepositoryRequestContext>,
    args: Arc<FetchArgs>,
) -> Result<Option<Bytes>, Error> {
    // If there are no refs explicitly requested, then we can return early
    if args.want_refs.is_empty() {
        return Ok(None);
    }
    let mut output_buffer = vec![];
    write_text_packetline(WANTED_REFS_HEADER, &mut output_buffer).await?;
    let refs = ref_oid_mapping(&context.ctx, &context.repo, args.want_refs.clone())
        .await
        .context("Failed to fetch ref_oid_mapping for wanted-refs")?;
    for (ref_key, oid) in refs {
        write_text_packetline(
            format!("{} {}", ref_key, oid.to_hex()).as_bytes(),
            &mut output_buffer,
        )
        .await?;
    }
    // Add a delim line to indicate the end of the wanted-refs section. Note that
    // the delim line will not be followed by a newline character
    delim_to_write(&mut output_buffer).await?;
    Ok(Some(Bytes::from(output_buffer)))
}

async fn packfile_uris(
    _context: Arc<RepositoryRequestContext>,
    _args: Arc<FetchArgs>,
) -> Result<Option<Bytes>, Error> {
    // TODO(rajshar): Implement packfile-uris support
    Ok(None)
}

impl FetchResponseHeaders {
    async fn from_request(
        context: RepositoryRequestContext,
        args: FetchArgs,
    ) -> Result<Self, Error> {
        let (context, args) = (Arc::new(context), Arc::new(args));
        let acknowledgements_future = async {
            acknowledgements(context.clone(), args.clone())
                .await
                .context("Failed to generate acknowledgements")
        };
        let shallow_info_future = async {
            shallow_info(context.clone(), args.clone())
                .await
                .context("Failed to generate shallow info")
        };
        let wanted_refs_future = async {
            wanted_refs(context.clone(), args.clone())
                .await
                .context("Failed to generate wanted refs")
        };
        let packfile_uris_future = async {
            packfile_uris(context.clone(), args.clone())
                .await
                .context("Failed to generate packfile uris")
        };
        let (
            (acknowledgements, pack_header),
            (shallow_info, shallow_response),
            wanted_refs,
            packfile_uris,
        ) = try_join4(
            acknowledgements_future,
            shallow_info_future,
            wanted_refs_future,
            packfile_uris_future,
        )
        .await?;
        Ok(Self {
            acknowledgements,
            shallow_info,
            wanted_refs,
            packfile_uris,
            pack_header,
            shallow_response,
        })
    }

    fn include_pack(&self) -> bool {
        self.pack_header.is_some()
    }
}

impl Iterator for FetchResponseHeaders {
    type Item = Bytes;

    fn next(&mut self) -> Option<Self::Item> {
        match self.acknowledgements.take() {
            Some(acknowledgements) => Some(acknowledgements),
            _ => {
                if !self.include_pack() {
                    // If we are not sending the packfile, we do not send any other section
                    // except acknowledgements
                    None
                } else {
                    match self.shallow_info.take() {
                        Some(shallow_info) => Some(shallow_info),
                        _ => match self.wanted_refs.take() {
                            Some(wanted_refs) => Some(wanted_refs),
                            _ => match self.packfile_uris.take() {
                                Some(packfile_uris) => Some(packfile_uris),
                                _ => self.pack_header.take(),
                            },
                        },
                    }
                }
            }
        }
    }
}

pub async fn clone_bundle(state: &mut State) -> Result<Response<Body>, HttpError> {
    let repo_name = RepositoryParams::borrow_from(state).repo_name();
    ScubaMiddlewareState::try_borrow_add(state, "repo", repo_name.as_str());
    ScubaMiddlewareState::try_borrow_add(state, "method", "clone_bundle");
    let request_context = RepositoryRequestContext::instantiate(
        state,
        GitMethodInfo::standard(repo_name, crate::model::GitMethod::CloneBundle),
    )
    .await?;

    let git_host = GitHost::from_state_mononoke_host(state)?;
    if let Some(clone_bundle_url) = get_bundle_url(state, &request_context, git_host).await {
        Ok(gotham::helpers::http::response::create_temporary_redirect(
            state,
            clone_bundle_url,
        ))
    } else {
        Ok(gotham::helpers::http::response::create_empty_response(
            state,
            StatusCode::NOT_FOUND,
        ))
    }
}

pub async fn upload_pack(state: &mut State) -> Result<Response<Body>, HttpError> {
    let repo_name = RepositoryParams::borrow_from(state).repo_name();
    ScubaMiddlewareState::try_borrow_add(state, "repo", repo_name.as_str());
    ScubaMiddlewareState::try_borrow_add(state, "method", "clone|pull");
    let body_bytes = get_body(state).await?;
    // We got a flush line packet to keep the connection alive. Just return Ok.
    if body_bytes == packetline::FLUSH_LINE {
        return empty_body(state);
    }
    let request_command =
        RequestCommand::parse_from_packetline(body_bytes).map_err(HttpError::e400)?;
    let request_context = RepositoryRequestContext::instantiate(
        state,
        GitMethodInfo::from_command(&request_command.command, repo_name),
    )
    .await?;
    state.put(Service::GitUploadPack);
    state.put(ResponseType::Result);
    match request_command.command {
        Command::LsRefs(ls_refs_args) => {
            let output = ls_refs(&request_context, ls_refs_args).await;
            let output = output.map_err(HttpError::e500)?.try_into_response(state);
            output.map_err(HttpError::e500)
        }
        Command::Fetch(fetch_args) => {
            let scuba_handler = MononokeGitScubaHandler::from_state(state);
            let output = match fetch(&request_context, fetch_args, scuba_handler).await {
                Ok(output) => Either::Right(output),
                Err(e) => Either::Left(git_error_message(&e).await?),
            };
            output.try_into_response(state).map_err(HttpError::e500)
        }
        Command::BundleUri => {
            let git_host = GitHost::from_state_mononoke_host(state)?;
            let output = bundle_uri(state, &request_context, git_host).await;
            let output = output.map_err(HttpError::e500)?.try_into_response(state);
            output.map_err(HttpError::e500)
        }
        Command::Push(_) => Err(HttpError::e500(anyhow::anyhow!(
            "Push command directed to incorrect upload-pack handler"
        ))),
    }
}

async fn get_bundle_url(
    state: &mut State,
    request_context: &RepositoryRequestContext,
    git_host: GitHost,
) -> Option<String> {
    if !request_context.bundle_uri_configured() {
        return None;
    }
    if !request_context.ctx.metadata().client_untrusted() {
        let bundle_uri = &request_context.repo.git_bundle_uri;
        let bundle_list = bundle_uri
            .get_latest_bundle_list(&request_context.ctx)
            .await
            .ok()?;

        if let Some(bundle_list) = bundle_list {
            // TODO(mzr) we only generate full repo bundles at the moment so all bundle lists are
            // of len 1. This might change once we implement incremental bundles at some point.
            if let Some(bundle) = bundle_list.bundles.first() {
                let url = bundle_uri
                    .get_url_for_bundle_handle(&request_context.ctx, &git_host, 60, &bundle.handle)
                    .await
                    .ok()?;
                return Some(url);
            }
        } else {
            state.put(BundleUriOutcome::Success("bundle-list empty".to_string()));
        }
    } else {
        state.put(BundleUriOutcome::Success("client untrusted".to_string()));
    }
    None
}

async fn bundle_uri(
    state: &mut State,
    request_context: &RepositoryRequestContext,
    git_host: GitHost,
) -> Result<impl TryIntoResponse + use<>, Error> {
    let mut out: Vec<u8> = br#"bundle.version=1
bundle.mode=all
bundle.heuristic=creationToken
"#
    .into();

    let bundle_trusted_only = request_context.bundle_uri_trusted_only();

    if !request_context.bundle_uri_configured() {
        state.put(BundleUriOutcome::Success(
            "no bundle-uri config".to_string(),
        ));
    } else if !bundle_trusted_only || !request_context.ctx.metadata().client_untrusted() {
        let bundle_uri = &request_context.repo.git_bundle_uri;
        let bundle_list = bundle_uri
            .get_latest_bundle_list(&request_context.ctx)
            .await?;

        if let Some(bundle_list) = bundle_list {
            let bundle_list_out: Result<Vec<u8>, anyhow::Error> = try {
                let mut bundle_list_out_buf: Vec<u8> = vec![];
                for (i, bundle) in bundle_list.bundles.iter().enumerate() {
                    let uri = bundle_uri
                        .get_url_for_bundle_handle(
                            &request_context.ctx,
                            &git_host,
                            60,
                            &bundle.handle,
                        )
                        .await?;

                    let str = format!(
                        r#"bundle.bundle_{}.uri={}
bundle.bundle_{}.creationtoken={}"#,
                        bundle.fingerprint,
                        uri,
                        bundle.fingerprint,
                        i + 1
                    );
                    bundle_list_out_buf.extend_from_slice(str.as_bytes());
                }
                bundle_list_out_buf
            };
            match bundle_list_out {
                Ok(blo) => {
                    state.put(BundleUriOutcome::Success(format!(
                        "advertised {} bundles from bundle list {}",
                        bundle_list.bundles.len(),
                        bundle_list.bundle_list_num
                    )));
                    out.extend_from_slice(&blo[..])
                }
                Err(err) => {
                    state.put(BundleUriOutcome::Error(format!("{err:?}")));
                }
            }
        } else {
            state.put(BundleUriOutcome::Success("bundle-list empty".to_string()));
        }
    } else {
        state.put(BundleUriOutcome::Success("client untrusted".to_string()));
    }

    let mut output = Vec::new();
    for line in out.split(|x| *x == b'\n') {
        if line.is_empty() {
            continue;
        }
        write_binary_packetline(line, &mut output).await?;
    }

    flush_to_write(&mut output).await?;
    let res = BytesBody::new(Bytes::from(output), mime::TEXT_PLAIN);
    Ok(res)
}

/// Method responsible for generating the response for ls_refs command request
pub async fn ls_refs(
    request_context: &RepositoryRequestContext,
    args: LsRefsArgs,
) -> Result<impl TryIntoResponse + use<>, Error> {
    let response = ls_refs_response(
        &request_context.ctx,
        &request_context.repo,
        args.into_request(request_context.pushvars.bypass_bookmark_cache()),
    )
    .await?;
    let mut output = Vec::new();
    response.write_packetline(&mut output).await?;
    flush_to_write(&mut output).await?;
    Ok(BytesBody::new(Bytes::from(output), mime::TEXT_PLAIN))
}

/// Method responsible for generating the response to fetch command request
pub async fn fetch(
    request_context: &RepositoryRequestContext,
    args: FetchArgs,
    scuba_handler: MononokeGitScubaHandler,
) -> Result<impl TryIntoResponse + use<>, Error> {
    let n_haves = args.haves().len();
    let n_wants = args.wants().len();
    let (writer, reader) = mpsc::channel::<Vec<u8>>(100);
    let (progress_writer, mut progress_reader) = mpsc::channel::<String>(50);
    let (error_writer, mut err_reader) = mpsc::channel::<String>(50);
    let mut fetch_response_headers =
        FetchResponseHeaders::from_request(request_context.clone(), args.clone()).await?;
    let include_pack = fetch_response_headers.include_pack();
    let shallow_response = fetch_response_headers.shallow_response.take();
    let delta_form = if request_context.pushvars.use_only_offset_delta() {
        DeltaForm::OnlyOffset
    } else {
        DeltaForm::RefAndOffset
    };
    let max_buffer = justknobs::get_as::<usize>("scm/mononoke:git_writer_buffer_bytes", None);
    // Some repos might be configured to display a message to users when they
    // run `git pull`.
    let mb_fetch_msg = git_fetch_message(request_context).await?;
    let bytes_stream = try_stream! {
        let mut container = Vec::with_capacity(max_buffer);
        for header in fetch_response_headers {
            yield header;
        }
        // Only include the packfile if it is requested by client
        if include_pack {
            let mut pack_reader = tokio_stream::wrappers::ReceiverStream::new(reader);
            if let Some(fetch_msg) = mb_fetch_msg {
                let mut buf = Vec::with_capacity(fetch_msg.len());
                write_progress_channel(fetch_msg.as_ref(), &mut buf).await?;
                yield Bytes::from(buf);
            }
            while let Some(progress) = progress_reader.recv().await {
                let mut buf = Vec::with_capacity(progress.len());
                write_progress_channel(progress.as_ref(), &mut buf).await?;
                yield Bytes::from(buf);
            }
            while let Some(chunk) = pack_reader.next().await {
                if container.len() >= max_buffer {
                    yield Bytes::from(container);
                    container = Vec::with_capacity(max_buffer);
                }
                // Write the actual packfile content to the data channel
                write_data_channel(chunk.as_ref(), &mut container).await?;

            }
            while let Some(err_msg) = err_reader.recv().await {
                let mut buf = Vec::with_capacity(err_msg.len());
                write_error_channel(err_msg.as_ref(), &mut buf).await?;
                yield Bytes::from(buf);
            }
        }
        if !container.is_empty() {
            yield Bytes::from(container);
        }
        let mut buf = Vec::with_capacity(FLUSH_LINE.len());
        flush_to_write(&mut buf).await?;
        yield Bytes::from(buf);
        return;
    };
    let generator = {
        let request_context = request_context.clone();
        async move {
            // Track this request for memory-based scaling
            let repo_name = request_context.repo.repo_identity().name().to_string();
            let main_client_id = request_context
                .ctx
                .metadata()
                .client_info()
                .and_then(|ci| ci.request_info.as_ref())
                .and_then(|ri| ri.main_id.clone());
            let weight_tracker: Option<Arc<WeightTracker>> = if justknobs::eval(
                "scm/mononoke:git_server_enable_memory_tracking",
                None,
                Some(&repo_name),
            ) {
                Some(WeightTracker::new(
                    request_context.ctx.fb,
                    repo_name.clone(),
                    main_client_id.as_deref(),
                    request_context.ctx.metadata().identities(),
                ))
            } else {
                None
            };
            // Convert to trait object for cross-crate use
            let weight_observer: Option<Arc<dyn WeightObserver>> =
                weight_tracker.map(|t| t as Arc<dyn WeightObserver>);
            // The WeightTracker will automatically decrement active_requests when dropped

            let mut scuba = scuba_handler.to_scuba(&request_context.ctx);
            let mut perf_scuba = scuba.clone();
            let writer_future = async move {
                if delta_form == DeltaForm::OnlyOffset {
                    progress_writer
                        .send("Packfile will be created using only offset deltas\n".to_string())
                        .await?;
                }
                let fetch_request = args.into_request(
                    concurrency(&request_context),
                    shallow_response,
                    request_context.pushvars.bypass_bookmark_cache(),
                    delta_form,
                );
                let request_signature = fetch_request.hash_heads_and_bases();
                let response_stream = fetch_response(
                    request_context.ctx.clone(),
                    &request_context.repo,
                    fetch_request,
                    progress_writer,
                    perf_scuba.clone(),
                    weight_observer.clone(),
                )
                .await?;
                perf_scuba.add(MononokeGitScubaKey::NWants, n_wants);
                perf_scuba.add(MononokeGitScubaKey::NHaves, n_haves);
                packfile_stats_to_scuba(&response_stream, &mut perf_scuba, request_signature);
                let mut pack_writer = PackfileWriter::new(
                    WrapperSender::new(writer),
                    response_stream.num_objects() as u32,
                    5000,
                    delta_form,
                    weight_observer,
                );
                pack_writer.write(response_stream.items).await?;
                pack_writer.finish().await?;
                anyhow::Ok(())
            };
            match writer_future.await {
                Ok(_) => anyhow::Ok(()),
                Err(e) => {
                    STATS::packfile_read_error.add_value(1);
                    scuba.add(MononokeGitScubaKey::PackfileReadError, format!("{e:?}"));
                    scuba.add("log_tag", "Packfile Read Error");
                    scuba.unsampled();
                    scuba.log();
                    error_writer.send(format!("{e:?}")).await?;
                    Ok(())
                }
            }
        }
    };

    let bytes_stream = ResponseStream::new(with_packfile_generator(bytes_stream, generator))
        .end_on_err::<anyhow::Error>();
    let body = StreamBody::new(bytes_stream, mime::APPLICATION_OCTET_STREAM);
    Ok(body)
}

fn with_packfile_generator(
    stream: impl Stream<Item = Result<Bytes>> + Send + 'static,
    generator: impl Future<Output = Result<()>> + Send + 'static,
) -> impl Stream<Item = Result<Bytes>> {
    PackfileResponseStream {
        stream,
        producer: Some(AbortOnDropHandle::new(mononoke::spawn_task(generator))),
    }
}

#[pin_project]
struct PackfileResponseStream<S> {
    #[pin]
    stream: S,
    producer: Option<AbortOnDropHandle<Result<()>>>,
}

impl<S: Stream<Item = Result<Bytes>>> Stream for PackfileResponseStream<S> {
    type Item = Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        if this.producer.is_none() {
            return Poll::Ready(None);
        }
        let item = ready!(this.stream.poll_next(cx));
        if !matches!(&item, Some(Ok(_))) {
            drop(this.producer.take());
        }
        Poll::Ready(item)
    }
}

#[cfg(test)]
mod cancellation_tests;

/// Checks if there are any messages that should be displayed to the user when
/// running `git pull` on this repo.
async fn git_fetch_message(_request_context: &RepositoryRequestContext) -> Result<Option<String>> {
    Ok(None)
}

/// Generate packline encoded error response that Git client understands
async fn git_error_message(
    error: &anyhow::Error,
) -> Result<impl TryIntoResponse + use<>, HttpError> {
    let error_message = format!("{error:?}");
    let mut buf = Vec::with_capacity(error_message.len());
    write_error_channel(error_message.as_ref(), &mut buf)
        .await
        .map_err(HttpError::e500)?;
    Ok(BytesBody::new(Bytes::from(buf), mime::TEXT_PLAIN))
}

fn packfile_stats_to_scuba(
    response: &FetchResponse<'_>,
    scuba: &mut MononokeScubaSampleBuilder,
    request_signature: String,
) {
    scuba.add(
        MononokeGitScubaKey::PackfileCommitCount,
        response.num_commits,
    );
    scuba.add(
        MononokeGitScubaKey::PackfileTreeAndBlobCount,
        response.num_trees_and_blobs,
    );
    scuba.add(MononokeGitScubaKey::PackfileTagCount, response.num_tags);
    scuba.add(MononokeGitScubaKey::RequestSignature, request_signature);
    scuba.add("log_tag", "Packfile stats");
    scuba.unsampled();
    scuba.log();
}
