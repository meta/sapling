/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use core::str;

use anyhow::Error;
use bytes::Bytes;
use git_symbolic_refs::GitSymbolicRefsRef;
use gix_hash::Kind;
use gix_hash::ObjectId;
use gotham::helpers::http::Body;
use gotham::mime;
use gotham::state::FromState;
use gotham::state::State;
use gotham_ext::error::HttpError;
use gotham_ext::middleware::ScubaMiddlewareState;
use gotham_ext::response::BytesBody;
use gotham_ext::response::TryIntoResponse;
use http::HeaderMap;
use http::Response;
use packetline::encode::flush_to_write;
use packetline::encode::write_text_packetline;
use protocol::generator::ls_refs_response;
use protocol::types::LsRefsRequest;
use protocol::types::RefTarget;
use protocol::types::RefsSource;
use protocol::types::RequestedRefs;
use protocol::types::RequestedSymrefs;
use protocol::types::TagInclusion;
use protocol::types::ref_line;

use crate::model::GitMethod;
use crate::model::GitMethodInfo;
use crate::model::RepositoryParams;
use crate::model::RepositoryRequestContext;
use crate::model::ResponseType;
use crate::model::Service;
use crate::model::ServiceType;

const UPLOAD_PACK_CAPABILITIES: &[&str] = &[
    "ls-refs=unborn",
    "fetch=shallow wait-for-done filter",
    "ref-in-want",
    "object-format=sha1",
];
const RECEIVE_PACK_CAPABILITIES: &str =
    "report-status atomic delete-refs quiet ofs-delta object-format=sha1";
const V1_UPLOAD_PACK_CAPABILITIES: &[&str] = &[
    "multi_ack_detailed",
    "side-band-64k",
    "thin-pack",
    "ofs-delta",
    "shallow",
    "no-progress",
    "include-tag",
    "object-format=sha1",
    "no-done",
];
const BUNDLE_URI_CAPABILITY: &str = "bundle-uri";
const USE_BUNDLE_URI: &str = "x-git-use-bundle-uri";
const ENABLE_V1_PROTOCOL: &str = "scm/mononoke:git_server_enable_v1_protocol";

const VERSION: &str = "2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitProtocolVersion {
    V1,
    V2,
}

impl std::fmt::Display for GitProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::V1 => write!(f, "1"),
            Self::V2 => write!(f, "2"),
        }
    }
}

fn protocol_version_from_headers(headers: &HeaderMap) -> GitProtocolVersion {
    let version = headers
        .get("Git-Protocol")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split_whitespace()
                .find_map(|token| token.strip_prefix("version="))
        })
        .and_then(|v| v.parse::<u8>().ok());
    match version {
        Some(2) => GitProtocolVersion::V2,
        _ => GitProtocolVersion::V1,
    }
}

async fn advertise_capability(
    request_context: RepositoryRequestContext,
    service_type: Service,
    use_bundle_uri: bool,
    protocol_version: GitProtocolVersion,
) -> Result<Vec<u8>, Error> {
    let client_trusted = !request_context.ctx.metadata().client_untrusted();
    let bundle_trusted_only = request_context.bundle_uri_trusted_only();

    let mut output = Vec::new();

    write_text_packetline(format!("# service={service_type}").as_bytes(), &mut output).await?;
    flush_to_write(&mut output).await?;
    match (service_type, protocol_version) {
        (Service::GitUploadPack, GitProtocolVersion::V1) => {
            read_advertisement_v1(request_context, &mut output).await?
        }
        (Service::GitUploadPack, GitProtocolVersion::V2) => {
            read_advertisement(
                &mut output,
                client_trusted,
                use_bundle_uri,
                bundle_trusted_only,
            )
            .await?
        }
        (Service::GitReceivePack, _) => write_advertisement(request_context, &mut output).await?,
    }
    flush_to_write(&mut output).await?;
    Ok(output)
}

async fn read_advertisement(
    output: &mut Vec<u8>,
    client_trusted: bool,
    use_bundle_uri: bool,
    bundle_trusted_only: bool,
) -> Result<(), Error> {
    write_text_packetline(format!("version {VERSION}").as_bytes(), output).await?;
    for capability in UPLOAD_PACK_CAPABILITIES {
        write_text_packetline(capability.as_bytes(), output).await?;
    }
    let client_validated = !bundle_trusted_only || client_trusted;
    if use_bundle_uri && client_validated {
        write_text_packetline(BUNDLE_URI_CAPABILITY.as_bytes(), output).await?;
    }
    Ok(())
}

