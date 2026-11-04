/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashSet;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use std::vec::IntoIter;

use futures::prelude::*;
use url::Url;

use crate::AuthResolver;
use crate::Easy2H;
use crate::claimer::RequestClaimer;
use crate::dispatcher::AsyncRequestDispatcher;
use crate::dispatcher::multi_worker_dispatcher;
use crate::dispatcher::spawn_blocking_dispatcher;
use crate::driver::MultiDriver;
use crate::errors::Abort;
use crate::errors::HttpClientError;
use crate::errors::maybe_add_os_error;
use crate::event_listeners::HttpClientEventListeners;
use crate::handler::HandlerExt;
use crate::pool::Pool;
use crate::receiver::ChannelReceiver;
use crate::receiver::DEFAULT_RESPONSE_BUFFER_LENGTH;
use crate::request::Method;
use crate::request::Request;
use crate::request::StreamRequest;
use crate::response::AsyncResponse;
use crate::response::Response;
use crate::stats::Stats;

pub type ResponseFuture =
    Pin<Box<dyn Future<Output = Result<AsyncResponse, HttpClientError>> + Send + 'static>>;
pub type StatsFuture =
    Pin<Box<dyn Future<Output = Result<Stats, HttpClientError>> + Send + 'static>>;

/// An async-compatible HTTP client powered by libcurl.
///
/// Essentially a more ergonomic API for working with
/// libcurl's multi interface. See URL for details:
///
/// https://curl.haxx.se/libcurl/c/libcurl-multi.html
///
/// NOTE: If you do not need to perform multiple concurrent
/// requests, you may want to use  `Request::send` instead.
#[derive(Clone)]
pub struct HttpClient {
    pool: Pool,
    event_listeners: HttpClientEventListeners,
    config: Config,
    claimer: RequestClaimer,
    dispatcher: Arc<dyn AsyncRequestDispatcher>,
}

#[derive(Clone)]
pub(crate) struct WorkerClient {
    pub(crate) pool: Pool,
    pub(crate) event_listeners: HttpClientEventListeners,
    pub(crate) config: Config,
    pub(crate) claimer: RequestClaimer,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub auth_resolver: Option<AuthResolver>,
    pub cert_path: Option<PathBuf>,
    pub key_path: Option<PathBuf>,
    pub ca_path: Option<PathBuf>,
    pub convert_cert: bool,

    pub client_info: Option<String>,
    pub disable_tls_verification: bool,
    // This sets curl's max connection limit, and (if `limit_requests==true`) causes this
    // library to limit the number of in-flight requests separately, _before_ the
    // requests are given to curl.
    pub max_concurrent_requests: Option<usize>,

    // Limit the number of concurrent requests for a "batch" of requests passed at once to
    // client.send_async(). This allows us to have a high global request limit for small
    // "random" requests, while having lower limits for heavy requests (e.g. fetch 10m
    // files across 1000 requests).
    pub max_concurrent_requests_per_batch: Option<usize>,

    // Max number of multiplexed HTTP/2 requests per-connection.
    // Currently defaults to 100 (in curl).
    pub max_concurrent_streams: Option<usize>,

    // Escape hatch to turn off our request limiting.
    pub limit_requests: bool,
    pub unix_socket_domains: HashSet<String>,
    pub unix_socket_path: Option<String>,
    pub verbose: bool,
    pub verbose_stats: bool,

    pub read_buffer_size: Option<u64>,
    pub write_buffer_size: Option<u64>,
    pub response_buffer_length: usize,
    pub follow_redirects: bool,

    pub http_proxy_host: Option<String>,
    pub http_no_proxy: Option<String>,
    pub http_worker_threads: usize,
}

impl Default for Config {
    fn default() -> Self {
        let version = curl::Version::get();

        // Example values: "SecureTransport", "OpenSSL/1.1.1t", "Schannel"
        let ssl_version = version.ssl_version();

        tracing::debug!(curl_ssl=?ssl_version);

        Self {
            auth_resolver: None,
            cert_path: None,
            key_path: None,
            ca_path: None,

            // Convert to PKCS#12 if we are using schannel, which doesn't like PEM.
            convert_cert: ssl_version.is_some_and(|v| v.starts_with("Schannel")),

            client_info: None,
            disable_tls_verification: false,
            max_concurrent_requests: None, // No limit by default
            max_concurrent_requests_per_batch: None,
            max_concurrent_streams: None,
            limit_requests: true,
            unix_socket_domains: HashSet::new(),
            unix_socket_path: None,
            verbose: false,
            verbose_stats: false,

            read_buffer_size: None,
            write_buffer_size: None,
            response_buffer_length: DEFAULT_RESPONSE_BUFFER_LENGTH,
            follow_redirects: true,

            http_proxy_host: None,
            http_no_proxy: None,
            http_worker_threads: 4,
        }
    }
}

