/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt::Arguments;
use std::fmt::Write;
use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Error;
use bytes::Bytes;
use cached_config::ConfigHandle;
use clientinfo::CLIENT_INFO_HEADER;
use clientinfo::ClientEntryPoint;
use clientinfo::ClientInfo;
use context::CoreContext;
use futures::future;
use futures::stream::Stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use gotham::handler::IntoBody as _;
use gotham::helpers::http::Body;
use gotham::state::FromState;
use gotham::state::State;
use gotham_derive::StateData;
use gotham_ext::body_ext::BodyExt as _;
use gotham_ext::middleware::scuba::ScubaMiddlewareState;
use hostname::get_hostname;
use http::Request;
use http::header;
use http::header::HeaderMap;
use http::uri::Authority;
use http::uri::Parts;
use http::uri::PathAndQuery;
use http::uri::Scheme;
use http::uri::Uri;
use http_body_util::BodyExt as _;
use hyper_openssl::client::legacy::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use lfs_protocol::RequestBatch;
use lfs_protocol::RequestObject;
use lfs_protocol::ResponseBatch;
use metaconfig_types::RepoConfigRef;
use metaconfig_types::RepoConfigSourceRef;
use mononoke_app::args::TLSArgs;
use mononoke_macros::mononoke;
use mononoke_types::ContentId;
#[cfg(fbcode_build)]
use network_util::get_device_network_speed_bits;
use openssl::ssl::SslConnector;
use openssl::ssl::SslFiletype;
use openssl::ssl::SslMethod;
use repo_authorization::AuthorizationContext;
use repo_permission_checker::RepoPermissionCheckerRef;
use repourl::encode_repo_name;
use tokio::runtime::Handle;
use tracing::debug;
use tracing::info;

use crate::LfsRepos;
use crate::Repo;
use crate::config::ServerConfig;
use crate::errors::LfsServerContextError;
use crate::errors::LfsServerError;
use crate::middleware::LfsMethod;
use crate::middleware::RequestContext;

pub type HttpsHyperClient = Client<HttpsConnector<HttpConnector>, Body>;

// The user agent string presented to upstream
const CLIENT_USER_AGENT: &str = "mononoke-lfs-server/0.1.0 git/2.15.1";

struct LfsServerContextInner {
    repositories: LfsRepos,
    client: Arc<HttpsHyperClient>,
    server: Arc<ServerUris>,
    always_wait_for_upstream: bool,
    max_upload_size: Option<u64>,
    config_handle: ConfigHandle<ServerConfig>,
    server_hostname: Arc<String>,
    bandwidth: Option<i64>,
    compression_sniff_enabled: bool,
}

#[derive(Clone, StateData)]
pub struct LfsServerContext {
    inner: Arc<Mutex<LfsServerContextInner>>,
    will_exit: Arc<AtomicBool>,
}

impl LfsServerContext {
    pub fn new(
        repositories: LfsRepos,
        server: ServerUris,
        always_wait_for_upstream: bool,
        max_upload_size: Option<u64>,
        will_exit: Arc<AtomicBool>,
        config_handle: ConfigHandle<ServerConfig>,
        tls_args: &Option<TLSArgs>,
        bandwidth: Option<i64>,
        compression_sniff_enabled: bool,
    ) -> Result<Self, Error> {
        let connector = match tls_args {
            Some(tls_args) => {
                let mut ssl_connector = SslConnector::builder(SslMethod::tls_client())?;
                ssl_connector.set_ca_file(&tls_args.tls_ca)?;
                ssl_connector.set_certificate_file(&tls_args.tls_certificate, SslFiletype::PEM)?;
                ssl_connector.set_private_key_file(&tls_args.tls_private_key, SslFiletype::PEM)?;
                let mut http_connector = HttpConnector::new();
                http_connector.enforce_http(false);
                HttpsConnector::with_connector(http_connector, ssl_connector)
            }
            None => HttpsConnector::new(),
        }
        .map_err(Error::from)
        .context(LfsServerError::HttpClientInitializationFailed)?;

        let client = Client::builder(TokioExecutor::new()).build(connector);
        let server_hostname =
            Arc::new(get_hostname().unwrap_or_else(|_| "UNKNOWN_HOSTNAME".to_string()));

        let inner = LfsServerContextInner {
            repositories,
            server: Arc::new(server),
            client: Arc::new(client),
            always_wait_for_upstream,
            max_upload_size,
            config_handle,
            server_hostname,
            bandwidth,
            compression_sniff_enabled,
        };

        Ok(LfsServerContext {
            inner: Arc::new(Mutex::new(inner)),
            will_exit,
        })
    }

