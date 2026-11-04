/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::prelude::*;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::AcqRel;
use std::time::Duration;
use std::time::SystemTime;

use clientinfo::CLIENT_INFO_HEADER;
use curl::easy::HttpVersion;
use curl::easy::List;
use curl::easy::PostRedirections;
use curl_sys::CURLOPTTYPE_LONG;
use curl_sys::CURLoption;
use http::header;
use lru_cache::LruCache;
use maplit::hashmap;
use openssl::pkcs12::Pkcs12;
use openssl::pkey::PKey;
use openssl::x509::X509;
use parking_lot::Mutex;
use parking_lot::RwLock;
use serde::Serialize;
use url::Url;

use crate::AuthResolver;
use crate::Easy2H;
use crate::claimer::RequestClaim;
use crate::claimer::RequestClaimer;
use crate::errors::HttpClientError;
use crate::errors::maybe_add_os_error;
use crate::event_listeners::RequestCreationEventListeners;
use crate::event_listeners::RequestEventListeners;
use crate::handler::Buffered;
use crate::handler::HandlerExt;
use crate::handler::Streaming;
use crate::receiver::Receiver;
use crate::response::Response;

pub const FETCH_CAUSE_HEADER: &str = "X-Fetch-Cause";
pub const FETCH_FROM_CAS_ATTEMPTED_HEADER: &str = "X-Fetch-From-CAS-Attempted";

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct MinTransferSpeed {
    pub min_bytes_per_second: u32,
    pub window: Duration,
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Method::Get => "GET",
                Method::Head => "HEAD",
                Method::Post => "POST",
                Method::Put => "PUT",
            }
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Encoding {
    Identity,
    Brotli,
    Deflate,
    Gzip,
    Zstd,
    Other(String),
}

impl Encoding {
    pub fn all() -> Vec<Self> {
        use Encoding::*;
        vec![Zstd, Brotli, Gzip, Deflate]
    }
}

impl<'a> From<&'a str> for Encoding {
    fn from(encoding: &'a str) -> Self {
        use Encoding::*;
        match encoding {
            "identity" => Identity,
            "br" => Brotli,
            "deflate" => Deflate,
            "gzip" => Gzip,
            "zstd" => Zstd,
            other => Other(other.into()),
        }
    }
}

impl AsRef<str> for Encoding {
    fn as_ref(&self) -> &str {
        use Encoding::*;
        match self {
            Identity => "identity",
            Brotli => "br",
            Deflate => "deflate",
            Gzip => "gzip",
            Zstd => "zstd",
            Other(s) => s,
        }
    }
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_ref())
    }
}

/// Metadata about this request.
#[derive(Debug, Clone)]
pub struct RequestInfo {
    id: RequestId,
    url: Url,
    method: Method,
}

impl RequestInfo {
    /// Obtain the URL of the request.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Obtain the HTTP method of the request.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Obtain the request ID
    ///
    /// The ID is automatically assigned and uniquely identifies the requests
    /// in this process.
    pub fn id(&self) -> RequestId {
        self.id
    }
}

/// A subset of the `Request` builder. Preserved in curl types.
/// Expose the request in curl handler callback context.
#[derive(Clone, Debug)]
pub struct RequestContext {
    pub(crate) info: RequestInfo,
    pub(crate) body: Option<Vec<u8>>,
    pub(crate) event_listeners: RequestEventListeners,
    pub(crate) unix_socket_path: Option<String>,
}

/// Identity of a request.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RequestId(usize);

/// A builder struct for HTTP requests, designed to be
/// a more ergonomic API for setting up a curl handle.
#[derive(Clone, Debug)]
pub struct Request {
    ctx: RequestContext,
    claimer: RequestClaimer,
    headers: HashMap<String, String>,
    auth_resolver: Option<AuthResolver>,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    cainfo: Option<PathBuf>,
    connect_timeout: Option<Duration>,
    overall_timeout: Option<Duration>,
    http_version: HttpVersion,
    accept_encoding: Vec<Encoding>,
    min_transfer_speed: Option<MinTransferSpeed>,
    verify_tls_host: bool,
    verify_tls_cert: bool,
    verbose: bool,
    convert_cert: bool,
    read_buffer_size: Option<u64>,
    write_buffer_size: Option<u64>,
    follow_redirects: bool,
    http_proxy_host: Option<String>,
    http_no_proxy: Option<String>,
}

static REQUEST_CREATION_LISTENERS: LazyLock<RwLock<RequestCreationEventListeners>> =
    LazyLock::new(Default::default);

// Attempt to use HTTP/2 by default. Will fall back to HTTP/1.1
// if version negotiation with the server fails or if the binary
// is built against some Curl version that doesn't have HTTP/2 available
static DEFAULT_HTTP_VERSION: LazyLock<HttpVersion> = LazyLock::new(|| {
    if curl::Version::get().feature_http2() {
        HttpVersion::V2
    } else {
        HttpVersion::Any
    }
});

impl RequestContext {
    /// Create a [`RequestContext`].
    pub fn new(url: Url, method: Method) -> Self {
        static ID: AtomicUsize = AtomicUsize::new(0);
        let id = RequestId(ID.fetch_add(1, AcqRel));
        Self {
            info: RequestInfo { id, url, method },
            body: None,
            event_listeners: Default::default(),
            unix_socket_path: None,
        }
    }

