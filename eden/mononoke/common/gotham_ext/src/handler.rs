/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io;
use std::net::SocketAddr;
use std::panic::RefUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::future::FutureExt;
use futures::task;
use gotham::handler::Handler;
use gotham::handler::HandlerFuture;
use gotham::handler::IntoResponse;
use gotham::handler::NewHandler;
use gotham::helpers::http::Body;
use gotham::state::State;
use gotham_derive::StateData;
use http::Request;
use http::Response;
use http_body_util::BodyExt as _;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::Incoming;
use tower_service::Service;

use crate::middleware::Middleware;
use crate::middleware::artillery::ArtilleryRootEndpoint;
use crate::middleware::artillery::ArtilleryTracingEnabled;
use crate::middleware::artillery::artillery_http_tracing_enabled;
use crate::middleware::post_request::PostResponseTask;
use crate::socket_data::TlsSocketData;

#[derive(StateData, Clone, PartialEq, Copy, Debug)]
pub enum SlapiCommitIdentityScheme {
    Hg,
    Git,
}

/// Labels a request for Artillery trace rooting, for the case where it arrives
/// with no inbound context to continue.
///
/// `Some(endpoint)` binds `endpoint` to the `mononoke_endpoint` Luna sample
/// key, leaving the policy to decide whether to sample the request and at what
/// rate. `None` means the request may never root, whatever the policy says.
///
/// Servers that never call [`MononokeHttpHandlerBuilder::with_root_trace_selector`]
/// behave exactly as they do today.
///
/// Runs before routing, so it sees only the raw request and must not depend on
/// anything a handler puts into `State`. It is on the path of every request,
/// so it must be cheap.
pub type RootTraceSelector = fn(&State) -> Option<&'static str>;

#[derive(Clone)]
pub struct MononokeHttpHandler<H> {
    inner: H,
    middleware: Arc<Vec<Box<dyn Middleware>>>,
    root_trace_selector: Option<RootTraceSelector>,
}

impl<H> MononokeHttpHandler<H> {
    pub fn into_service<T>(
        self,
        addr: SocketAddr,
        tls_socket_data: Option<TlsSocketData>,
    ) -> MononokeHttpHandlerAsService<H, T> {
        MononokeHttpHandlerAsService {
            handler: self,
            addr,
            tls_socket_data,
            state: None,
        }
    }

    pub fn into_service_with_state<T: gotham::state::StateData>(
        self,
        addr: SocketAddr,
        tls_socket_data: Option<TlsSocketData>,
        state: T,
    ) -> MononokeHttpHandlerAsService<H, T> {
        MononokeHttpHandlerAsService {
            handler: self,
            addr,
            tls_socket_data,
            state: Some(state),
        }
    }
}

// This trait is boilerplate to let us bind this in Gotham as the service. This is essentially
// patterned match off of how the Router Handler works in Gotham.
impl<H: Handler + Clone + Send + Sync + 'static + RefUnwindSafe> NewHandler
    for MononokeHttpHandler<H>
{
    type Instance = MononokeHttpHandler<H>;

    fn new_handler(&self) -> Result<Self::Instance, anyhow::Error> {
        Ok(self.clone())
    }
}

/// Executes a stack middleware. If any Middleware instance preempts the request, it returns the
/// index of the first middleware that didn't run and the response.
async fn run_middleware(
    middleware: &[Box<dyn Middleware>],
    state: &mut State,
) -> Option<(usize, Response<Body>)> {
    for (i, m) in middleware.iter().enumerate() {
        if let Some(response) = m.inbound(state).await {
            return Some((i + 1, response));
        }
    }

    None
}