    pub async fn request(
        &self,
        ctx: CoreContext,
        repository: String,
        host: String,
        headers: &Option<&HeaderMap>,
        method: LfsMethod,
    ) -> Result<RepositoryRequestContext, LfsServerContextError> {
        let (
            repo,
            client,
            server,
            always_wait_for_upstream,
            max_upload_size,
            config,
            server_hostname,
            bandwidth,
            compression_sniff_enabled,
        ) = {
            let inner = self.inner.lock().expect("poisoned lock");

            match inner.repositories.get(&repository) {
                Some(repo) => (
                    repo,
                    inner.client.clone(),
                    inner.server.clone(),
                    inner.always_wait_for_upstream,
                    inner.max_upload_size,
                    inner.config_handle.get(),
                    inner.server_hostname.clone(),
                    inner.bandwidth,
                    inner.compression_sniff_enabled,
                ),
                None => {
                    return Err(LfsServerContextError::RepositoryDoesNotExist(repository));
                }
            }
        };

        let enforce_acl_check =
            repo.repo_config().enforce_lfs_acl_check && config.enforce_acl_check();

        acl_check(&ctx, &repo, enforce_acl_check, method).await?;

        let force_http = should_force_http_scheme(&host, headers, &server);

        Ok(RepositoryRequestContext {
            ctx,
            repo,
            uri_builder: UriBuilder {
                repository,
                server,
                host,
                server_hostname,
                force_http,
            },
            client: HttpClient::Enabled(client),
            config,
            always_wait_for_upstream,
            max_upload_size,
            bandwidth,
            compression_sniff_enabled,
        })
    }

    pub fn get_config_handle(&self) -> ConfigHandle<ServerConfig> {
        self.inner
            .lock()
            .expect("poisoned lock")
            .config_handle
            .clone()
    }

    pub fn get_config(&self) -> Arc<ServerConfig> {
        let inner = self.inner.lock().expect("poisoned lock");
        inner.config_handle.get()
    }

    pub fn will_exit(&self) -> bool {
        self.will_exit.load(Ordering::Relaxed)
    }
}

#[cfg(fbcode_build)]
pub fn get_bandwidth() -> Option<i64> {
    // We want to return None on error because the ratelimit metric fails open
    get_device_network_speed_bits("eth0").map_or_else(
        |e| {
            info!("Failed to get network speed {}", e);
            None
        },
        Some,
    )
}

#[cfg(not(fbcode_build))]
pub fn get_bandwidth() -> Option<i64> {
    info!("Could not determine network speed");
    None
}

async fn acl_check(
    ctx: &CoreContext,
    repo: &impl RepoPermissionCheckerRef,
    enforce_authorization: bool,
    method: LfsMethod,
) -> Result<(), LfsServerContextError> {
    let authz = AuthorizationContext::new(ctx);
    let acl_check = if method.is_read_only() {
        authz.check_full_repo_read(ctx, repo).await
    } else {
        authz.check_full_repo_draft(ctx, repo).await
    };

    if acl_check.is_denied() && enforce_authorization {
        Err(LfsServerContextError::Forbidden)
    } else {
        Ok(())
    }
}

#[derive(Clone)]
enum HttpClient {
    Enabled(Arc<HttpsHyperClient>),
    #[cfg(test)]
    Disabled,
}

#[derive(Clone)]
pub struct RepositoryRequestContext {
    pub ctx: CoreContext,
    pub repo: Arc<Repo>,
    pub uri_builder: UriBuilder,
    pub config: Arc<ServerConfig>,
    always_wait_for_upstream: bool,
    max_upload_size: Option<u64>,
    client: HttpClient,
    bandwidth: Option<i64>,
    compression_sniff_enabled: bool,
}

pub struct HttpClientResponse<S: Stream<Item = Result<Bytes, Error>> + Send + 'static> {
    headers: HeaderMap,
    body: Option<S>,
    handle: Handle,
}

