/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io;
use std::io::Cursor;
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::task;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use base64::Engine;
use bookmarks::BookmarksRef;
use bookmarks_cache::BookmarksCacheRef;
use bytes::Bytes;
#[cfg(fbcode_build)]
use clientinfo::CLIENT_INFO_HEADER;
#[cfg(fbcode_build)]
use clientinfo::ClientEntryPoint;
#[cfg(fbcode_build)]
use clientinfo::ClientInfo;
use context::CoreContext;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use gotham_ext::handler::SlapiCommitIdentityScheme;
#[cfg(fbcode_build)]
use gotham_ext::middleware::metadata::ingress_request_identities_from_headers;
use gotham_ext::socket_data::TlsSocketData;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use http::Request;
use http::Response;
use http::Uri;
use http_body_util::BodyExt as _;
use http_body_util::Full;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::Incoming;
use hyper::ext;
use hyper_util::rt::TokioIo;
use metadata::Metadata;
use mononoke_api::Repo;
use percent_encoding::percent_decode;
use qps::Qps;
use session_id::generate_session_id;
use sha1::Digest;
use sha1::Sha1;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tower_service::Service;
use tracing::debug;
use tracing::error;
use tracing::trace;

use crate::connection_acceptor;
use crate::connection_acceptor::AcceptedConnection;
use crate::connection_acceptor::Acceptor;
use crate::connection_acceptor::FramedConn;
use crate::connection_acceptor::MononokeStream;

const HEADER_CLIENT_COMPRESSION: &str = "x-client-compression";
const HEADER_CLIENT_DEBUG: &str = "x-client-debug";
const HEADER_WEBSOCKET_KEY: &str = "sec-websocket-key";
const HEADER_WEBSOCKET_ACCEPT: &str = "sec-websocket-accept";
const HEADER_MONONOKE_ENCODING: &str = "x-mononoke-encoding";
const HEADER_MONONOKE_HOST: &str = "x-mononoke-host";
const HEADER_REVPROXY_REGION: &str = "x-fb-revproxy-region";

// See https://tools.ietf.org/html/rfc6455#section-1.3
const WEBSOCKET_MAGIC_KEY: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