    /// Obtain the HTTP url of the request.
    pub fn url(&self) -> &Url {
        self.info.url()
    }

    /// Obtain the HTTP method of the request.
    pub fn method(&self) -> &Method {
        self.info.method()
    }

    /// Obtain the request Id.
    ///
    /// The Id is automatically assigned and uniquely identifies the requests
    /// in this process.
    pub fn id(&self) -> RequestId {
        self.info.id()
    }

    /// Obtain the request metadata.
    pub fn info(&self) -> &RequestInfo {
        &self.info
    }

    /// Set the data to be uploaded in the request body.
    pub fn body<B: Into<Vec<u8>>>(mut self, data: B) -> Self {
        self.body = Some(data.into());
        self
    }

    /// Set the data to be uploaded in the request body.
    pub fn set_body<B: Into<Vec<u8>>>(&mut self, data: B) {
        self.body = Some(data.into());
    }

    /// Provide a way to register event callbacks.
    pub fn event_listeners(&mut self) -> &mut RequestEventListeners {
        &mut self.event_listeners
    }
}

impl Request {
    pub(crate) fn new(url: Url, method: Method, claimer: RequestClaimer) -> Self {
        let ctx = RequestContext::new(url, method);
        Self {
            ctx,
            claimer,
            // Always set Expect so we can disable curl automatically expecting "100-continue".
            // That would require two response reads, which breaks the http_client model.
            headers: hashmap! {
                "Expect".to_string() => "".to_string(),
            },
            auth_resolver: None,
            cert: None,
            key: None,
            cainfo: None,
            connect_timeout: None,
            overall_timeout: None,
            http_version: DEFAULT_HTTP_VERSION.clone(),
            accept_encoding: Vec::new(),
            min_transfer_speed: None,
            verify_tls_host: true,
            verify_tls_cert: true,
            verbose: false,
            convert_cert: false,
            read_buffer_size: None,
            write_buffer_size: None,
            follow_redirects: true,
            http_proxy_host: None,
            http_no_proxy: None,
        }
    }

    /// Obtain the request Id.
    ///
    /// The Id is automatically assigned and uniquely identifies the requests
    /// in this process.
    pub fn id(&self) -> RequestId {
        self.ctx.id()
    }

    /// Get a reference to this request's context.
    ///
    /// The request context contains most of the information about the request,
    /// such as URL, method, etc.
    pub fn ctx(&self) -> &RequestContext {
        &self.ctx
    }

    /// Get a mutable reference to this request's context.
    pub fn ctx_mut(&mut self) -> &mut RequestContext {
        &mut self.ctx
    }

    /// Set the data to be uploaded in the request body.
    pub fn body<B: Into<Vec<u8>>>(mut self, data: B) -> Self {
        self.set_body(data);
        self
    }

    /// Set the data to be uploaded in the request body.
    pub fn set_body<B: Into<Vec<u8>>>(&mut self, data: B) -> &mut Self {
        self.ctx.set_body(data);
        self
    }

    /// Set the http version for this request. Defaults to HTTP/2.
    pub fn http_version(mut self, version: HttpVersion) -> Self {
        self.set_http_version(version);
        self
    }

    /// Set the http version for this request. Defaults to HTTP/2.
    pub fn set_http_version(&mut self, version: HttpVersion) -> &mut Self {
        self.http_version = version;
        self
    }

    /// Specify the content compression formats the client should advertise to
    /// the server. By default, this will be every format supported by libcurl.
    pub fn accept_encoding(mut self, formats: impl IntoIterator<Item = Encoding>) -> Self {
        self.set_accept_encoding(formats);
        self
    }

    /// Specify the content compression formats the client should advertise to
    /// the server. By default, this will be every format supported by libcurl.
    pub fn set_accept_encoding(
        &mut self,
        formats: impl IntoIterator<Item = Encoding>,
    ) -> &mut Self {
        self.accept_encoding = formats.into_iter().collect();
        self
    }

    /// Set transfer speed options for this request.
    pub fn min_transfer_speed(mut self, min_transfer_speed: Option<MinTransferSpeed>) -> Self {
        self.min_transfer_speed = min_transfer_speed;
        self
    }

    /// Set transfer speed options for this request.
    pub fn set_min_transfer_speed(&mut self, min_transfer_speed: MinTransferSpeed) -> &mut Self {
        self.min_transfer_speed = Some(min_transfer_speed);
        self
    }

    /// Serialize the given value as JSON and use it as the request body.
    pub fn json<S: Serialize>(mut self, value: &S) -> Result<Self, serde_json::Error> {
        self.set_json_body(value)?;
        Ok(self)
    }

    /// Serialize the given value as JSON and use it as the request body.
    pub fn set_json_body<S: Serialize>(
        &mut self,
        value: &S,
    ) -> Result<&mut Self, serde_json::Error> {
        self.set_header("Content-Type", "application/json")
            .set_body(serde_json::to_vec(value)?);
        Ok(self)
    }

    /// Serialize the given value as CBOR and use it as the request body.
    pub fn cbor<S: Serialize>(mut self, value: &S) -> Result<Self, serde_cbor::Error> {
        self.set_cbor_body(value)?;
        Ok(self)
    }