impl<S: Stream<Item = Result<Bytes, Error>> + Send + 'static> HttpClientResponse<S> {
    pub async fn concat(mut self) -> Result<Bytes, Error> {
        let body = self
            .body
            .take()
            .expect("Body cannot be missing")
            .try_concat_body(&self.headers)?
            .await?;
        Ok(body)
    }

    pub async fn discard(self) -> Result<(), Error> {
        self.concat().await?;
        Ok(())
    }

    pub fn into_inner(mut self) -> S {
        self.body.take().expect("Body cannot be missing")
    }
}

impl<S: Stream<Item = Result<Bytes, Error>> + Send + 'static> Drop for HttpClientResponse<S> {
    fn drop(&mut self) {
        if let Some(body) = self.body.take() {
            let discard = body.for_each(|_| future::ready(()));
            self.handle.spawn(discard);
        }
    }
}

fn host_maybe_port_to_host(host_maybe_port: &str) -> Result<String, LfsServerContextError> {
    Ok(host_maybe_port
        .parse::<Uri>()
        .map_err(|_e| LfsServerContextError::MissingHostHeader)?
        .authority()
        .ok_or(LfsServerContextError::MissingHostHeader)?
        .host()
        .to_string())
}

fn should_force_http_scheme(host: &str, headers: &Option<&HeaderMap>, server: &ServerUris) -> bool {
    if server.force_http_hosts.iter().any(|h| h == host) {
        return true;
    }
    if let Some(headers) = headers {
        for header_name in &server.force_http_headers {
            if let Some(val) = headers.get(header_name.as_str()) {
                if val.as_bytes() == b"1" {
                    return true;
                }
            }
        }
    }
    false
}

fn get_host_header(headers: &Option<&HeaderMap>) -> Result<String, LfsServerContextError> {
    let host_maybe_port = headers
        .ok_or(LfsServerContextError::MissingHostHeader)?
        .get(http::header::HOST)
        .ok_or(LfsServerContextError::MissingHostHeader)?
        .to_str()
        .map_err(|_e| LfsServerContextError::MissingHostHeader)?;

    host_maybe_port_to_host(host_maybe_port)
}

impl RepositoryRequestContext {
    pub async fn instantiate(
        state: &mut State,
        repository: String,
        method: LfsMethod,
    ) -> Result<Self, LfsServerContextError> {
        let req_ctx = state.borrow_mut::<RequestContext>();
        req_ctx.set_request(repository.clone(), method);

        let ctx = req_ctx.ctx.clone();

        let headers = HeaderMap::try_borrow_from(state);
        let host = get_host_header(&headers)?;

        let lfs_ctx = LfsServerContext::borrow_from(state);
        let request_context = lfs_ctx
            .request(ctx, repository, host, &headers, method)
            .await?;

        // Per-repo config provenance; mutation id is always None until plumbed.
        let source = request_context.repo.repo_config_source();
        ScubaMiddlewareState::try_borrow_add_repo_config_provenance(
            state,
            source.version.as_deref(),
            source.mutation_id,
        );

        Ok(request_context)
    }

    pub fn always_wait_for_upstream(&self) -> bool {
        self.always_wait_for_upstream
    }

    pub fn max_upload_size(&self) -> Option<u64> {
        self.max_upload_size
    }

    pub fn bandwidth(&self) -> Option<i64> {
        self.bandwidth
    }

    /// Whether the deployment was started with compression sniffing enabled
    /// (the `--enable-compression-sniff` CLI flag). Per-request behavior is
    /// additionally gated by a JustKnob.
    pub fn compression_sniff_enabled(&self) -> bool {
        self.compression_sniff_enabled
    }