async fn read_advertisement_v1(
    request_context: RepositoryRequestContext,
    output: &mut Vec<u8>,
) -> Result<(), Error> {
    let ls_refs_request = LsRefsRequest {
        requested_symrefs: RequestedSymrefs::ExcludeAll,
        requested_refs: RequestedRefs::all(),
        tag_inclusion: TagInclusion::Peeled,
        refs_source: RefsSource::WarmBookmarksCache,
    };

    let response =
        ls_refs_response(&request_context.ctx, &request_context.repo, ls_refs_request).await?;

    let mut refs_map = response.included_refs;

    let mut capabilities: Vec<String> = V1_UPLOAD_PACK_CAPABILITIES
        .iter()
        .map(|c| c.to_string())
        .collect();

    match request_context
        .repo
        .git_symbolic_refs()
        .get_ref_by_symref(&request_context.ctx, "HEAD".to_string())
        .await
    {
        Ok(Some(head_ref)) => {
            let target_ref = head_ref.ref_name_with_type();
            if let Some(target) = refs_map.get(&target_ref) {
                capabilities.push(format!("symref=HEAD:{target_ref}"));
                refs_map.insert("HEAD".to_string(), RefTarget::Plain(*target.id()));
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("Failed to look up HEAD symref: {:#}", e),
    }

    let mut refs: Vec<_> = refs_map.into_iter().collect();
    refs.sort_by(|a, b| a.0.cmp(&b.0));

    let capabilities_str = capabilities.join(" ");

    let mut refs_iter = refs.into_iter();
    match refs_iter.next() {
        Some((ref_name, target)) => {
            let first_ref_line = ref_line(ref_name.as_str(), &target);
            write_text_packetline(
                format!("{first_ref_line}\0{capabilities_str}").as_bytes(),
                output,
            )
            .await?;
        }
        None => {
            write_text_packetline(
                format!(
                    "{} capabilities^{{}}\0{}",
                    ObjectId::null(Kind::Sha1),
                    capabilities_str
                )
                .as_bytes(),
                output,
            )
            .await?;
        }
    }
    for (ref_name, target) in refs_iter {
        write_text_packetline(ref_line(ref_name.as_str(), &target).as_bytes(), output).await?;
    }
    Ok(())
}

async fn write_advertisement(
    request_context: RepositoryRequestContext,
    output: &mut Vec<u8>,
) -> Result<(), Error> {
    let mut refs: Vec<_> = ls_refs_response(
        &request_context.ctx,
        &request_context.repo,
        LsRefsRequest::write_advertisement(),
    )
    .await?
    .included_refs
    .into_iter()
    .collect();
    refs.sort_by(|a, b| a.0.cmp(&b.0));

    let mut refs = refs.into_iter();
    match refs.next() {
        Some((ref_name, target)) => {
            let first_ref_line = ref_line(ref_name.as_str(), &target);
            write_text_packetline(
                format!("{first_ref_line}\0{RECEIVE_PACK_CAPABILITIES}").as_bytes(),
                output,
            )
            .await?;
        }
        None => {
            write_text_packetline(
                format!(
                    "{} capabilities^{{}}\0{}",
                    ObjectId::null(Kind::Sha1),
                    RECEIVE_PACK_CAPABILITIES
                )
                .as_bytes(),
                output,
            )
            .await?;
        }
    }
    for (ref_name, target) in refs {
        write_text_packetline(ref_line(ref_name.as_str(), &target).as_bytes(), output).await?;
    }
    Ok(())
}

pub async fn capability_advertisement(state: &mut State) -> Result<Response<Body>, HttpError> {
    let service_type = ServiceType::borrow_from(state).service;
    let repo_name = RepositoryParams::borrow_from(state).repo_name();
    let git_method_info = match service_type {
        Service::GitUploadPack => {
            GitMethodInfo::standard(repo_name.clone(), GitMethod::AdvertiseRead)
        }
        Service::GitReceivePack => {
            GitMethodInfo::standard(repo_name.clone(), GitMethod::AdvertiseWrite)
        }
    };
    let request_context = RepositoryRequestContext::instantiate(state, git_method_info).await?;

    let headers = HeaderMap::borrow_from(state);
    let force_bundle_uri = headers
        .get(USE_BUNDLE_URI)
        .and_then(|x| x.to_str().ok())
        .and_then(|x| x.parse().ok());

    let v1_enabled = justknobs::eval(ENABLE_V1_PROTOCOL, None, None);
    let protocol_version = if v1_enabled {
        protocol_version_from_headers(headers)
    } else {
        GitProtocolVersion::V2
    };

    ScubaMiddlewareState::try_borrow_add(state, "protocol_version", protocol_version.to_string());

    let use_bundle_uri = request_context.bundle_uri_configured()
        && force_bundle_uri
            .unwrap_or_else(|| request_context.repo.repo_config.enable_git_bundle_uri);

    let output = advertise_capability(
        request_context,
        service_type,
        use_bundle_uri,
        protocol_version,
    )
    .await
    .map_err(HttpError::e500)?;
    let service_type = ServiceType::borrow_from(state).service;
    state.put(service_type.to_owned());
    state.put(ResponseType::Advertisement);
    BytesBody::new(Bytes::from(output), mime::TEXT_PLAIN)
        .try_into_response(state)
        .map_err(HttpError::e500)
}