#[derive(Error, Debug)]
pub enum HttpError {
    #[error("Bad request")]
    BadRequest(#[source] Error),

    #[error("Forbidden")]
    Forbidden,

    #[error("Not found")]
    NotFound,

    #[error("Method not allowed")]
    MethodNotAllowed,

    #[error("Internal server error")]
    InternalServerError(#[source] Error),
}

impl HttpError {
    pub fn internal(e: impl Into<Error>) -> Self {
        Self::InternalServerError(e.into())
    }

    pub fn http_response(&self) -> http::Result<Response<UnsyncBoxBody<Bytes, io::Error>>> {
        let status = match self {
            Self::BadRequest(..) => http::StatusCode::BAD_REQUEST,
            Self::Forbidden => http::StatusCode::FORBIDDEN,
            Self::NotFound => http::StatusCode::NOT_FOUND,
            Self::MethodNotAllowed => http::StatusCode::METHOD_NOT_ALLOWED,
            Self::InternalServerError(..) => http::StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = match self {
            Self::BadRequest(e) => {
                UnsyncBoxBody::new(Full::from(format!("{e:#}")).map_err(|never| match never {}))
            }
            Self::Forbidden => UnsyncBoxBody::default(),
            Self::NotFound => UnsyncBoxBody::default(),
            Self::MethodNotAllowed => UnsyncBoxBody::default(),
            Self::InternalServerError(e) => {
                UnsyncBoxBody::new(Full::from(format!("{e:#}")).map_err(|never| match never {}))
            }
        };

        Response::builder().status(status).body(body)
    }
}

pub struct MononokeHttpService<S> {
    pub conn: AcceptedConnection,
    sock: PhantomData<S>,
}

impl<S> MononokeHttpService<S> {
    pub fn new(conn: AcceptedConnection) -> Self {
        Self {
            conn,
            sock: PhantomData,
        }
    }
}

impl<S> Clone for MononokeHttpService<S> {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            sock: PhantomData,
        }
    }
}

fn bump_qps(headers: &HeaderMap, qps: Option<&Qps>) -> Result<()> {
    let qps = match qps {
        Some(qps) => qps,
        None => return Ok(()),
    };
    match headers.get(HEADER_REVPROXY_REGION) {
        Some(proxy_region) => {
            qps.bump(proxy_region.to_str()?)?;
            Ok(())
        }
        None => Err(anyhow!("No {HEADER_REVPROXY_REGION:?} header.")),
    }
}

/**
 * websocket upgrades:
 *  – http/1.1 (RFC6455) uses "upgrade" header
 *  – http/2 (RFC8441) uses CONNECT method & :protocol pseudo header
 */
fn is_websocket_req(is_h2: bool, req: &Request<Incoming>) -> Result<bool, HttpError> {
    let upgrade_protocol: &str = if is_h2 {
        req.extensions()
            .get::<ext::Protocol>()
            .map(|p| p.as_str())
            .unwrap_or_default()
    } else {
        req.headers()
            .get(http::header::UPGRADE)
            .as_ref()
            .map(|h| h.to_str())
            .transpose()
            .with_context(|| {
                // NOTE: We're just stringifying here: the borrow is fine.
                #[allow(clippy::borrow_interior_mutable_const)]
                let header = &http::header::UPGRADE;
                format!("Invalid header: {header}")
            })
            .map_err(HttpError::BadRequest)?
            .unwrap_or_default()
    };
    Ok(upgrade_protocol == "websocket")
}

impl<S> MononokeHttpService<S>
where
    S: MononokeStream,
{
    async fn handle(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<UnsyncBoxBody<Bytes, io::Error>>, HttpError> {
        if req.method() == Method::GET
            && (req.uri().path() == "/" || req.uri().path() == "/health_check")
        {
            let res: &[u8] = if self.acceptor().will_exit.load(Ordering::Relaxed) {
                b"EXITING"
            } else {
                b"I_AM_ALIVE"
            };

            let res = Response::builder()
                .status(http::StatusCode::OK)
                .body(UnsyncBoxBody::new(
                    Full::new(Bytes::from_static(res)).map_err(|never| match never {}),
                ))
                .map_err(HttpError::internal)?;

            return Ok(res);
        }

        if is_websocket_req(self.conn.is_h2, &req)? {
            return self.handle_websocket_request(req).await;
        }

        let (req, body) = req.into_parts();

        if req.uri.path() == "/netspeedtest" {
            return crate::netspeedtest::handle(req.method, &req.headers, body).await;
        }

        if let Some(path) = req.uri.path().strip_prefix("/control") {
            return self.handle_control_request(req.method, path).await;
        }

        if let Some((flavour, path_and_query)) = req
            .uri
            .path_and_query()
            .as_ref()
            .and_then(|pq| pq.as_str().strip_prefix("/"))
            .and_then(|pq| pq.split_once('/'))
        {
            let pq = http::uri::PathAndQuery::from_str(&format!("/{path_and_query}"))
                .context("Error translating SaplingRemoteAPI request path")
                .map_err(HttpError::internal)?;
            match flavour {
                "edenapi" | "slapi" => {
                    return self
                        .handle_eden_api_request(req, pq, body, SlapiCommitIdentityScheme::Hg)
                        .await;
                }
                "slapigit" => {
                    return self
                        .handle_eden_api_request(req, pq, body, SlapiCommitIdentityScheme::Git)
                        .await;
                }
                _ => {
                    return Err(HttpError::BadRequest(anyhow!(
                        "Unknown SaplingRemoteAPI flavour"
                    )));
                }
            }
        }
        Err(HttpError::NotFound)
    }

    async fn handle_websocket_request(
        &self,
        mut req: Request<Incoming>,
    ) -> Result<Response<UnsyncBoxBody<Bytes, io::Error>>, HttpError> {
        let reponame_urlencoded = req.uri().path().trim_matches('/').to_string();

        let reponame = percent_decode(reponame_urlencoded.as_bytes())
            .decode_utf8()
            .context("reponame must be url-encoded utf-8")
            .map_err(HttpError::BadRequest)?
            .into_owned();

        let websocket_key = calculate_websocket_accept(req.headers());

        let status_code = if self.conn.is_h2 {
            http::StatusCode::OK
        } else {
            http::StatusCode::SWITCHING_PROTOCOLS
        };

        let mut builder = Response::builder()
            .status(status_code)
            .header(http::header::CONNECTION, "upgrade")
            .header(http::header::UPGRADE, "websocket")
            .header(HEADER_WEBSOCKET_ACCEPT, websocket_key);

        let metadata = h2m::try_convert_headers_to_metadata(&self.conn, req.headers())
            .await
            .context("Invalid metadata")
            .map_err(HttpError::BadRequest)?;

        let zstd_level = justknobs::get_as::<i32>("scm/mononoke:zstd_compression_level", None);
        let compression = match req.headers().get(HEADER_CLIENT_COMPRESSION) {
            Some(header_value) => match header_value.as_bytes() {
                b"zstd=stdin" if zstd_level > 0 => Ok(Some(zstd_level)),
                header_value_bytes => Err(anyhow!(
                    "'{}' is not a recognized compression value",
                    String::from_utf8_lossy(header_value_bytes),
                )),
            },
            None => Ok(None),
        }
        .map_err(HttpError::BadRequest)?;

        match compression {
            Some(zstd_level) => {
                builder = builder.header(HEADER_MONONOKE_ENCODING, format!("zstd={zstd_level}"));
            }
            _ => {}
        };

        let res = builder
            .body(UnsyncBoxBody::default())
            .map_err(HttpError::internal)?;

        let this = self.clone();

        let fut = async move {
            let io = hyper::upgrade::on(&mut req)
                .await
                .context("Failed to upgrade connection")?;

            let (mut rx, tx) = tokio::io::split(TokioIo::new(io));

            // Sometimes server rejects client's request quickly. So quickly,
            // that right after sending 101 Switching Protocols, it immediately
            // sends wireproto error message to the client on the stderr channel,
            // and terminates the connection right away.
            //
            // Client sees HTTP 101 and assumes it can speak wireproto to the server.
            // It tries to send wireproto's "hello" command, but fails miserably, because
            // the connection has already been closed.
            //
            // Example:
            // https://pxl.cl/1XcnH
            //
            // Lines bellow make server wait for the client to send any data before
            // any wireproto handling can take place. Normally server that speaks just
            // wireproto shouldn't send anything to the client until it sees "hello".
            // Here we try to replicate that behavior by making sure the client
            // sent something. We assume it's wireproto's "hello".
            let mut buffer = [0; 1];
            rx.read_exact(&mut buffer).await?;
            let rx = AsyncReadExt::chain(Cursor::new(buffer), rx);

            let framed = FramedConn::setup(rx, tx, compression)?;

            connection_acceptor::handle_wireproto(this.conn, framed, reponame, metadata)
                .await
                .context("Failed to handle_wireproto")?;

            Result::<_, Error>::Ok(())
        };

        // Spawning concurrent task handling wireproto
        self.conn
            .pending
            .spawn_task(fut, "Failed to handle websocket channel");

        // Returning with HTTP 101. The task spawned above will handle
        // upgraded connection.
        Ok(res)
    }

    async fn handle_control_request(
        &self,
        method: Method,
        path: &str,
    ) -> Result<Response<UnsyncBoxBody<Bytes, io::Error>>, HttpError> {
        if method != Method::POST {
            return Err(HttpError::MethodNotAllowed);
        }

        if !self.acceptor().enable_http_control_api {
            return Err(HttpError::Forbidden);
        }

        let ok = Response::builder()
            .status(http::StatusCode::OK)
            .body(UnsyncBoxBody::default())
            .map_err(HttpError::internal)?;

        if path == "/drop_bookmarks_cache" {
            for repo in self.acceptor().mononoke.repos() {
                repo.bookmarks().drop_caches();
            }

            return Ok(ok);
        }

        if path == "/sync_warm_bookmarks_cache" {
            let ctx = CoreContext::new(self.acceptor().fb);
            for repo in self.acceptor().mononoke.repos() {
                repo.bookmarks_cache().sync(&ctx).await;
            }
            return Ok(ok);
        }

        if path == "/force_update_configerator" {
            self.acceptor().config_store.force_update_configs();
            return Ok(ok);
        }

        Err(HttpError::NotFound)
    }

    async fn handle_eden_api_request(
        &self,
        mut req: http::request::Parts,
        pq: http::uri::PathAndQuery,
        body: Incoming,
        flavour: SlapiCommitIdentityScheme,
    ) -> Result<Response<UnsyncBoxBody<Bytes, io::Error>>, HttpError> {
        let mut uri_parts = req.uri.into_parts();

        uri_parts.path_and_query = Some(pq);

        req.uri = Uri::from_parts(uri_parts)
            .context("Error translating SaplingRemoteAPI request")
            .map_err(HttpError::internal)?;

        if let Err(e) = bump_qps(&req.headers, self.acceptor().qps.as_deref()) {
            trace!("Failed to bump QPS: {:?}", e);
        }

        let tls_socket_data = if self.conn.is_trusted {
            TlsSocketData::trusted_proxy((*self.conn.identities).clone())
        } else {
            TlsSocketData::authenticated_identities((*self.conn.identities).clone())
        };

        let req = Request::from_parts(req, UnsyncBoxBody::new(body.map_err(io::Error::other)));

        let res = self
            .acceptor()
            .edenapi
            .clone()
            .into_service_with_state(self.conn.pending.addr, Some(tls_socket_data), flavour)
            .call_gotham(req)
            .await;

        Ok(res)
    }

    fn acceptor(&self) -> &Acceptor<Repo> {
        &self.conn.pending.acceptor
    }
}

impl<S> Service<Request<Incoming>> for MononokeHttpService<S>
where
    S: MononokeStream,
{
    type Response = Response<UnsyncBoxBody<Bytes, io::Error>>;
    type Error = http::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut task::Context<'_>) -> task::Poll<Result<(), Self::Error>> {
        task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Incoming>) -> Self::Future {
        let this = self.clone();

        async move {
            let method = req.method().clone();
            let uri = req.uri().clone();
            debug!("{} {}", method, uri);

            this.handle(req)
                .await
                .map(|mut res| {
                    match HeaderValue::from_str(this.conn.pending.acceptor.server_hostname.as_str())
                    {
                        Ok(header) => {
                            res.headers_mut().insert(HEADER_MONONOKE_HOST, header);
                        }
                        Err(e) => {
                            error!(
                                "http service error: can't set {} header: {}",
                                HEADER_MONONOKE_HOST,
                                anyhow::Error::from(e),
                            );
                        }
                    };
                    res
                })
                .or_else(|e| {
                    let res = e.http_response();

                    error!(
                        "http service error: {} {}: {:#}",
                        method,
                        uri,
                        anyhow::Error::from(e)
                    );

                    res
                })
            // NOTE: If we fail to even generate the response here, this will crash
            // serve_connection in Hyper, so we don't actually need to log this here.
        }
        .boxed()
    }
}

// See https://tools.ietf.org/html/rfc6455#section-1.3
fn calculate_websocket_accept(headers: &HeaderMap<HeaderValue>) -> String {
    let mut sha1 = Sha1::new();

    // This is OK to fall back to empty, because we only need to give
    // this header, if it's asked for. In case of hg<->mononoke with
    // no Proxygen in between, this header will be missing and the result
    // ignored.
    if let Some(header) = headers.get(HEADER_WEBSOCKET_KEY) {
        sha1.update(header.as_ref());
    }
    sha1.update(WEBSOCKET_MAGIC_KEY.as_bytes());
    let hash: [u8; 20] = sha1.finalize().into();
    base64::engine::general_purpose::STANDARD.encode(hash)
}

#[cfg(not(fbcode_build))]
mod h2m {
    use super::*;