    pub async fn dispatch(
        &self,
        mut request: Request<Body>,
    ) -> Result<HttpClientResponse<impl Stream<Item = Result<Bytes, Error>> + use<>>, Error> {
        #[allow(clippy::infallible_destructuring_match)]
        let client = match self.client {
            HttpClient::Enabled(ref client) => client,
            #[cfg(test)]
            HttpClient::Disabled => panic!("HttpClient is disabled in test"),
        };

        request.headers_mut().insert(
            header::USER_AGENT,
            header::HeaderValue::from_static(CLIENT_USER_AGENT),
        );

        request.headers_mut().insert(
            CLIENT_INFO_HEADER,
            header::HeaderValue::from_str(
                &ClientInfo::default_with_entry_point(ClientEntryPoint::LfsServer).to_json()?,
            )?,
        );

        let res = client.request(request);

        // NOTE: We spawn the request on an executor because we'd like to read the response even if
        // we drop the future returned here. The reason for that is that if we don't read a
        // response, Hyper will not reuse the connection for its pool (which makes sense for the
        // general case: if your server is sending you 5GB of data and you drop the future, you
        // don't want to read all that later just to reuse a connection).
        let fut = async move {
            let res = res.await.context(LfsServerError::UpstreamDidNotRespond)?;

            let (head, body) = res.into_parts();

            if !head.status.is_success() {
                let body = body
                    .into_data_stream()
                    .try_concat_body(&head.headers)?
                    .await?;
                return Err(LfsServerError::UpstreamError(
                    head.status,
                    String::from_utf8_lossy(&body).to_string(),
                )
                .into());
            }

            // NOTE: This buffers the response here, since all our callsites need a concatenated
            // response. If we want to add callsites that need a streaming response, we should add
            // our own wrapper type that wraps the response and the headers.
            Ok(HttpClientResponse {
                headers: head.headers,
                body: Some(body.into_data_stream().map_err(Error::from)),
                handle: Handle::current(),
            })
        };

        mononoke::spawn_task(fut).await?
    }

    pub async fn upstream_batch(
        &self,
        batch: &RequestBatch,
    ) -> Result<Option<ResponseBatch>, LfsServerError> {
        // If upstream is disabled, we won't send an upstream request
        if !self.repo.repo_config().lfs.use_upstream_lfs_server {
            debug!("No Upstream LFS server configured for repo");
            return Ok(None);
        }
        // If we don't have an upstream, we can't send an upstream request
        let Some(uri) = self.uri_builder.upstream_batch_uri()? else {
            debug!("No Upstream LFS server url configured");
            return Ok(None);
        };

        let body: Bytes = serde_json::to_vec(&batch)
            .map_err(|e| LfsServerError::SerializationFailed(e.into()))?
            .into();

        let req = Request::post(uri)
            .body(body.into_body())
            .map_err(|e| LfsServerError::Error(e.into()))?;

        let res = self
            .dispatch(req)
            .await
            .map_err(LfsServerError::UpstreamBatchNoResponse)?
            .concat()
            .await
            .map_err(LfsServerError::UpstreamBatchNoResponse)?;

        let batch = serde_json::from_slice::<ResponseBatch>(&res)
            .map_err(|e| LfsServerError::UpstreamBatchInvalid(e.into()))?;

        Ok(Some(batch))
    }
}

#[derive(Clone)]
pub struct UriBuilder {
    pub repository: String,
    pub server: Arc<ServerUris>,
    pub host: String,
    pub server_hostname: Arc<String>,
    pub force_http: bool,
}

impl UriBuilder {
    fn pick_uri(&self) -> Result<&BaseUri, LfsServerError> {
        self.server
            .self_uris
            .iter()
            .find(|&x| x.authority.host() == self.host)
            .ok_or_else(|| LfsServerError::HostNotAllowlisted(self.host.clone()))
    }

    fn scheme_override(&self) -> Option<Scheme> {
        if self.force_http {
            Some(Scheme::HTTP)
        } else {
            None
        }
    }

    pub fn upload_uri(&self, object: &RequestObject) -> Result<Uri, LfsServerError> {
        self.pick_uri()?
            .build(
                format_args!(
                    "{}/upload/{}/{}?server_hostname={}",
                    encode_repo_name(&self.repository),
                    object.oid,
                    object.size,
                    self.server_hostname,
                ),
                self.scheme_override(),
            )
            .map_err(|e| LfsServerError::UriBuilderFailed("upload_uri", e))
    }

    pub fn download_uri(&self, content_id: &ContentId) -> Result<Uri, LfsServerError> {
        self.pick_uri()?
            .build(
                format_args!(
                    "{}/download/{}?server_hostname={}",
                    encode_repo_name(&self.repository),
                    content_id,
                    self.server_hostname
                ),
                self.scheme_override(),
            )
            .map_err(|e| LfsServerError::UriBuilderFailed("download_uri", e))
    }