impl HttpClient {
    pub fn new() -> Self {
        Self::from_config(Default::default())
    }

    pub fn from_config(config: Config) -> Self {
        crate::init();
        let claimer = RequestClaimer::new(config.limit_requests, config.max_concurrent_requests);
        let dispatcher = if config.http_worker_threads == 0 {
            spawn_blocking_dispatcher()
        } else {
            multi_worker_dispatcher(config.http_worker_threads)
        };

        Self {
            config,
            claimer,
            pool: Pool::new(),
            event_listeners: Default::default(),
            dispatcher,
        }
    }

    pub fn verbose_stats(mut self, verbose: bool) -> Self {
        self.config.verbose_stats = verbose;
        self
    }

    pub fn max_concurrent_requests(mut self, max: Option<usize>) -> Self {
        self.config.max_concurrent_requests = max;
        self.claimer = self.claimer.with_limit(max);
        self
    }

    /// Perform multiple HTTP requests concurrently.
    ///
    /// This function will block until all transfers have completed.
    /// Whenever a transfer completes, the user-provided closure
    /// will be called with the result.
    ///
    /// The closure returns a boolean. If false, this function will
    /// return early and all other pending transfers will be aborted.
    pub fn send<F>(
        &self,
        requests: Vec<Request>,
        mut response_cb: F,
    ) -> Result<Stats, HttpClientError>
    where
        F: FnMut(Result<Response, HttpClientError>) -> Result<(), Abort>,
    {
        crate::check_not_shutting_down()?;
        let mut multi = self.pool.multi();
        multi
            .get_mut()
            .set_max_total_connections(self.config.max_concurrent_requests.unwrap_or(0))?;

        if let Some(max_streams) = self.config.max_concurrent_streams {
            multi.get_mut().set_max_concurrent_streams(max_streams)?;
        }

        let driver = MultiDriver::new(multi.get(), self.config.verbose_stats);

        for mut request in requests {
            self.event_listeners.trigger_new_request(request.ctx_mut());
            let handle: Easy2H = request.try_into()?;
            driver.add(handle)?;
        }

        let mut tls_error = false;
        let stats = driver.perform(|res| {
            if let Err((_, e)) = &res {
                let e: HttpClientError = e.clone().into();
                if let HttpClientError::Tls(_) = e {
                    tls_error = true;
                }
            }
            let res = res
                .map_err(|(mut easy, e)| {
                    let ctx = easy.get_mut().request_context_mut();
                    let info = ctx.info().clone();
                    ctx.event_listeners().trigger_failure(&info);
                    self.event_listeners.trigger_failed_request(ctx);
                    maybe_add_os_error(&easy, e).into()
                })
                .and_then(|mut easy| {
                    let ctx = easy.get_mut().request_context_mut();
                    let info = ctx.info().clone();
                    ctx.event_listeners().trigger_success(&info);
                    self.event_listeners.trigger_succeeded_request(ctx);
                    Response::try_from(easy.get_mut())
                });
            response_cb(res)
        })?;

        self.event_listeners.trigger_stats(&stats);

        drop(driver);

        // Don't reuse the connection if we've hit auth issues. We've seen cases where we reuse
        // expired credentials.
        if tls_error {
            multi.discard();
        }

        Ok(stats)
    }

    /// Async version of `send` which runs all of the given request concurrently
    /// in another thread.
    ///
    /// Returns a `Vec` of `ResponseFuture`s corresponding to each of the given
    /// input `Request`s. (They are returned in the same order as they were
    /// passed in.) Each `ResponseFuture` will resolve once all of the headers
    /// for that request have been received. The resulting `AsyncResponse` can
    /// be used to access the headers and body stream.
    pub fn send_async<I: IntoIterator<Item = Request>>(
        &self,
        requests: I,
    ) -> Result<(Vec<ResponseFuture>, StatsFuture), HttpClientError> {
        crate::check_not_shutting_down()?;
        let client = self.worker_client();

        let mut stream_requests = Vec::new();
        let mut responses = Vec::new();

        for req in requests {
            let (request, response) = self.prepare_async_request(req);
            stream_requests.push(request);
            responses.push(response);
        }

        if !stream_requests.is_empty() {
            self.resolve_auth()?;
        }
        let stats = self.dispatcher.dispatch(client, stream_requests)?;

        Ok((responses, stats))
    }

    /// Async version of `send` for a single request.
    pub fn send_async_single(&self, request: Request) -> Result<ResponseFuture, HttpClientError> {
        crate::check_not_shutting_down()?;
        self.resolve_auth()?;
        let client = self.worker_client();
        let (request, response) = self.prepare_async_request(request);
        let stats = self.dispatcher.dispatch(client, vec![request])?;
        Ok(async move {
            match response.await {
                Ok(response) => Ok(response),
                error @ Err(HttpClientError::RequestDropped(_)) => stats.await.and(error),
                Err(error) => Err(error),
            }
        }
        .boxed())
    }