    /// Serialize the given value as CBOR and use it as the request body.
    pub fn set_cbor_body<S: Serialize>(
        &mut self,
        value: &S,
    ) -> Result<&mut Self, serde_cbor::Error> {
        self.set_header("Content-Type", "application/cbor")
            .set_body(serde_cbor::to_vec(value)?);
        Ok(self)
    }

    /// Enables or disables following redirects (equivalent to CURLOPT_FOLLOWLOCATION or -L flag).
    pub fn set_follow_redirects(&mut self, follow_redirects: bool) -> &mut Self {
        self.follow_redirects = follow_redirects;
        self
    }

    /// Set a request header.
    pub fn header(mut self, name: impl ToString, value: impl ToString) -> Self {
        self.set_header(name, value);
        self
    }

    /// Set a request header.
    pub fn set_header(&mut self, name: impl ToString, value: impl ToString) -> &mut Self {
        self.headers
            .insert(name.to_string().to_lowercase(), value.to_string());
        self
    }

    pub(crate) fn set_auth_resolver(&mut self, auth_resolver: AuthResolver) -> &mut Self {
        self.auth_resolver = Some(auth_resolver);
        self
    }

    pub fn get_header_mut<'a>(&'a mut self, name: impl ToString) -> Option<&'a mut String> {
        self.headers.get_mut(&name.to_string().to_lowercase())
    }

    /// Specify a client certificate for TLS mutual authentication.
    ///
    /// This should be a path to a base64-encoded PEM file containing the
    /// client's X.509 certificate. When using a client certificate, the client
    /// must also provide the corresponding private key; this can either be
    /// concatenated to the certificate in the PEM file (in which case it will
    /// be used automatically), or specified separately via the `key` method.
    pub fn cert(mut self, cert: impl AsRef<Path>) -> Self {
        self.set_cert(cert);
        self
    }

    /// Specify a client certificate for TLS mutual authentication.
    ///
    /// This should be a path to a base64-encoded PEM file containing the
    /// client's X.509 certificate. When using a client certificate, the client
    /// must also provide the corresponding private key; this can either be
    /// concatenated to the certificate in the PEM file (in which case it will
    /// be used automatically), or specified separately via the `key` method.
    pub fn set_cert(&mut self, cert: impl AsRef<Path>) -> &mut Self {
        self.cert = Some(cert.as_ref().into());
        self
    }

    /// Specify a client private key for TLS mutual authentication.
    ///
    /// This method can be used to specify the path to the client's private
    /// key if this key was not included in the certificate file specified via
    /// the `cert` method.
    pub fn key(mut self, key: impl AsRef<Path>) -> Self {
        self.set_key(key);
        self
    }

    /// Specify a client private key for TLS mutual authentication.
    ///
    /// This method can be used to specify the path to the client's private
    /// key if this key was not included in the certificate file specified via
    /// the `cert` method.
    pub fn set_key(&mut self, key: impl AsRef<Path>) -> &mut Self {
        self.key = Some(key.as_ref().into());
        self
    }

    /// Specify a CA certificate bundle to be used to verify the
    /// server's certificate. If not specified, the client will
    /// use the system default CA certificate bundle.
    pub fn cainfo(mut self, cainfo: impl AsRef<Path>) -> Self {
        self.set_cainfo(cainfo);
        self
    }

    /// Specify a CA certificate bundle to be used to verify the
    /// server's certificate. If not specified, the client will
    /// use the system default CA certificate bundle.
    pub fn set_cainfo(&mut self, cainfo: impl AsRef<Path>) -> &mut Self {
        self.cainfo = Some(cainfo.as_ref().into());
        self
    }

    /// Set the maximum time this request is allowed to take, including opening a
    /// connection if needed (e.g. including DNS resolution).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.set_timeout(timeout);
        self
    }

    /// Set the maximum time this request is allowed to take, including opening a
    /// connection if needed (e.g. including DNS resolution).
    pub fn set_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.overall_timeout = Some(timeout);
        self
    }

    /// Set the maximum time to spend opening a connection, if required.
    /// This includes DNS resolution and TCP/TLS initiation.
    pub fn set_connect_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// Configure whether the client should verify that the server's hostname
    /// matches either the common name (CN) or a subject alternate name (SAN)
    /// present in the server's TLS certificate. Disabling this option will make
    /// the connection insecure. This is primarily useful for testing.
    pub fn verify_tls_host(mut self, verify: bool) -> Self {
        self.set_verify_tls_host(verify);
        self
    }

    /// Configure whether the client should verify that the server's hostname
    /// matches either the common name (CN) or a subject alternate name (SAN)
    /// present in the server's TLS certificate. Disabling this option will make
    /// the connection insecure. This is primarily useful for testing.
    pub fn set_verify_tls_host(&mut self, verify: bool) -> &mut Self {
        self.verify_tls_host = verify;
        self
    }

    /// Configure whether the client should verify the authenticity of the
    /// server's TLS certificate using the CA certificate bundle specified
    /// via `cainfo` (or the default CA bundle if not set). This option is
    /// enabled by default; disabling it will make the connection insecure.
    /// This is primarily useful for testing.
    pub fn verify_tls_cert(mut self, verify: bool) -> Self {
        self.set_verify_tls_cert(verify);
        self
    }

    /// Configure whether the client should verify the authenticity of the
    /// server's TLS certificate using the CA certificate bundle specified
    /// via `cainfo` (or the default CA bundle if not set). This option is
    /// enabled by default; disabling it will make the connection insecure.
    /// This is primarily useful for testing.
    pub fn set_verify_tls_cert(&mut self, verify: bool) -> &mut Self {
        self.verify_tls_cert = verify;
        self
    }

    /// Turn on libcurl's verbose output. This will cause libcurl to print lots
    /// of verbose debug messages to stderr. This can be useful when trying to
    /// understand exactly what libcurl is doing under the hood, which can help
    /// to debug low-level protocol issues.
    pub fn verbose(mut self, verbose: bool) -> Self {
        self.set_verbose(verbose);
        self
    }

    /// Request a read buffer of the specified size, or the default value if None.
    /// Corresponds to CURLOPT_BUFFERSIZE.
    pub fn set_read_buffer_size(&mut self, size: Option<u64>) -> &mut Self {
        self.read_buffer_size = size;
        self
    }

    /// Request a write buffer of the specified size, or the default value if None.
    /// Corresponds to CURLOPT_UPLOAD_BUFFERSIZE.
    pub fn set_write_buffer_size(&mut self, size: Option<u64>) -> &mut Self {
        self.write_buffer_size = size;
        self
    }

    pub fn set_client_info(&mut self, client_info: &Option<String>) -> &mut Self {
        if let Some(info) = client_info {
            self.set_header(CLIENT_INFO_HEADER, info);
        }
        self
    }

    /// Set the fetch cause for this request. This is used for traffic attribution.
    pub fn set_fetch_cause(&mut self, fetch_cause: Option<impl ToString>) -> &mut Self {
        if let Some(c) = fetch_cause {
            self.set_header(FETCH_CAUSE_HEADER, c);
        }
        self
    }

    pub fn set_fetch_from_cas_attempted(
        &mut self,
        fetch_from_cas_attempted: Option<bool>,
    ) -> &mut Self {
        if let Some(true) = fetch_from_cas_attempted {
            self.set_header(FETCH_FROM_CAS_ATTEMPTED_HEADER, 1);
        }
        self
    }

    /// Turn on libcurl's verbose output. This will cause libcurl to print lots
    /// of verbose debug messages to stderr. This can be useful when trying to
    /// understand exactly what libcurl is doing under the hood, which can help
    /// to debug low-level protocol issues.
    pub fn set_verbose(&mut self, verbose: bool) -> &mut Self {
        self.verbose = verbose;
        self
    }

    pub fn set_auth_proxy_socket_path(
        &mut self,
        auth_proxy_socket_path: Option<String>,
    ) -> &mut Self {
        self.ctx.unix_socket_path = auth_proxy_socket_path;
        self
    }

    /// Convert the client's X.509 certificate from a PEM file into an in-memory
    /// PKCS#12 archive before passing it to libcurl. This is necessary on some
    /// platforms (most notably Windows) where the system crypto APIs (SChannel
    /// in the case of Windows) do not support loading PEM files.
    pub fn convert_cert(mut self, convert: bool) -> Self {
        self.set_convert_cert(convert);
        self
    }

    /// Convert the client's X.509 certificate from a PEM file into an in-memory
    /// PKCS#12 archive before passing it to libcurl. This is necessary on some
    /// platforms (most notably Windows) where the system crypto APIs (SChannel
    /// in the case of Windows) do not support loading PEM files.
    pub fn set_convert_cert(&mut self, convert: bool) -> &mut Self {
        self.convert_cert = convert;
        self
    }

    pub fn set_http_proxy_host(&mut self, http_proxy_host: Option<String>) -> &mut Self {
        self.http_proxy_host = http_proxy_host;
        self
    }

    pub fn set_http_no_proxy(&mut self, http_no_proxy: Option<String>) -> &mut Self {
        self.http_no_proxy = http_no_proxy;
        self
    }

    /// Execute the request, blocking until completion.
    ///
    /// This method is intended as a simple way to perform
    /// one-off HTTP requests. `HttpClient::send` should be
    /// used instead of this method when working with many
    /// concurrent requests or large requests that require
    /// progress reporting.
    pub fn send(self) -> Result<Response, HttpClientError> {
        crate::check_not_shutting_down()?;
        let mut easy: Easy2H = self.try_into()?;
        let res = easy.perform();
        let ctx = easy.get_mut().request_context_mut();
        let info = ctx.info().clone();

        match res {
            Ok(()) => {
                ctx.event_listeners().trigger_success(&info);
            }
            Err(e) => {
                ctx.event_listeners().trigger_failure(&info);
                return Err(maybe_add_os_error(&easy, e).into());
            }
        }

        Response::try_from(easy.get_mut())
    }

    /// Turn this `Request` into a streaming request. The
    /// received data for this request will be passed as
    /// it arrives to the given `Receiver`.
    pub fn into_streaming(self, receiver: Box<dyn Receiver>) -> StreamRequest {
        StreamRequest {
            request: self,
            receiver,
        }
    }

    /// Turn this `Request` into a `curl::Easy2` handle using the given
    /// `Handler` to process the response.
    pub(crate) fn into_handle(
        mut self,
        create_handler: impl FnOnce(RequestContext) -> Box<dyn HandlerExt>,
    ) -> Result<Easy2H, HttpClientError> {
        // Allow request creation listeners to configure the Request before we
        // use it, potentially overriding settings explicitly configured via
        // the methods on Request.
        REQUEST_CREATION_LISTENERS
            .read()
            .trigger_new_request(&mut self);

        let body_size = self.ctx.body.as_ref().map(|body| body.len() as u64);
        let mut url = self.ctx.url().clone();
        if self.ctx.unix_socket_path.is_some() {
            url.set_scheme("http")
                .expect("Failed setting url scheme to http");
            self.set_verify_tls_cert(false)
                .set_verify_tls_host(false)
                .set_convert_cert(false);

            if let Some(user_agent) = self.get_header_mut("user-agent") {
                user_agent.push_str("+x2pagentd");
            }
        }

        let uds = self.ctx.unix_socket_path.clone();

        let handler = create_handler(self.ctx);

        crate::check_not_shutting_down()?;
        crate::init_openssl();
        let mut easy = Easy2H::new(handler);

        easy.url(url.as_str())?;
        easy.verbose(self.verbose)?;
        easy.unix_socket_path(uds)?;
        if self.follow_redirects {
            easy.follow_location(true)?;
            easy.post_redirections(PostRedirections::new().redirect_all(true))?;
        }

        if let Some(proxy) = self.http_proxy_host {
            easy.proxy(&proxy)?;
        } else if let Ok(proxy) = std::env::var("http_proxy") {
            easy.proxy(&proxy)?;
        } else if let Ok(proxy) = std::env::var("HTTP_PROXY") {
            easy.proxy(&proxy)?;
        }

        if let Some(no_proxy) = self.http_no_proxy {
            easy.noproxy(&no_proxy)?;
        } else if let Ok(no_proxy) = std::env::var("no_proxy") {
            easy.noproxy(&no_proxy)?;
        } else if let Ok(no_proxy) = std::env::var("NO_PROXY") {
            easy.noproxy(&no_proxy)?;
        }

        // Configure the handle for the desired HTTP method.
        match easy.get_ref().request_context().method() {
            Method::Get => {}
            Method::Head => {
                easy.nobody(true)?;
            }
            Method::Post => {
                easy.post(true)?;
                if let Some(size) = body_size {
                    easy.post_field_size(size)?;
                }
            }
            Method::Put => {
                easy.upload(true)?;
                if let Some(size) = body_size {
                    easy.in_filesize(size)?;
                }
            }
        }

        if !self.accept_encoding.is_empty() {
            // To maintain compatibility with libcurl, if the Accept-Encoding is explicitly set to
            // the empty string, advertise all formats the client supports.
            if self.accept_encoding.len() == 1
                && self.accept_encoding[0] == Encoding::Other("".into())
            {
                self.accept_encoding = Encoding::all()
            }

            let encoding = self
                .accept_encoding
                .iter()
                .map(|s| s.as_ref())
                .collect::<Vec<_>>()
                .join(", ");

            // XXX: Ideally, we should set the Accept-Encoding via the accept_encoding() method
            // (which corresponds to CURLOPT_ACCEPT_ENCODING). This will cause libcurl to decode
            // the response body automatically if the received Content-Encoding matches one of the
            // requested formats.
            //
            // Unfortunately, although libcurl can be built to support many compression formats,
            // the Rust bindings configure it so that only a few formats (e.g., gzip and deflate)
            // are supported. To work around this, right now we just set the Accept-Encoding header
            // as a regular header (without setting CURLOPT_ACCEPT_ENCODING) and decode the response
            // manually. This allows us to ensure support for formats we care about (e.g., zstd).
            self.headers
                .insert(header::ACCEPT_ENCODING.as_str().into(), encoding);
        }

        // Add headers.
        let mut headers = List::new();
        for (name, value) in self.headers.iter() {
            let header = format!("{name}: {value}");
            headers.append(&header)?;
        }
        easy.http_headers(headers)?;

        // Configure TLS verification.
        easy.ssl_verify_host(self.verify_tls_host)?;
        easy.ssl_verify_peer(self.verify_tls_cert)?;

        let resolved_auth = self
            .auth_resolver
            .as_ref()
            .map(AuthResolver::resolve)
            .transpose()?;
        let cert = self
            .cert
            .as_ref()
            .or_else(|| resolved_auth.and_then(|auth| auth.cert_path.as_ref()));
        let key = self
            .key
            .as_ref()
            .or_else(|| resolved_auth.and_then(|auth| auth.key_path.as_ref()));

        match cert {
            Some(cert) if self.convert_cert => {
                // Convert certificate to PKCS#12 format for platforms that do
                // not support loading PEM files (notably Windows).
                tracing::debug!("Converting certificate {:?} to PKCS#12 format", cert);
                let blob = pem_to_pkcs12(cert, key)?;
                easy.ssl_cert_type("P12")?;
                easy.ssl_cert_blob(&blob)?;
            }
            Some(cert) => {
                easy.ssl_cert(cert)?;
                if let Some(key) = key {
                    easy.ssl_key(key)?;
                }
            }
            None => {}
        }

        // Added in curl 7.54.0, but not exposed by curl_sys for some reason.
        const CURLOPT_SUPPRESS_CONNECT_HEADERS: CURLoption = CURLOPTTYPE_LONG + 265;

        unsafe {
            // Tell curl to suppress CONNECT response from proxy. The proxy response is
            // difficult to handle correctly since you receive two sets of status
            // codes/headers, and have to "know" it is coming. See
            // https://curl.se/libcurl/c/CURLOPT_SUPPRESS_CONNECT_HEADERS.html.
            let mut rc = curl_sys::curl_easy_setopt(easy.raw(), CURLOPT_SUPPRESS_CONNECT_HEADERS);

            if rc == curl_sys::CURLE_OK && cfg!(windows) {
                rc = curl_sys::curl_easy_setopt(
                    easy.raw(),
                    curl_sys::CURLOPT_SSL_OPTIONS,
                    // Windows enables ssl revocation checking by default, which doesn't work inside the
                    // datacenter.
                    curl_sys::CURLSSLOPT_NO_REVOKE |
                    // When using openssl, this imports CAs from Windows cert store.
                    curl_sys::CURLSSLOPT_NATIVE_CA,
                );
            }

            if rc == curl_sys::CURLE_OK {
                Ok(())
            } else {
                let mut err = curl::Error::new(rc);
                if let Some(msg) = easy.take_error_buf() {
                    err.set_extra(msg);
                }
                Err(err)
            }
        }?;

        let cainfo = self
            .cainfo
            .as_ref()
            .or_else(|| resolved_auth.and_then(|auth| auth.ca_path.as_ref()));
        if let Some(cainfo) = cainfo {
            easy.cainfo(cainfo)?;
        }

        if let Some(timeout) = self.overall_timeout {
            easy.timeout(timeout)?;
        }

        if let Some(timeout) = self.connect_timeout {
            easy.connect_timeout(timeout)?;
        }

        easy.http_version(self.http_version)?;

        if let Some(mts) = self.min_transfer_speed {
            easy.low_speed_limit(mts.min_bytes_per_second)?;
            easy.low_speed_time(mts.window)?;
        }

        if let Some(read_buffer_size) = self.read_buffer_size {
            easy.buffer_size(read_buffer_size as usize)?;
        }

        if let Some(write_buffer_size) = self.write_buffer_size {
            easy.upload_buffer_size(write_buffer_size as usize)?;
        }

        // Tell libcurl to report progress to the handler.
        easy.progress(true)?;

        Ok(easy)
    }

    /// Register a callback function that is called on new requests.
    pub fn on_new_request(f: impl Fn(&mut Self) + Send + Sync + 'static) {
        REQUEST_CREATION_LISTENERS.write().on_new_request(f);
    }
}