    pub fn consistent_download_uri(
        &self,
        content_id: &ContentId,
        routing_key: String,
        tasks_per_content: NonZeroU16,
    ) -> Result<Uri, LfsServerError> {
        self.pick_uri()?
            .build(
                format_args!(
                    "{}/download/{}?routing={}&server_hostname={}&tpc={}",
                    encode_repo_name(&self.repository),
                    content_id,
                    routing_key,
                    self.server_hostname,
                    tasks_per_content.get(),
                ),
                self.scheme_override(),
            )
            .map_err(|e| LfsServerError::UriBuilderFailed("consistent_download_uri", e))
    }

    pub fn upstream_batch_uri(&self) -> Result<Option<Uri>, LfsServerError> {
        self.server
            .upstream_uri
            .as_ref()
            .map(|uri| {
                uri.build(format_args!("objects/batch"), None)
                    .map_err(|e| LfsServerError::UriBuilderFailed("upstream_batch_uri", e))
            })
            .transpose()
    }
}

fn parse_and_check_uri(src: &str) -> Result<BaseUri, LfsServerError> {
    let uri = src
        .parse::<Uri>()
        .map_err(|_e| LfsServerError::InvalidUri(src.to_string(), "invalid uri"))?;

    let Parts {
        scheme,
        authority,
        path_and_query,
        ..
    } = uri.into_parts();

    Ok(BaseUri {
        scheme: scheme
            .ok_or_else(|| LfsServerError::InvalidUri(src.to_string(), "missing scheme"))?,
        authority: authority
            .ok_or_else(|| LfsServerError::InvalidUri(src.to_string(), "missing authority"))?,
        path_and_query,
    })
}

#[derive(Debug)]
pub struct ServerUris {
    /// The list of allowlisted root URLs to use when composing URLs for this LFS server
    self_uris: Vec<BaseUri>,
    /// The URL for an upstream LFS server
    upstream_uri: Option<BaseUri>,
    /// Hosts for which to force http scheme in generated hrefs
    force_http_hosts: Vec<String>,
    /// Headers whose presence (with value "1") forces http scheme in generated hrefs
    force_http_headers: Vec<String>,
}

impl ServerUris {
    pub fn new(
        self_uris: Vec<String>,
        upstream_uri: Option<String>,
        force_http_hosts: Vec<String>,
        force_http_headers: Vec<String>,
    ) -> Result<Self, Error> {
        Ok(Self {
            self_uris: self_uris
                .into_iter()
                .map(|v| parse_and_check_uri(&v))
                .collect::<Result<Vec<_>, _>>()?,
            upstream_uri: upstream_uri.map(|v| parse_and_check_uri(&v)).transpose()?,
            force_http_hosts,
            force_http_headers,
        })
    }
}

#[derive(Debug)]
struct BaseUri {
    scheme: Scheme,
    authority: Authority,
    path_and_query: Option<PathAndQuery>,
}

impl BaseUri {
    pub fn build(&self, args: Arguments, scheme_override: Option<Scheme>) -> Result<Uri, Error> {
        let mut p = String::new();
        if let Some(ref path_and_query) = self.path_and_query {
            write!(&mut p, "{path_and_query}")?;
            if !path_and_query.path().ends_with('/') {
                write!(&mut p, "/")?;
            }
        }
        p.write_fmt(args)?;

        Uri::builder()
            .scheme(scheme_override.unwrap_or_else(|| self.scheme.clone()))
            .authority(self.authority.clone())
            .path_and_query(&p[..])
            .build()
            .map_err(Error::from)
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use fbinit::FacebookInit;
    use lfs_protocol::Sha256 as LfsSha256;
    use mononoke_macros::mononoke;
    use mononoke_types::ContentId;
    use mononoke_types::hash::Sha256;
    use repo_permission_checker::AlwaysAllowRepoPermissionChecker;
    use repo_permission_checker::MockRepoPermissionChecker;
    use test_repo_factory::TestRepoFactory;

    use super::*;

    const ONES_HASH: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const TWOS_HASH: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const SIZE: u64 = 123;
    const SERVER_HOSTNAME: &str = "mzr.re";

    pub fn uri_builder(
        self_uris: Vec<&str>,
        upstream_uri: Option<&str>,
        host: String,
    ) -> Result<UriBuilder, Error> {
        let server = ServerUris::new(
            self_uris.into_iter().map(|v| v.to_string()).collect(),
            upstream_uri.map(|v| v.to_string()),
            vec![],
            vec![],
        )?;
        Ok(UriBuilder {
            repository: "repo123/blah".to_string(),
            server: Arc::new(server),
            host,
            server_hostname: Arc::new(SERVER_HOSTNAME.to_string()),
            force_http: false,
        })
    }

    pub struct TestContextBuilder<'a> {
        fb: FacebookInit,
        repo: Repo,
        self_uris: Vec<&'a str>,
        upstream_uri: Option<String>,
        config: ServerConfig,
        host: String,
    }