// Gotham's router only runs middleware on route matches, which means we don't get any visibility
// into e.g. 404's. So, we use this handler to wrap Gotham's router and run our own middleware
// stack. Another reason not to use Gotham's middleware stack is that it places all middleware
// instances in the poll chain, so for e.g. an upload, we'd chain poll() calls through every single
// instance of middleware. In contrast, our middleware stack is completely out of the way once it
// has finished executing.
impl<H: Handler + Send + Sync + 'static> Handler for MononokeHttpHandler<H> {
    fn handle(self, mut state: State) -> Pin<Box<HandlerFuture>> {
        let tracing_enabled = artillery_http_tracing_enabled();
        let root_endpoint = if tracing_enabled {
            self.root_trace_selector.and_then(|select| select(&state))
        } else {
            None
        };
        state.put(ArtilleryTracingEnabled(tracing_enabled));
        state.put(ArtilleryRootEndpoint(root_endpoint));

        let fut = async move {
            // On request, middleware is called in order, then called the other way around on
            // response (this is what regular Router middleware in Gotham would do).
            let (idx, mut state, mut response) =
                match run_middleware(self.middleware.as_ref(), &mut state).await {
                    Some((idx, r)) => (idx, state, r),
                    None => {
                        // NOTE: It's a bit unfortunate that we have to return a HandlerFuture here
                        // when really we'd rather be working with just (State, HttpResponse<Body>)
                        // everywhere, but that's how it is in Gotham.
                        let (state, res) = match self.inner.handle(state).await {
                            Ok((state, res)) => (state, res),
                            Err((state, err)) => {
                                let response = err.into_response(&state);
                                (state, response)
                            }
                        };

                        let idx = self.middleware.len();
                        (idx, state, res)
                    }
                };

            // On outbound, only run middleware that did run in inbound.
            for m in self.middleware[0..idx].iter().rev() {
                m.outbound(&mut state, &mut response).await;
            }

            (state, response)
        };

        let fut = request_context_ext::with_fresh_request_context(fut);
        async move {
            let (mut state, response) = fut.await;
            if let Some(task) = state.try_take::<PostResponseTask>() {
                task.spawn();
            }
            Ok((state, response))
        }
        .boxed()
    }
}

// NOTE: This is just syntactic sugar for MononokeHttpHandler::builder(), which is why the
// "handler" type is () here.
impl MononokeHttpHandler<()> {
    pub fn builder() -> MononokeHttpHandlerBuilder {
        MononokeHttpHandlerBuilder {
            middleware: vec![],
            root_trace_selector: None,
        }
    }
}

pub struct MononokeHttpHandlerBuilder {
    middleware: Vec<Box<dyn Middleware>>,
    root_trace_selector: Option<RootTraceSelector>,
}

impl MononokeHttpHandlerBuilder {
    #[allow(clippy::should_implement_trait)]
    pub fn add<T: Middleware>(mut self, middleware: T) -> Self {
        self.middleware.push(Box::new(middleware));
        self
    }

    /// Opt this server in to rooting Artillery traces for selected requests
    /// that arrive without inbound context. Servers that do not call this are
    /// unaffected. See [`RootTraceSelector`].
    pub fn with_root_trace_selector(mut self, selector: RootTraceSelector) -> Self {
        self.root_trace_selector = Some(selector);
        self
    }

    pub fn build<H: Handler + Send + Sync + 'static>(self, inner: H) -> MononokeHttpHandler<H> {
        MononokeHttpHandler {
            inner,
            middleware: Arc::new(self.middleware),
            root_trace_selector: self.root_trace_selector,
        }
    }
}

/// This is an instance of MononokeHttpHandlerAsService that is connected to a client. We can use
/// it to call into Gotham explicitly, or use it as a Hyper service.
#[derive(Clone)]
pub struct MononokeHttpHandlerAsService<H, T> {
    handler: MononokeHttpHandler<H>,
    addr: SocketAddr,
    tls_socket_data: Option<TlsSocketData>,
    state: Option<T>,
}

impl<H, T> MononokeHttpHandlerAsService<H, T>
where
    H: Handler + Clone + Send + Sync + 'static + RefUnwindSafe,
    T: gotham::state::StateData + Clone,
{
    pub async fn call_gotham(self, req: Request<Body>) -> Response<Body> {
        let mut state = State::from_request(req, self.addr);
        if let Some(tls_socket_data) = self.tls_socket_data {
            tls_socket_data.populate_state(&mut state);
        }

        if let Some(s) = self.state {
            state.put(s);
        }

        match self.handler.handle(state).await {
            Ok((_state, res)) => res,
            Err((state, err)) => err.into_response(&state),
        }
    }
}