    pub async fn try_convert_headers_to_metadata(
        conn: &AcceptedConnection,
        headers: &HeaderMap<HeaderValue>,
    ) -> Result<Metadata> {
        let debug = headers.contains_key(HEADER_CLIENT_DEBUG);

        let _ = conn.pending.acceptor.common_config; // Fix compiler warning in OSS build

        Ok(Metadata::new(
            Some(&generate_session_id().to_string()),
            (*conn.identities).clone(),
            debug,
            metadata::security::is_client_untrusted(|h| {
                Ok(headers
                    .get(h)
                    .map(|h| h.to_str().map(|s| s.to_owned()))
                    .transpose()?)
            })?,
            Some(conn.pending.addr.ip()),
            Some(conn.pending.addr.port()),
        )
        .await)
    }
}

#[cfg(fbcode_build)]
mod h2m {
    use std::net::IpAddr;

    use cats::try_get_cats_idents;
    use cats_constants::X_FORWARDED_CATS_HEADER;
    use percent_encoding::percent_decode;
    use permission_checker::MononokeIdentity;

    use super::*;

    const HEADER_ENCODED_CLIENT_IDENTITY: &str = "x-fb-validated-client-encoded-identity";
    const HEADER_CLIENT_IP: &str = "tfb-orig-client-ip";
    const HEADER_CLIENT_PORT: &str = "tfb-orig-client-port";
    fn metadata_populate_trusted(
        metadata: &mut Metadata,
        headers: &HeaderMap<HeaderValue>,
    ) -> Result<()> {
        if let Some(cats) = headers.get(X_FORWARDED_CATS_HEADER) {
            metadata
                .add_raw_encoded_cats(cats.to_str().context("Invalid encoded cats")?.to_string());
        }
        let src_region = headers
            .get(HEADER_REVPROXY_REGION)
            .and_then(|r| r.to_str().ok().map(|r| r.to_string()));

        if let Some(src_region) = src_region {
            metadata.add_revproxy_region(src_region);
        }
        Ok(())
    }