    impl TestContextBuilder<'_> {
        pub fn repo(mut self, repo: Repo) -> Self {
            self.repo = repo;
            self
        }

        pub fn upstream_uri(mut self, upstream_uri: Option<String>) -> Self {
            self.upstream_uri = upstream_uri;
            self
        }

        pub fn config(mut self, config: ServerConfig) -> Self {
            self.config = config;
            self
        }

        pub fn build(self) -> Result<RepositoryRequestContext, Error> {
            let Self {
                fb,
                repo,
                self_uris,
                upstream_uri,
                config,
                host,
            } = self;

            let uri_builder = uri_builder(self_uris, upstream_uri.as_deref(), host)?;

            Ok(RepositoryRequestContext {
                ctx: CoreContext::test_mock(fb),
                repo: Arc::new(repo),
                config: Arc::new(config),
                uri_builder,
                always_wait_for_upstream: false,
                max_upload_size: None,
                client: HttpClient::Disabled,
                bandwidth: None,
                compression_sniff_enabled: false,
            })
        }
    }

    impl RepositoryRequestContext {
        pub async fn test_builder(fb: FacebookInit) -> Result<TestContextBuilder<'static>, Error> {
            let repo = TestRepoFactory::new(fb)?.build().await?;

            Self::test_builder_with_repo(fb, repo)
        }

        pub fn test_builder_with_repo(
            fb: FacebookInit,
            repo: Repo,
        ) -> Result<TestContextBuilder<'static>, Error> {
            Ok(TestContextBuilder {
                fb,
                repo,
                self_uris: vec!["http://foo.com/"],
                upstream_uri: Some("http://bar.com".to_string()),
                config: ServerConfig::default(),
                host: "foo.com".to_string(),
            })
        }
    }

    fn obj() -> Result<RequestObject, Error> {
        Ok(RequestObject {
            oid: LfsSha256::from_str(ONES_HASH)?,
            size: SIZE,
        })
    }

    fn content_id() -> Result<ContentId, Error> {
        ContentId::from_str(ONES_HASH)
    }

    fn oid() -> Result<Sha256, Error> {
        Sha256::from_str(TWOS_HASH)
    }

    fn build_url(base: &str, hash: &str, size: Option<u64>) -> String {
        if let Some(size) = size {
            format!("{base}/{hash}/{size}?server_hostname={SERVER_HOSTNAME}")
        } else {
            format!("{base}/{hash}?server_hostname={SERVER_HOSTNAME}")
        }
    }

    #[mononoke::test]
    fn test_basic_upload_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upload_uri(&obj()?)?.to_string(),
            build_url(
                "http://foo.com/repo123%2Fblah/upload",
                ONES_HASH,
                Some(SIZE)
            ),
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_basic_upload_uri_slash() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upload_uri(&obj()?)?.to_string(),
            build_url(
                "http://foo.com/repo123%2Fblah/upload",
                ONES_HASH,
                Some(SIZE)
            ),
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_prefix_upload_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/bar"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upload_uri(&obj()?)?.to_string(),
            build_url(
                "http://foo.com/bar/repo123%2Fblah/upload",
                ONES_HASH,
                Some(SIZE)
            ),
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_prefix_slash_upload_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/bar/"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upload_uri(&obj()?)?.to_string(),
            //format!("http://foo.com/bar/repo123/upload/{}/{}", ONES_HASH, SIZE),
            build_url(
                "http://foo.com/bar/repo123%2Fblah/upload",
                ONES_HASH,
                Some(SIZE)
            ),
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_basic_download_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.download_uri(&content_id()?)?.to_string(),
            build_url("http://foo.com/repo123%2Fblah/download", ONES_HASH, None),
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_basic_download_uri_slash() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.download_uri(&content_id()?)?.to_string(),
            build_url("http://foo.com/repo123%2Fblah/download", ONES_HASH, None),
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_prefix_download_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/bar"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.download_uri(&content_id()?)?.to_string(),
            build_url(
                "http://foo.com/bar/repo123%2Fblah/download",
                ONES_HASH,
                None
            ),
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_prefix_slash_download_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/bar/"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.download_uri(&content_id()?)?.to_string(),
            build_url(
                "http://foo.com/bar/repo123%2Fblah/download",
                ONES_HASH,
                None
            ),
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_basic_consistent_download_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.consistent_download_uri(
                &content_id()?,
                format!("{}", oid()?),
                1.try_into().unwrap()
            )?
            .to_string(),
            format!(
                "http://foo.com/repo123%2Fblah/download/{ONES_HASH}?routing={TWOS_HASH}&server_hostname={SERVER_HOSTNAME}&tpc=1"
            ),
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_basic_upstream_batch_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upstream_batch_uri()?.map(|uri| uri.to_string()),
            Some("http://bar.com/objects/batch".to_string()),
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_basic_upstream_batch_uri_slash() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upstream_batch_uri()?.map(|uri| uri.to_string()),
            Some("http://bar.com/objects/batch".to_string()),
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_prefix_upstream_batch_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com"],
            Some("http://bar.com/foo"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upstream_batch_uri()?.map(|uri| uri.to_string()),
            Some("http://bar.com/foo/objects/batch".to_string()),
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_prefix_slash_upstream_batch_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com"],
            Some("http://bar.com/foo/"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upstream_batch_uri()?.map(|uri| uri.to_string()),
            Some("http://bar.com/foo/objects/batch".to_string()),
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_acl_check_no_certificates(fb: FacebookInit) -> Result<(), Error> {
        let aclchecker = AlwaysAllowRepoPermissionChecker::new();
        let repo: Repo = test_repo_factory::TestRepoFactory::new(fb)?
            .with_permission_checker(Arc::new(aclchecker))
            .build()
            .await?;

        let ctx = CoreContext::test_mock(fb);
        // No certificates + ACL enforcement off = free pass
        acl_check(&ctx, &repo, false, LfsMethod::Download).await?;

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_acl_check_action(fb: FacebookInit) -> Result<(), Error> {
        let mut aclchecker = MockRepoPermissionChecker::new();
        aclchecker
            .expect_check_if_read_access_allowed()
            .return_const(true)
            .times(1);
        aclchecker
            .expect_check_if_draft_access_allowed_with_tunable_enforcement()
            .return_const(true)
            .times(1);

        let repo: Repo = test_repo_factory::TestRepoFactory::new(fb)?
            .with_permission_checker(Arc::new(aclchecker))
            .build()
            .await?;

        let ctx = CoreContext::test_mock(fb);
        acl_check(&ctx, &repo, false, LfsMethod::Download).await?;
        acl_check(&ctx, &repo, false, LfsMethod::Upload).await?;

        Ok(())
    }

    #[mononoke::test]
    fn test_host_maybe_port_to_host() -> Result<(), Error> {
        assert_eq!(host_maybe_port_to_host("example.com")?, "example.com");
        assert_eq!(host_maybe_port_to_host("example.com:90")?, "example.com");
        assert_eq!(host_maybe_port_to_host("[::1]")?, "[::1]");
        assert_eq!(host_maybe_port_to_host("[::1]:80")?, "[::1]");
        assert_eq!(host_maybe_port_to_host("127.0.0.1:80")?, "127.0.0.1");
        Ok(())
    }

    fn make_server_uris(force_http_hosts: Vec<&str>, force_http_headers: Vec<&str>) -> ServerUris {
        ServerUris::new(
            vec!["https://foo.com".to_string()],
            None,
            force_http_hosts
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            force_http_headers
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
        )
        .unwrap()
    }

    #[mononoke::test]
    fn test_should_force_http_matching_host() {
        let server = make_server_uris(vec!["proxy.example.com"], vec![]);
        let headers: Option<&HeaderMap> = None;
        assert!(should_force_http_scheme(
            "proxy.example.com",
            &headers,
            &server
        ));
    }

    #[mononoke::test]
    fn test_should_force_http_non_matching_host() {
        let server = make_server_uris(vec!["proxy.example.com"], vec![]);
        let headers: Option<&HeaderMap> = None;
        assert!(!should_force_http_scheme(
            "other.example.com",
            &headers,
            &server
        ));
    }

    #[mononoke::test]
    fn test_should_force_http_matching_header() {
        let server = make_server_uris(vec![], vec!["corpx2pagent"]);
        let mut hm = HeaderMap::new();
        hm.insert("corpx2pagent", "1".parse().unwrap());
        let headers = Some(&hm);
        assert!(should_force_http_scheme("any.host.com", &headers, &server));
    }

    #[mononoke::test]
    fn test_should_force_http_header_wrong_value() {
        let server = make_server_uris(vec![], vec!["corpx2pagent"]);
        let mut hm = HeaderMap::new();
        hm.insert("corpx2pagent", "0".parse().unwrap());
        let headers = Some(&hm);
        assert!(!should_force_http_scheme("any.host.com", &headers, &server));
    }

    #[mononoke::test]
    fn test_should_force_http_header_missing() {
        let server = make_server_uris(vec![], vec!["corpx2pagent"]);
        let hm = HeaderMap::new();
        let headers = Some(&hm);
        assert!(!should_force_http_scheme("any.host.com", &headers, &server));
    }

    #[mononoke::test]
    fn test_should_force_http_no_config() {
        let server = make_server_uris(vec![], vec![]);
        let headers: Option<&HeaderMap> = None;
        assert!(!should_force_http_scheme("any.host.com", &headers, &server));
    }

    #[mononoke::test]
    fn test_force_http_overrides_https_upload_uri() -> Result<(), Error> {
        let server = ServerUris::new(
            vec!["https://foo.com".to_string()],
            Some("https://bar.com".to_string()),
            vec![],
            vec![],
        )?;
        let b = UriBuilder {
            repository: "repo123/blah".to_string(),
            server: Arc::new(server),
            host: "foo.com".to_string(),
            server_hostname: Arc::new(SERVER_HOSTNAME.to_string()),
            force_http: true,
        };
        let uri = b.upload_uri(&obj()?)?.to_string();
        assert!(uri.starts_with("http://"), "expected http:// but got {uri}");
        assert!(
            !uri.starts_with("https://"),
            "expected no https:// but got {uri}"
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_force_http_overrides_https_download_uri() -> Result<(), Error> {
        let server = ServerUris::new(
            vec!["https://foo.com".to_string()],
            Some("https://bar.com".to_string()),
            vec![],
            vec![],
        )?;
        let b = UriBuilder {
            repository: "repo123/blah".to_string(),
            server: Arc::new(server),
            host: "foo.com".to_string(),
            server_hostname: Arc::new(SERVER_HOSTNAME.to_string()),
            force_http: true,
        };
        let uri = b.download_uri(&content_id()?)?.to_string();
        assert!(uri.starts_with("http://"), "expected http:// but got {uri}");
        Ok(())
    }

    #[mononoke::test]
    fn test_force_http_does_not_affect_upstream() -> Result<(), Error> {
        let server = ServerUris::new(
            vec!["https://foo.com".to_string()],
            Some("https://bar.com".to_string()),
            vec![],
            vec![],
        )?;
        let b = UriBuilder {
            repository: "repo123/blah".to_string(),
            server: Arc::new(server),
            host: "foo.com".to_string(),
            server_hostname: Arc::new(SERVER_HOSTNAME.to_string()),
            force_http: true,
        };
        let uri = b.upstream_batch_uri()?.unwrap().to_string();
        assert!(
            uri.starts_with("https://"),
            "upstream should stay https:// but got {uri}"
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_no_force_http_preserves_https() -> Result<(), Error> {
        let server = ServerUris::new(
            vec!["https://foo.com".to_string()],
            Some("https://bar.com".to_string()),
            vec![],
            vec![],
        )?;
        let b = UriBuilder {
            repository: "repo123/blah".to_string(),
            server: Arc::new(server),
            host: "foo.com".to_string(),
            server_hostname: Arc::new(SERVER_HOSTNAME.to_string()),
            force_http: false,
        };
        let uri = b.upload_uri(&obj()?)?.to_string();
        assert!(
            uri.starts_with("https://"),
            "expected https:// but got {uri}"
        );
        Ok(())
    }
}