impl<H, T> Service<Request<Incoming>> for MononokeHttpHandlerAsService<H, T>
where
    H: Handler + Clone + Send + Sync + 'static + RefUnwindSafe,
    T: gotham::state::StateData + Clone,
{
    type Response = Response<Body>;
    type Error = anyhow::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut task::Context<'_>) -> task::Poll<Result<(), Self::Error>> {
        task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Incoming>) -> Self::Future {
        let this = self.clone();

        async move {
            let res = this
                .call_gotham(req.map(|body| UnsyncBoxBody::new(body.map_err(io::Error::other))))
                .await;
            Ok(res)
        }
        .boxed()
    }
}

#[cfg(test)]
mod test {
    use futures::future;
    use gotham::test::TestServer;
    use gotham_derive::StateData;
    use http::StatusCode;
    use mononoke_macros::mononoke;

    use super::*;

    // Basic response handler for tests

    #[derive(Clone)]
    struct TestHandler;

    impl Handler for TestHandler {
        fn handle(self, state: State) -> Pin<Box<HandlerFuture>> {
            let response = Response::builder()
                .status(StatusCode::OK)
                .body(Body::default())
                .unwrap();

            future::ready(Ok((state, response))).boxed()
        }
    }

    // Handler that expects to not be called

    #[derive(Clone)]
    struct PanicHandler;

    impl Handler for PanicHandler {
        fn handle(self, _state: State) -> Pin<Box<HandlerFuture>> {
            panic!("PanicHandler::handle was called")
        }
    }

    // Basic middleware for tests

    struct NoopMiddleware;

    #[async_trait::async_trait]
    impl Middleware for NoopMiddleware {}

    // MiddlewareValueMiddleware is used below for asserting that middleware is called in the right
    // order.

    #[derive(StateData)]
    pub struct MiddlewareValue(u64);

    struct MiddlewareValueMiddleware(Option<u64>, u64);

    #[async_trait::async_trait]
    impl Middleware for MiddlewareValueMiddleware {
        async fn inbound(&self, state: &mut State) -> Option<Response<Body>> {
            assert_eq!(state.try_borrow::<MiddlewareValue>().map(|v| v.0), self.0);
            state.put(MiddlewareValue(self.1));
            None
        }

        async fn outbound(&self, state: &mut State, _response: &mut Response<Body>) {
            assert_eq!(state.take::<MiddlewareValue>().0, self.1);

            if let Some(v) = self.0 {
                state.put(MiddlewareValue(v))
            }
        }
    }

    // InterceptMiddleware is used to check that middleware can intercept requests.

    struct InterceptMiddleware;

    #[async_trait::async_trait]
    impl Middleware for InterceptMiddleware {
        async fn inbound(&self, _state: &mut State) -> Option<Response<Body>> {
            let response = Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::default())
                .unwrap();
            Some(response)
        }

        async fn outbound(&self, _state: &mut State, response: &mut Response<Body>) {
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }

    // PanicMiddleware is used to check that intercepting a request interrupts the call chain.

    struct PanicMiddleware;

    #[async_trait::async_trait]
    impl Middleware for PanicMiddleware {
        async fn inbound(&self, _state: &mut State) -> Option<Response<Body>> {
            panic!("PanicMiddleware::inbound was called");
        }

        async fn outbound(&self, _state: &mut State, _response: &mut Response<Body>) {
            panic!("PanicMiddleware::outbound was called");
        }
    }

    #[mononoke::test]
    fn test_empty() -> Result<(), anyhow::Error> {
        let handler = MononokeHttpHandler::builder().build(TestHandler);
        let server = TestServer::new(handler)?;
        let res = server.client().get("http://host/").perform()?;
        assert_eq!(res.status(), StatusCode::OK);
        Ok(())
    }

    #[mononoke::test]
    fn test_noop() -> Result<(), anyhow::Error> {
        let handler = MononokeHttpHandler::builder()
            .add(NoopMiddleware)
            .add(NoopMiddleware)
            .build(TestHandler);
        let server = TestServer::new(handler)?;
        let res = server.client().get("http://host/").perform()?;
        assert_eq!(res.status(), StatusCode::OK);
        Ok(())
    }