    /// Used only for wireproto handling.
    pub async fn try_convert_headers_to_metadata(
        conn: &AcceptedConnection,
        headers: &HeaderMap<HeaderValue>,
    ) -> Result<Metadata> {
        let debug = headers.contains_key(HEADER_CLIENT_DEBUG);
        let internal_identity = &conn.pending.acceptor.common_config.internal_identity;
        let is_trusted = conn.is_trusted;
        let client_info: Option<ClientInfo> = headers
            .get(CLIENT_INFO_HEADER)
            .and_then(|h| h.to_str().ok())
            .and_then(|ci| serde_json::from_str(ci).ok());

        // CATs are verifiable - we know that only the signer could have
        // generated them. We extract the signer's identity. The connecting
        // party doesn't have to be trusted.
        //
        // Invalid CAT tokens are silently discarded (with a warn log emitted
        // by `try_get_cats_idents`) so that the request proceeds without
        // CAT-derived identities rather than failing the whole request.
        let cats_identities = internal_identity.as_ref().and_then(|identity| {
            try_get_cats_idents(conn.pending.acceptor.fb.clone(), headers, identity)
        });

        if is_trusted {
            if let (Some(encoded_identities), Some(client_address), Some(client_port)) = (
                headers.get(HEADER_ENCODED_CLIENT_IDENTITY),
                headers.get(HEADER_CLIENT_IP),
                headers.get(HEADER_CLIENT_PORT),
            ) {
                let json_identities = percent_decode(encoded_identities.as_ref())
                    .decode_utf8()
                    .context("Invalid encoded identities")?;

                let mut identities = MononokeIdentity::try_from_json_encoded(&json_identities)
                    .context("Invalid identities")?;
                let ip_addr = client_address
                    .to_str()?
                    .parse::<IpAddr>()
                    .context("Invalid IP Address")?;

                let client_port = client_port
                    .to_str()?
                    .parse::<u16>()
                    .context("Invalid client port")?;

                identities.extend(cats_identities.unwrap_or_default());

                let mut metadata = Metadata::new(
                    Some(&generate_session_id().to_string()),
                    identities,
                    debug,
                    metadata::security::is_client_untrusted(|h| {
                        Ok(headers
                            .get(h)
                            .map(|h| h.to_str().map(|s| s.to_owned()))
                            .transpose()?)
                    })?,
                    Some(ip_addr),
                    Some(client_port),
                )
                .await;

                let client_info = client_info.unwrap_or_else(|| {
                    ClientInfo::default_with_entry_point(ClientEntryPoint::SaplingRemoteApi)
                });
                metadata.add_client_info(client_info);

                metadata_populate_trusted(&mut metadata, headers)?;
                return Ok(metadata);
            }
        }

        let mut identities = cats_identities.unwrap_or_default();
        identities.extend(conn.identities.iter().cloned());

        if conn.mtls_disabled {
            identities.extend(
                ingress_request_identities_from_headers(headers)
                    .unwrap()
                    .iter()
                    .cloned(),
            );
        }

        // Generic fallback
        let mut metadata = Metadata::new(
            Some(&generate_session_id().to_string()),
            identities,
            debug,
            metadata::security::is_client_untrusted(|h| {
                Ok(headers
                    .get(h)
                    .map(|h| h.to_str().map(|s| s.to_owned()))
                    .transpose()?)
            })?,
            Some(conn.pending.addr.ip()),
            Some(conn.pending.addr.port()),
        )
        .await;

        let client_info = client_info.unwrap_or_else(|| {
            ClientInfo::default_with_entry_point(ClientEntryPoint::SaplingRemoteApi)
        });
        metadata.add_client_info(client_info);

        Ok(metadata)
    }
}