impl TryFrom<Request> for Easy2H {
    type Error = HttpClientError;

    fn try_from(req: Request) -> Result<Self, Self::Error> {
        req.into_handle(|c| Box::new(Buffered::new(c)))
    }
}

pub struct StreamRequest {
    pub(crate) request: Request,
    pub(crate) receiver: Box<dyn Receiver>,
}

impl StreamRequest {
    pub(crate) fn into_easy(self, claim: RequestClaim) -> Result<Easy2H, HttpClientError> {
        let StreamRequest { request, receiver } = self;
        request.into_handle(|ctx| Box::new(Streaming::new(receiver, ctx, claim)))
    }
}

fn read_file(path: impl AsRef<Path>) -> Result<Vec<u8>, anyhow::Error> {
    let mut f = File::open(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

#[derive(Eq, Hash, PartialEq)]
struct PemCacheKey {
    pub cert: PathBuf,
    pub key: Option<PathBuf>,
    pub cert_mtime: SystemTime,
    pub key_mtime: Option<SystemTime>,
}

static PEM_CONVERT_CACHE: LazyLock<Mutex<LruCache<PemCacheKey, Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(LruCache::new(10)));

/// Convert a PEM-formatted X.509 certificate chain and private key into a
/// PKCS#12 archive, which can then be directly passed to libcurl using
/// `CURLOPT_SSLCERT_BLOB`. This is useful because not all TLS engines (notably
/// SChannel (WinSSL) on Windows) support loading PEM files, but all major TLS
/// engines support PKCS#12. Returns a DER-encoded binary representation of
/// the combined certificate chain and private key.
fn pem_to_pkcs12(
    cert: impl AsRef<Path>,
    key: Option<impl AsRef<Path>>,
) -> Result<Vec<u8>, anyhow::Error> {
    let mut cache = PEM_CONVERT_CACHE.lock();
    let cert_mtime = cert.as_ref().metadata()?.modified()?;
    let key_mtime = match &key {
        Some(key) => Some(key.as_ref().metadata()?.modified()?),
        None => None,
    };
    let cache_key = PemCacheKey {
        cert: cert.as_ref().to_owned(),
        key: key.as_ref().map(|k| k.as_ref().to_owned()),
        cert_mtime,
        key_mtime,
    };
    if let Some(data) = cache.get_mut(&cache_key) {
        return Ok(data.clone());
    }

    // It's common for the certificate and private key to be concatenated
    // together in the same PEM file. If a key path isn't specified, assume
    // this is the case and use the certificate PEM for the key as well.
    let cert_bytes = read_file(cert)?;
    let key_bytes = match key {
        Some(key) => Cow::Owned(read_file(key)?),
        None => Cow::Borrowed(&cert_bytes),
    };

    let cert = X509::from_pem(&cert_bytes)?;
    let key = PKey::private_key_from_pem(&key_bytes)?;

    // PKCS#12 archives are encrypted, so we need to specify a password when
    // creating one. Here we just use an empty password since it seems like most
    // TLS engines will attempt to decrypt using the empty string if no password
    // is specified.
    let pkcs12 = Pkcs12::builder()
        .name("")
        .pkey(&key)
        .cert(&cert)
        .build2("")?;

    let result = pkcs12.to_der()?;
    cache.insert(cache_key, result.clone());

    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::Ordering::Acquire;
    use std::sync::atomic::Ordering::Relaxed;

    use anyhow::Result;
    use futures::TryStreamExt;
    use http::StatusCode;
    use http::header;
    use http::header::HeaderName;
    use http::header::HeaderValue;
    use mockito::Matcher;
    use serde_json::json;

    use super::*;
    use crate::Config;
    use crate::HttpClient;
    use crate::ResolvedAuth;

    #[test]
    fn test_get() -> Result<()> {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/test")
            .with_status(200)
            .match_header("X-Api-Key", "1234")
            .with_header("Content-Type", "text/plain")
            .with_header("X-Served-By", "mock")
            .with_body("Hello, world!")
            .create();

        let client = HttpClient::new();

        let url = Url::parse(&server.url())?.join("test")?;
        let res = client.get(url).header("X-Api-Key", "1234").send()?;

        mock.assert();

        assert_eq!(res.head.status, StatusCode::OK);
        assert_eq!(&*res.body, &b"Hello, world!"[..]);
        assert_eq!(
            res.head.headers.get(header::CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("text/plain")
        );
        assert_eq!(
            res.head
                .headers
                .get(HeaderName::from_bytes(b"X-Served-By")?)
                .unwrap(),
            HeaderValue::from_static("mock")
        );

        Ok(())
    }

    #[test]
    fn test_auth_resolver_failure_is_retried_by_later_request() -> Result<()> {
        let mut server = mockito::Server::new();
        let mock = server.mock("GET", "/test").with_status(200).create();
        let calls = Arc::new(AtomicUsize::new(0));
        let auth_resolver = AuthResolver::new({
            let calls = calls.clone();
            move || {
                if calls.fetch_add(1, Relaxed) == 0 {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "certificate is not available yet",
                    ))
                } else {
                    Ok(ResolvedAuth::default())
                }
            }
        });
        let client = HttpClient::from_config(Config {
            auth_resolver: Some(auth_resolver),
            ..Default::default()
        });
        let url = Url::parse(&server.url())?.join("test")?;

        assert!(client.get(url.clone()).send().is_err());
        assert_eq!(client.get(url).send()?.head.status, StatusCode::OK);

        mock.assert();
        assert_eq!(calls.load(Relaxed), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_send_async_reports_auth_resolver_failure() -> Result<()> {
        let client = HttpClient::from_config(Config {
            auth_resolver: Some(AuthResolver::new(|| {
                Err::<ResolvedAuth, _>(io::Error::new(
                    io::ErrorKind::NotFound,
                    "certificate is missing",
                ))
            })),
            ..Default::default()
        });
        let url = Url::parse("https://example.com/test")?;

        let (responses, _stats) = client.send_async(vec![client.get(url)])?;
        let err = match responses.into_iter().next().unwrap().await {
            Ok(_) => panic!("expected the request to fail"),
            Err(err) => err,
        };

        // FIXME: The auth error is lost and the caller sees a dropped request.
        // `send_async` should return `Err` with "certificate is missing" before
        // dispatching the request.
        assert!(matches!(err, HttpClientError::RequestDropped(_)));
        Ok(())
    }

    #[tokio::test]
    async fn test_async_get() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/test")
            .with_status(200)
            .match_header("X-Api-Key", "1234")
            .with_header("Content-Type", "text/plain")
            .with_header("X-Served-By", "mock")
            .with_body("Hello, world!")
            .create();

        let client = HttpClient::new();

        let url = Url::parse(&server.url())?.join("test")?;
        let req = client.get(url).header("X-Api-Key", "1234");
        let response = client.send_async_single(req)?;
        let res = response.await?;

        mock.assert();

        assert_eq!(res.head.status, StatusCode::OK);
        assert_eq!(
            res.head.headers.get(header::CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("text/plain")
        );
        assert_eq!(
            res.head
                .headers
                .get(HeaderName::from_bytes(b"X-Served-By")?)
                .unwrap(),
            HeaderValue::from_static("mock")
        );

        let body = res.into_body().raw().try_concat().await?;
        assert_eq!(&*body, &b"Hello, world!"[..]);

        Ok(())
    }

    #[test]
    fn test_head() -> Result<()> {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("HEAD", "/test")
            .with_status(200)
            .match_header("X-Api-Key", "1234")
            .with_header("Content-Type", "text/plain")
            .with_header("X-Served-By", "mock")
            .create();

        let client = HttpClient::new();

        let url = Url::parse(&server.url())?.join("test")?;
        let res = client.head(url).header("X-Api-Key", "1234").send()?;

        mock.assert();

        assert_eq!(res.head.status, StatusCode::OK);
        assert!(res.body.is_empty());
        assert_eq!(
            res.head.headers.get(header::CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("text/plain")
        );
        assert_eq!(
            res.head
                .headers
                .get(HeaderName::from_bytes(b"X-Served-By")?)
                .unwrap(),
            HeaderValue::from_static("mock")
        );

        Ok(())
    }

    #[test]
    fn test_post() -> Result<()> {
        let body = "foo=hello&bar=world";

        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/test")
            .with_status(201)
            .match_header("Content-Type", "application/x-www-form-urlencoded")
            .match_body(Matcher::Exact(body.into()))
            .create();

        let client = HttpClient::new();

        let url = Url::parse(&server.url())?.join("test")?;
        let res = client.post(url).body(body.as_bytes()).send()?;

        mock.assert();
        assert_eq!(res.head.status, StatusCode::CREATED);

        Ok(())
    }

    #[test]
    fn test_post_large() -> Result<()> {
        let body_bytes = vec![65; 1024 * 1024];
        let body = String::from_utf8_lossy(body_bytes.as_ref());

        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/test")
            .with_status(201)
            .match_header("Expect", Matcher::Missing)
            .match_body(Matcher::Exact(body.into()))
            .create();

        let client = HttpClient::new();

        let url = Url::parse(&server.url())?.join("test")?;
        let res = client.post(url).body(body_bytes).send()?;

        mock.assert();
        assert_eq!(res.head.status, StatusCode::CREATED);

        Ok(())
    }

    #[test]
    fn test_put() -> Result<()> {
        let body = "Hello, world!";

        let mut server = mockito::Server::new();
        let mock = server
            .mock("PUT", "/test")
            .with_status(201)
            .match_header("Content-Type", "text/plain")
            .match_body(Matcher::Exact(body.into()))
            .create();

        let client = HttpClient::new();

        let url = Url::parse(&server.url())?.join("test")?;
        let res = client
            .put(url)
            .header("Content-Type", "text/plain")
            .body(body.as_bytes())
            .send()?;

        mock.assert();
        assert_eq!(res.head.status, StatusCode::CREATED);

        Ok(())
    }

    #[test]
    fn test_json() -> Result<()> {
        let body = json!({
            "foo": "bar",
            "hello": "world"
        });

        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/test")
            .with_status(201)
            .match_header("Content-Type", "application/json")
            .match_body(Matcher::Json(body.clone()))
            .create();

        let client = HttpClient::new();

        let url = Url::parse(&server.url())?.join("test")?;
        let res = client.post(url).json(&body)?.send()?;

        mock.assert();
        assert_eq!(res.head.status, StatusCode::CREATED);

        Ok(())
    }

    #[test]
    fn test_cbor() -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            foo: &'a str,
            hello: &'a str,
        }

        let body = Body {
            foo: "bar",
            hello: "world",
        };

        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/test")
            .with_status(201)
            .match_header("Content-Type", "application/cbor")
            .match_body(serde_cbor::to_vec(&body)?)
            .create();

        let client = HttpClient::new();

        let url = Url::parse(&server.url())?.join("test")?;
        let res = client.post(url).cbor(&body)?.send()?;

        mock.assert();
        assert_eq!(res.head.status, StatusCode::CREATED);

        Ok(())
    }

    #[test]
    fn test_accept_encoding() -> Result<()> {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/test")
            .with_status(200)
            .match_header("Accept-Encoding", "zstd, gzip, foobar")
            .create();

        let encodings = vec![
            Encoding::Zstd,
            Encoding::Gzip,
            Encoding::Other("foobar".into()),
        ];

        let client = HttpClient::new();

        let url = Url::parse(&server.url())?.join("test")?;
        let _ = client.get(url).accept_encoding(encodings).send()?;

        mock.assert();
        Ok(())
    }

    const DUMMY_URL_STR: &str = "https://a.example.com/b";
    const DUMMY_METHOD: Method = Method::Get;

    impl RequestContext {
        /// Dummy RequestContext for testing.
        pub(crate) fn dummy() -> Self {
            Self::new(Url::parse(DUMMY_URL_STR).unwrap(), DUMMY_METHOD)
        }
    }

    #[test]
    fn test_request_context() {
        let req = RequestContext::dummy();
        assert_eq!(req.url().as_str(), DUMMY_URL_STR);
        assert_eq!(req.method(), &DUMMY_METHOD);

        let req2 = RequestContext::dummy();
        assert_ne!(req.id(), req2.id());
    }

    #[test]
    fn test_request_callback() -> Result<()> {
        let called = Arc::new(AtomicUsize::new(0));
        Request::on_new_request({
            let called = called.clone();
            move |req| {
                // The callback can receive requests in other tests.
                // So we need to check the request is sent by this test.
                if req.ctx().url().path() == "/test_callback" {
                    called.fetch_add(1, AcqRel);
                }
            }
        });

        let mut server = mockito::Server::new();
        let mock = server
            .mock("HEAD", "/test_callback")
            .with_status(200)
            .create();

        let client = HttpClient::new();

        let url = Url::parse(&server.url())?.join("test_callback")?;
        let _res = client.head(url).send()?;

        mock.assert();
        assert_eq!(called.load(Acquire), 1);

        Ok(())
    }

    #[test]
    fn test_convert_cert_flag() -> Result<()> {
        let client = HttpClient::new();
        let url: Url = "https://example.com".parse()?;

        // Make sure convert_cert defaults to cfg!(windows) and gets
        // passed along to request.
        assert_eq!(
            curl::Version::get().ssl_version() == Some("Schannel"),
            client.get(url.clone()).convert_cert
        );

        let client = HttpClient::from_config(Config {
            convert_cert: true,
            ..Default::default()
        });
        assert!(client.get(url).convert_cert);

        Ok(())
    }
}