    #[mononoke::test]
    fn test_chain() -> Result<(), anyhow::Error> {
        let handler = MononokeHttpHandler::builder()
            .add(MiddlewareValueMiddleware(None, 1))
            .add(MiddlewareValueMiddleware(Some(1), 2))
            .build(TestHandler);
        let server = TestServer::new(handler)?;
        let res = server.client().get("http://host/").perform()?;
        assert_eq!(res.status(), StatusCode::OK);
        Ok(())
    }

    #[mononoke::test]
    fn test_intercept_alone() -> Result<(), anyhow::Error> {
        let handler = MononokeHttpHandler::builder()
            .add(InterceptMiddleware)
            .build(TestHandler);
        let server = TestServer::new(handler)?;
        let res = server.client().get("http://host/").perform()?;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[mononoke::test]
    fn test_intercept_chain() -> Result<(), anyhow::Error> {
        let handler = MononokeHttpHandler::builder()
            .add(MiddlewareValueMiddleware(None, 1))
            .add(MiddlewareValueMiddleware(Some(1), 2))
            .add(InterceptMiddleware)
            .add(PanicMiddleware)
            .build(PanicHandler);
        let server = TestServer::new(handler)?;
        let res = server.client().get("http://host/").perform()?;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[cfg(fbcode_build)]
    #[mononoke::fbinit_test]
    fn post_response_waits_for_final_handler_cpu(fb: fbinit::FacebookInit) {
        use std::hint::black_box;
        use std::sync::atomic::AtomicI64;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        use context::CoreContext;
        use context::PerfCounterType;
        use futures::channel::oneshot;

        use crate::middleware::PostResponseCallbacks;
        use crate::middleware::PostResponseMiddleware;
        use crate::middleware::request_context::RequestContext;
        use crate::response::EmptyBody;
        use crate::response::TryIntoResponse;

        struct CpuHandler {
            ctx: CoreContext,
            reported: oneshot::Sender<i64>,
        }

        impl Handler for CpuHandler {
            fn handle(self, mut state: State) -> Pin<Box<HandlerFuture>> {
                self.ctx.track_cpu_time();
                state.put(RequestContext {
                    ctx: self.ctx.clone(),
                });
                state.borrow_mut::<PostResponseCallbacks>().add(move |_| {
                    let _ = self.reported.send(
                        self.ctx
                            .perf_counters()
                            .get_counter(PerfCounterType::CpuTimeNs),
                    );
                });
                let response = EmptyBody::new().try_into_response(&mut state).unwrap();
                future::ready(Ok((state, response))).boxed()
            }
        }

        struct FinalCpuWork {
            before: Arc<AtomicI64>,
        }

        #[async_trait::async_trait]
        impl Middleware for FinalCpuWork {
            async fn outbound(&self, state: &mut State, _response: &mut Response<Body>) {
                tokio::task::yield_now().await;
                self.before.store(
                    state
                        .borrow::<RequestContext>()
                        .ctx
                        .perf_counters()
                        .get_counter(PerfCounterType::CpuTimeNs),
                    Ordering::Relaxed,
                );
                let mut value = 1u64;
                for _ in 0..100_000 {
                    value = black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
                }
            }
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let ctx = CoreContext::new(fb);
            let before = Arc::new(AtomicI64::new(0));
            let (reported, receiver) = oneshot::channel();
            let handler = MononokeHttpHandler::builder()
                .add(FinalCpuWork {
                    before: before.clone(),
                })
                .add(PostResponseMiddleware::default())
                .build(CpuHandler { ctx, reported });
            let state = State::from_request(
                Request::new(Body::default()),
                "127.0.0.1:0".parse().unwrap(),
            );
            assert!(handler.handle(state).await.is_ok());
            let cpu = tokio::time::timeout(Duration::from_secs(5), receiver)
                .await
                .unwrap()
                .unwrap();
            assert!(
                cpu > before.load(Ordering::Relaxed),
                "final poll CPU was missing"
            );
        });
    }
}