    /// Resolve auth before dispatching so auth errors reach the caller directly.
    fn resolve_auth(&self) -> Result<(), HttpClientError> {
        if let Some(auth_resolver) = &self.config.auth_resolver {
            auth_resolver.resolve()?;
        }
        Ok(())
    }

    fn prepare_async_request(&self, req: Request) -> (StreamRequest, ResponseFuture) {
        let request_info = req.ctx().info().clone();
        let (receiver, streams) = ChannelReceiver::new(self.config.response_buffer_length);
        let request = req.into_streaming(Box::new(receiver));
        let response = AsyncResponse::new(streams, request_info).boxed();
        (request, response)
    }

    fn worker_client(&self) -> WorkerClient {
        WorkerClient {
            pool: self.pool.clone(),
            event_listeners: self.event_listeners.clone(),
            config: self.config.clone(),
            claimer: self.claimer.clone(),
        }
    }

    /// Obtain the `HttpClientEventListeners` to register callbacks.
    pub fn event_listeners(&mut self) -> &mut HttpClientEventListeners {
        &mut self.event_listeners
    }

    /// Easier way to register event callbacks using the builder pattern.
    pub fn with_event_listeners(mut self, f: impl FnOnce(&mut HttpClientEventListeners)) -> Self {
        f(&mut self.event_listeners);
        self
    }

    /// Perform the given requests, but stream the responses to the
    /// `Receiver` attached to each respective request rather than
    /// buffering the content of each response.
    ///
    /// Note that this function is not asynchronous; it WILL BLOCK
    /// until all of the transfers are complete, and will return
    /// the total stats across all transfers when complete.
    pub fn stream(&self, requests: Vec<StreamRequest>) -> Result<Stats, HttpClientError> {
        self.worker_client().stream(requests)
    }

    /// Create a request with this client's config applied.
    pub fn new_request(&self, url: Url, method: Method) -> Request {
        self.configure_request(Request::new(
            url,
            method,
            self.claimer.with_limit(self.config.max_concurrent_requests),
        ))
    }

    /// Create a GET request with this client's config applied.
    pub fn get(&self, url: Url) -> Request {
        self.new_request(url, Method::Get)
    }

    /// Create a HEAD request with this client's config applied.
    pub fn head(&self, url: Url) -> Request {
        self.new_request(url, Method::Head)
    }

    /// Create a POST request with this client's config applied.
    pub fn post(&self, url: Url) -> Request {
        self.new_request(url, Method::Post)
    }

    /// Create a PUT request with this client's config applied.
    pub fn put(&self, url: Url) -> Request {
        self.new_request(url, Method::Put)
    }

    fn configure_request(&self, mut req: Request) -> Request {
        req.set_client_info(&self.config.client_info);
        req.set_convert_cert(self.config.convert_cert);
        req.set_verbose(self.config.verbose);

        if let Some(auth_resolver) = &self.config.auth_resolver {
            req.set_auth_resolver(auth_resolver.clone());
        }

        if let Some(domain) = req.ctx().url().domain() {
            if self.config.unix_socket_domains.contains(domain) {
                req.set_auth_proxy_socket_path(self.config.unix_socket_path.clone());
            }
        }

        if let Some(cert_path) = &self.config.cert_path {
            req.set_cert(cert_path);
        }

        if let Some(key_path) = &self.config.key_path {
            req.set_key(key_path);
        }

        if let Some(ca_path) = &self.config.ca_path {
            req.set_cainfo(ca_path);
        }

        req.set_verify_tls_cert(!self.config.disable_tls_verification);
        req.set_verify_tls_host(!self.config.disable_tls_verification);

        req.set_read_buffer_size(self.config.read_buffer_size);
        req.set_write_buffer_size(self.config.write_buffer_size);

        req
    }
}

impl WorkerClient {
    /// Perform the given requests, but stream the responses to the
    /// `Receiver` attached to each respective request rather than
    /// buffering the content of each response.
    ///
    /// Note that this function is not asynchronous; it WILL BLOCK
    /// until all of the transfers are complete, and will return
    /// the total stats across all transfers when complete.
    pub(crate) fn stream(&self, requests: Vec<StreamRequest>) -> Result<Stats, HttpClientError> {
        crate::check_not_shutting_down()?;
        // This is a "local" limit for how many concurrent requests we allow for a single
        // batch of requests. Requests are still subject to the global limit via self.claimer.
        let mut allowed_requests = self
            .config
            .max_concurrent_requests_per_batch
            .unwrap_or(requests.len());

        // Add as many of remaining requests to the handle as we can, limited by the claimer.
        let try_add = |h: &MultiDriver,
                       reqs: &mut IntoIter<StreamRequest>,
                       allowed_requests: &mut usize|
         -> Result<(), HttpClientError> {
            for claim in self
                .claimer
                .try_claim_requests((*allowed_requests).min(reqs.len()))
            {
                let mut request = match reqs.next() {
                    Some(request) => request,
                    // Shouldn't happen, but just in case.
                    None => break,
                };

                self.event_listeners
                    .trigger_new_request(request.request.ctx_mut());
                h.add(request.into_easy(claim)?)?;

                *allowed_requests -= 1;
            }

            Ok(())
        };

        let mut requests = requests.into_iter();
        let mut stats = Stats::default();

        while requests.len() > 0 {
            let mut multi = self.pool.multi();
            multi
                .get_mut()
                // TODO: don't conflate connections with requests
                .set_max_total_connections(self.config.max_concurrent_requests.unwrap_or(0))?;

            if let Some(max_streams) = self.config.max_concurrent_streams {
                multi.get_mut().set_max_concurrent_streams(max_streams)?;
            }

            let driver = MultiDriver::new(multi.get(), self.config.verbose_stats);

            // Add requests to the driver. This can add anywhere from zero to all the requests.
            try_add(&driver, &mut requests, &mut allowed_requests)?;

            let mut tls_error = false;
            let result = driver
                .perform(|res| {
                    if let Err((_, e)) = &res {
                        let e: HttpClientError = e.clone().into();
                        if let HttpClientError::Tls(_) = e {
                            tls_error = true;
                        }
                    }

                    self.report_result_and_drop_receiver(res)?;

                    allowed_requests += 1;

                    // A request finished - let's see if there are pending requests we can now add
                    // to this multi. This allows pending requests to proceed without needing to
                    // wait for _all_ in-progress requests to finish. Note that there may be other
                    // curl multis active bound by the same request limit, so it is still possible
                    // for our pending requests to wait longer than they need to (i.e. when a
                    // request finishes on a different multi, our loop here will still wait for one
                    // of our requests to finish before trying to enqueue new requests).
                    try_add(&driver, &mut requests, &mut allowed_requests)
                        .map_err(|err| Abort::WithReason(err.into()))
                })
                .inspect(|stats| {
                    self.event_listeners.trigger_stats(stats);
                });

            drop(driver);

            // Don't reuse the connection if we've hit auth issues. We've seen cases where we reuse
            // expired credentials.
            if tls_error {
                multi.discard();
            }

            stats += result?;

            if requests.len() > 0 {
                // We still have pending requests. This likely mean requests on a
                // different multi are using up all the request slots. Add a small sleep
                // to avoid spinning CPU while we wait for requests slots.
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        Ok(stats)
    }

    /// Callback for `MultiDriver::perform` when working with
    /// a `Streaming` handler. Reports the result of the
    /// completed request to the handler's `Receiver`.
    pub(crate) fn report_result_and_drop_receiver(
        &self,
        res: Result<Easy2H, (Easy2H, curl::Error)>,
    ) -> Result<(), Abort> {
        // We need to get the `Easy2` handle in both the
        // success and error cases since we ultimately
        // need to pass the result to the handler contained
        // therein.
        let (mut easy, res) = match res {
            Ok(mut easy) => {
                let ctx = easy.get_mut().request_context_mut();
                let info = ctx.info().clone();
                ctx.event_listeners().trigger_success(&info);
                self.event_listeners.trigger_succeeded_request(ctx);
                (easy, Ok(()))
            }
            Err((mut easy, e)) => {
                let ctx = easy.get_mut().request_context_mut();
                let info = ctx.info().clone();
                ctx.event_listeners().trigger_failure(&info);
                self.event_listeners.trigger_failed_request(ctx);
                let e = maybe_add_os_error(&easy, e);
                (easy, Err(e.into()))
            }
        };

        // Extract the `Receiver` from the `Streaming` handler
        // inside the Easy2 handle. If it's already gone, just
        // log it and move on. (This shouldn't normally happen.)
        if let Some(mut receiver) = easy.get_mut().take_receiver() {
            receiver.done(res)
        } else {
            tracing::error!("Cannot report status because receiver is missing");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::Read;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::sync::Mutex;
    use std::time::Duration;

    use anyhow::Result;
    use futures::future;
    use http::StatusCode;
    use tokio::time::sleep;
    use url::Url;

    use super::*;
    use crate::Method;
    use crate::RequestContext;
    use crate::dispatcher::AsyncRequestDispatcher;
    use crate::receiver::testutil::TestReceiver;

    fn dispatcher_client(worker_threads: usize) -> HttpClient {
        HttpClient::from_config(Config {
            http_worker_threads: worker_threads,
            ..Default::default()
        })
    }

    fn closed_proxy_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test port");
        let addr = listener.local_addr().expect("get local test port");
        drop(listener);
        format!("http://{addr}")
    }

    fn read_request(stream: &mut TcpStream) {
        let mut buf = [0u8; 4096];
        let mut request = Vec::new();
        loop {
            let n = stream.read(&mut buf).expect("read request");
            assert!(n > 0, "client closed before sending full request");
            request.extend_from_slice(&buf[..n]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
    }

    struct RecordingDispatcher {
        seen: Arc<Mutex<Vec<(Option<usize>, bool)>>>,
    }

    impl AsyncRequestDispatcher for RecordingDispatcher {
        fn dispatch(
            &self,
            client: WorkerClient,
            _requests: Vec<StreamRequest>,
        ) -> Result<StatsFuture, HttpClientError> {
            self.seen.lock().unwrap().push((
                client.config.max_concurrent_requests,
                client.config.verbose_stats,
            ));
            Ok(future::ready(Ok(Stats::default())).boxed())
        }
    }

    #[test]
    fn test_client() -> Result<()> {
        let body1 = b"body1";
        let body2 = b"body2";
        let body3 = b"body3";

        let mut server = mockito::Server::new();

        let mock1 = server
            .mock("GET", "/test1")
            .with_status(201)
            .with_body(body1)
            .create();

        let mock2 = server
            .mock("GET", "/test2")
            .with_status(201)
            .with_body(body2)
            .create();

        let mock3 = server
            .mock("GET", "/test3")
            .with_status(201)
            .with_body(body3)
            .create();

        let server_url = Url::parse(&server.url())?;

        let client = HttpClient::new();

        let url1 = server_url.join("test1")?;
        let req1 = client.get(url1);

        let url2 = server_url.join("test2")?;
        let req2 = client.get(url2);

        let url3 = server_url.join("test3")?;
        let req3 = client.get(url3);

        let mut not_received = HashSet::new();
        not_received.insert(body1.to_vec());
        not_received.insert(body2.to_vec());
        not_received.insert(body3.to_vec());

        let stats = client.send(vec![req1, req2, req3], |res| {
            let res = res.unwrap();
            assert_eq!(res.head.status, StatusCode::CREATED);
            assert!(not_received.remove(&*res.body));
            Ok(())
        })?;

        mock1.assert();
        mock2.assert();
        mock3.assert();

        assert!(not_received.is_empty());
        assert_eq!(stats.requests, 3);

        Ok(())
    }

    #[test]
    fn test_stream() -> Result<()> {
        let body1 = b"body1";
        let body2 = b"body2";
        let body3 = b"body3";

        let mut server = mockito::Server::new();

        let mock1 = server
            .mock("GET", "/test1")
            .with_status(201)
            .with_body(body1)
            .create();

        let mock2 = server
            .mock("GET", "/test2")
            .with_status(201)
            .with_body(body2)
            .create();

        let mock3 = server
            .mock("GET", "/test3")
            .with_status(201)
            .with_body(body3)
            .create();

        let server_url = Url::parse(&server.url())?;

        let client = HttpClient::new();

        let url1 = server_url.join("test1")?;
        let rcv1 = TestReceiver::new();
        let req1 = client.get(url1).into_streaming(Box::new(rcv1.clone()));

        let url2 = server_url.join("test2")?;
        let rcv2 = TestReceiver::new();
        let req2 = client.get(url2).into_streaming(Box::new(rcv2.clone()));

        let url3 = server_url.join("test3")?;
        let rcv3 = TestReceiver::new();
        let req3 = client.get(url3).into_streaming(Box::new(rcv3.clone()));

        let stats = client.stream(vec![req1, req2, req3])?;

        mock1.assert();
        mock2.assert();
        mock3.assert();
        assert_eq!(stats.requests, 3);

        assert_eq!(rcv1.status().unwrap(), StatusCode::CREATED);
        let body = rcv1.chunks().into_iter().flatten().collect::<Vec<_>>();
        assert_eq!(&*body, body1);

        assert_eq!(rcv2.status().unwrap(), StatusCode::CREATED);
        let body = rcv2.chunks().into_iter().flatten().collect::<Vec<_>>();
        assert_eq!(&*body, body2);

        assert_eq!(rcv3.status().unwrap(), StatusCode::CREATED);
        let body = rcv3.chunks().into_iter().flatten().collect::<Vec<_>>();
        assert_eq!(&*body, body3);

        Ok(())
    }

    #[tokio::test]
    async fn test_async() -> Result<()> {
        let body1 = b"body1";
        let body2 = b"body2";
        let body3 = b"body3";

        let mut server = mockito::Server::new_async().await;

        let mock1 = server
            .mock("GET", "/test1")
            .with_status(201)
            .with_body(body1)
            .create();

        let mock2 = server
            .mock("GET", "/test2")
            .with_status(201)
            .with_body(body2)
            .create();

        let mock3 = server
            .mock("GET", "/test3")
            .with_status(201)
            .with_body(body3)
            .create();

        let server_url = Url::parse(&server.url())?;

        let client = HttpClient::new();

        let url1 = server_url.join("test1")?;
        let req1 = client.get(url1);

        let url2 = server_url.join("test2")?;
        let req2 = client.get(url2);

        let url3 = server_url.join("test3")?;
        let req3 = client.get(url3);

        let (futures, stats) = client.send_async(vec![req1, req2, req3])?;

        let mut responses = Vec::new();
        for fut in futures {
            responses.push(fut.await?);
        }

        mock1.assert();
        mock2.assert();
        mock3.assert();

        let mut not_received = HashSet::new();
        not_received.insert(body1.to_vec());
        not_received.insert(body2.to_vec());
        not_received.insert(body3.to_vec());

        for res in responses {
            assert_eq!(res.head.status, StatusCode::CREATED);
            let body = res.into_body().raw().try_concat().await?;
            assert!(not_received.remove(&*body));
        }

        assert!(not_received.is_empty());

        let stats = stats.await?;
        assert_eq!(stats.requests, 3);

        Ok(())
    }

    #[tokio::test]
    async fn test_async_dispatcher_empty_batch() -> Result<()> {
        let client = dispatcher_client(1);

        let (responses, stats) = client.send_async(Vec::new())?;

        assert!(responses.is_empty());
        assert_eq!(stats.await?, Stats::default());
        Ok(())
    }

    #[test]
    fn test_async_dispatcher_shutdown_cancels_waiting_batch() -> Result<()> {
        let client = dispatcher_client(1).max_concurrent_requests(Some(1));
        let _held_claim = client.claimer.claim_request();
        let request = client.get(Url::parse("http://example.com/test")?);
        let (responses, stats) = client.send_async(vec![request])?;
        drop(responses);

        let (shutdown_tx, shutdown_rx) = crossbeam::channel::bounded(1);
        let shutdown_thread = std::thread::spawn(move || {
            drop(client);
            shutdown_tx.send(()).unwrap();
        });

        shutdown_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("dispatcher shutdown should cancel a batch waiting for a request slot");
        shutdown_thread.join().unwrap();
        assert!(futures::executor::block_on(stats).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_async_dispatcher_mixed_success_and_failure() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let body = b"body";

        let mock = server
            .mock("GET", "/test")
            .with_status(201)
            .with_body(body)
            .create();

        let server_url = Url::parse(&server.url())?;
        let client = dispatcher_client(2);

        let success = client.get(server_url.join("test")?);

        let mut failure = client.get(server_url.join("test")?);
        failure
            .set_http_proxy_host(Some(closed_proxy_url()))
            .set_http_no_proxy(Some(String::new()));

        let (responses, stats) = client.send_async(vec![success, failure])?;
        assert_eq!(responses.len(), 2);
        let mut responses = responses.into_iter();
        let success = responses.next().unwrap();
        let failure = responses.next().unwrap();

        let success = success.await?;
        assert_eq!(success.head.status, StatusCode::CREATED);
        assert_eq!(success.into_body().raw().try_concat().await?, body);

        assert!(failure.await.is_err());

        let stats = stats.await?;
        assert_eq!(stats.requests, 2);

        mock.assert();
        Ok(())
    }

    #[tokio::test]
    async fn test_async_dispatcher_waits_for_request_slots() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let body = b"body";

        let _mock = server
            .mock("GET", "/test")
            .with_status(201)
            .with_body(body)
            .create();

        let server_url = Url::parse(&server.url())?;
        let client = dispatcher_client(1).max_concurrent_requests(Some(1));

        let held_claim = client.claimer.claim_request();

        let request = client.get(server_url.join("test")?);
        let (responses, stats) = client.send_async(vec![request])?;
        let response = responses.into_iter().next().unwrap();

        sleep(Duration::from_millis(20)).await;
        drop(held_claim);

        let response = response.await?;
        assert_eq!(response.head.status, StatusCode::CREATED);
        assert_eq!(response.into_body().raw().try_concat().await?, body);

        let stats = stats.await?;
        assert_eq!(stats.requests, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_async_dispatcher_waits_for_multiple_queued_batches() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let body1 = b"body1";
        let body2 = b"body2";

        let mock1 = server
            .mock("GET", "/test1")
            .with_status(201)
            .with_body(body1)
            .create();
        let mock2 = server
            .mock("GET", "/test2")
            .with_status(201)
            .with_body(body2)
            .create();

        let server_url = Url::parse(&server.url())?;
        let client = dispatcher_client(1).max_concurrent_requests(Some(1));

        let held_claim = client.claimer.claim_request();

        let (responses1, stats1) =
            client.send_async(vec![client.get(server_url.join("test1")?)])?;
        let (responses2, stats2) =
            client.send_async(vec![client.get(server_url.join("test2")?)])?;

        sleep(Duration::from_millis(20)).await;
        drop(held_claim);

        let response1 = responses1.into_iter().next().unwrap().await?;
        assert_eq!(response1.head.status, StatusCode::CREATED);
        assert_eq!(response1.into_body().raw().try_concat().await?, body1);

        let response2 = responses2.into_iter().next().unwrap().await?;
        assert_eq!(response2.head.status, StatusCode::CREATED);
        assert_eq!(response2.into_body().raw().try_concat().await?, body2);

        assert_eq!(stats1.await?.requests, 1);
        assert_eq!(stats2.await?.requests, 1);

        mock1.assert();
        mock2.assert();
        Ok(())
    }

    #[tokio::test]
    async fn test_async_dispatcher_waiting_batch_preserves_real_error() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let body = b"body";

        let mock = server
            .mock("GET", "/test")
            .with_status(201)
            .with_body(body)
            .create();

        let server_url = Url::parse(&server.url())?;
        let client = dispatcher_client(1).max_concurrent_requests(Some(1));

        let held_claim = client.claimer.claim_request();

        let success = client.get(server_url.join("test")?);
        let mut failure = client.get(server_url.join("test")?);
        failure
            .set_http_proxy_host(Some(closed_proxy_url()))
            .set_http_no_proxy(Some(String::new()));

        let (responses, stats) = client.send_async(vec![success, failure])?;
        let mut responses = responses.into_iter();
        let success = responses.next().unwrap();
        let failure = responses.next().unwrap();

        sleep(Duration::from_millis(20)).await;
        drop(held_claim);

        let success = success.await?;
        assert_eq!(success.head.status, StatusCode::CREATED);
        assert_eq!(success.into_body().raw().try_concat().await?, body);

        let err = match failure.await {
            Ok(_) => panic!("expected queued failure request to fail"),
            Err(err) => err.to_string(),
        };
        assert!(
            !err.contains("dropped before it could complete"),
            "expected the queued request to surface its real error, got: {err}"
        );

        assert_eq!(stats.await?.requests, 2);

        mock.assert();
        Ok(())
    }

    #[tokio::test]
    async fn test_async_dispatcher_dropped_response_does_not_poison_siblings() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let body1 = b"body1";
        let body2 = b"body2";

        let mock1 = server
            .mock("GET", "/test1")
            .with_status(201)
            .with_body(body1)
            .create();
        let mock2 = server
            .mock("GET", "/test2")
            .with_status(201)
            .with_body(body2)
            .create();

        let server_url = Url::parse(&server.url())?;
        let client = dispatcher_client(2);

        let (responses, stats) = client.send_async(vec![
            client.get(server_url.join("test1")?),
            client.get(server_url.join("test2")?),
        ])?;
        let mut responses = responses.into_iter();
        drop(responses.next().unwrap());
        let survivor = responses.next().unwrap();

        let survivor = survivor.await?;
        assert_eq!(survivor.head.status, StatusCode::CREATED);
        assert_eq!(survivor.into_body().raw().try_concat().await?, body2);

        assert_eq!(stats.await?.requests, 2);

        mock1.assert();
        mock2.assert();
        Ok(())
    }

    #[tokio::test]
    async fn test_async_dispatcher_uses_builder_updated_multi_limits() -> Result<()> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let client = HttpClient::from_config(Config {
            http_worker_threads: 1,
            limit_requests: false,
            ..Default::default()
        })
        .max_concurrent_requests(Some(1))
        .verbose_stats(true);
        let client = HttpClient {
            dispatcher: Arc::new(RecordingDispatcher { seen: seen.clone() }),
            ..client
        };

        let req = client.get(Url::parse("http://example.com/test")?);
        let (_responses, stats) = client.send_async(vec![req])?;
        assert_eq!(stats.await?, Stats::default());
        assert_eq!(*seen.lock().unwrap(), vec![(Some(1), true)]);

        Ok(())
    }

    #[tokio::test]
    async fn test_event_listeners() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let server_url = Url::parse(&server.url())?;

        // this is actually used, it changes how mockito behaves
        const BODY: &[u8] = b"body";
        let _mock1 = server
            .mock("GET", "/test1")
            .with_status(201)
            .with_body(BODY)
            .create();

        let url = server_url.join("test1")?;

        let (tx, rx) = crossbeam::channel::unbounded();
        let (msg_tx, msg_rx) = crossbeam::channel::unbounded();

        let client = HttpClient::new().with_event_listeners(|l| {
            l.on_stats(move |stats| tx.send(stats.clone()).expect("send stats over channel"));

            let check_request = &|r: &RequestContext| assert!(matches!(r.method(), Method::Get));

            l.on_new_request({
                let msg_tx = msg_tx.clone();
                move |r| {
                    check_request(r);
                    msg_tx.send("on_new_request").unwrap();

                    let l = r.event_listeners();
                    l.on_download_bytes({
                        let msg_tx = msg_tx.clone();
                        move |req, n| {
                            assert_eq!(n, BODY.len());
                            check_request(req);
                            msg_tx.send("on_download_bytes").unwrap();
                        }
                    });
                    l.on_upload_bytes({
                        let msg_tx = msg_tx.clone();
                        move |req, n| {
                            assert_eq!(n, 0);
                            check_request(req);
                            msg_tx.send("on_upload_bytes").unwrap();
                        }
                    });
                    l.on_content_length({
                        let msg_tx = msg_tx.clone();
                        move |req, n| {
                            assert_eq!(n, BODY.len());
                            check_request(req);
                            msg_tx.send("on_content_length").unwrap();
                        }
                    });
                }
            });

            l.on_failed_request({
                let msg_tx = msg_tx.clone();
                move |r| {
                    check_request(r);
                    msg_tx.send("on_failed_request").unwrap();
                }
            });

            l.on_succeeded_request({
                let msg_tx = msg_tx.clone();
                move |r| {
                    check_request(r);
                    msg_tx.send("on_succeeded_request").unwrap();
                }
            });
        });

        let check_events = |has_content_length: bool| {
            assert_eq!(msg_rx.recv().unwrap(), "on_new_request");
            if has_content_length {
                assert_eq!(msg_rx.recv().unwrap(), "on_content_length");
            }
            assert_eq!(msg_rx.recv().unwrap(), "on_download_bytes");
            assert_eq!(msg_rx.recv().unwrap(), "on_succeeded_request");
        };

        let request = client.get(url);

        let stats = client.send(vec![request.clone()], |_| Ok(()))?;
        assert_eq!(stats, rx.recv()?);
        check_events(true);

        let stats = client.send(vec![request.clone()], |_| Ok(()))?;
        assert_eq!(stats, rx.recv()?);
        check_events(true);

        let (_stream, stats) = client.send_async(vec![request.clone()])?;
        let stats = stats.await?;
        assert_eq!(stats, rx.recv()?);
        check_events(false);

        let (_stream, stats) = client.send_async(vec![request.clone()])?;
        let stats = stats.await?;
        assert_eq!(stats, rx.recv()?);
        check_events(false);

        let my_stream_req = || {
            request
                .clone()
                .into_streaming(Box::new(TestReceiver::new()))
        };

        let stats = client.stream(vec![my_stream_req()])?;
        assert_eq!(stats, rx.recv()?);
        check_events(false);

        let stats = client.stream(vec![my_stream_req()])?;
        assert_eq!(stats, rx.recv()?);
        check_events(false);

        drop((client, msg_tx));

        // All msg_tx should be dropped. recv() should not be blocking.
        msg_rx.recv().unwrap_err();

        Ok(())
    }

    #[tokio::test]
    async fn test_async_dispatcher_event_listeners_on_failure() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let server_url = Url::parse(&server.url())?;

        let _mock = server
            .mock("GET", "/test")
            .with_status(201)
            .with_body("body")
            .create();

        let (stats_tx, stats_rx) = crossbeam::channel::unbounded();
        let (msg_tx, msg_rx) = crossbeam::channel::unbounded();
        let client = dispatcher_client(2).with_event_listeners(|l| {
            l.on_stats(move |stats| {
                stats_tx
                    .send(stats.clone())
                    .expect("send stats over channel")
            });
            l.on_new_request({
                let msg_tx = msg_tx.clone();
                move |_r| {
                    msg_tx.send("new").unwrap();
                }
            });
            l.on_succeeded_request({
                let msg_tx = msg_tx.clone();
                move |_r| {
                    msg_tx.send("success").unwrap();
                }
            });
            l.on_failed_request({
                let msg_tx = msg_tx.clone();
                move |_r| {
                    msg_tx.send("failure").unwrap();
                }
            });
        });

        let success = client.get(server_url.join("test")?);
        let mut failure = client.get(server_url.join("test")?);
        failure
            .set_http_proxy_host(Some(closed_proxy_url()))
            .set_http_no_proxy(Some(String::new()));

        let (responses, stats) = client.send_async(vec![success, failure])?;
        assert_eq!(responses.len(), 2);
        let mut responses = responses.into_iter();
        let success = responses.next().unwrap();
        let failure = responses.next().unwrap();

        success.await?;
        assert!(failure.await.is_err());

        let stats = stats.await?;
        assert_eq!(stats, stats_rx.recv()?);
        assert_eq!(stats.requests, 2);

        let mut messages = vec![
            msg_rx.recv()?,
            msg_rx.recv()?,
            msg_rx.recv()?,
            msg_rx.recv()?,
        ];
        messages.sort_unstable();
        assert_eq!(messages, vec!["failure", "new", "new", "success"]);

        Ok(())
    }
}
