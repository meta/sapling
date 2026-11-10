/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Provider-neutral distributed tracing hooks for Sapling.

use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use atexit::AtExit;

/// Name/value pairs for one HTTP request or child process.
pub type Headers = Vec<(String, String)>;

/// Tracing backend registered by the embedding binary.
pub trait Provider: Send + Sync {
    /// Start tracing the current command.
    fn start(&self);
    /// Record the code the process exits with, before `finish`.
    fn exit_code(&self, code: i32);
    /// Finish the trace started by `start`.
    fn finish(&self);
    /// Propagation headers for an outgoing HTTP request.
    fn outgoing_http(&self, url: &str) -> Headers;
    /// Environment variables that continue the trace in a child process.
    fn outgoing_env(&self) -> Headers;
}

static PROVIDER: OnceLock<Box<dyn Provider>> = OnceLock::new();
static STARTED: AtomicBool = AtomicBool::new(false);

/// Register the tracing provider and attach it to Sapling's HTTP client. The first call wins.
pub fn register_provider(provider: impl Provider + 'static) {
    if PROVIDER.set(Box::new(provider)).is_ok() {
        http_client::Request::on_new_request(|request| {
            for (name, value) in outgoing_http(request.ctx().url().as_str()) {
                request.set_header(name, value);
            }
        });
    }
}

/// Trace until the returned guard drops. Nested commands join the outermost trace.
pub fn scope(enabled: bool) -> Option<AtExit> {
    let provider = PROVIDER.get().filter(|_| enabled)?;
    if STARTED.swap(true, Ordering::AcqRel) {
        return None;
    }
    provider.start();
    Some(AtExit::new(
        "distributed tracing",
        Box::new(|| {
            provider.finish();
            STARTED.store(false, Ordering::Release);
        }),
    ))
}

fn active() -> Option<&'static dyn Provider> {
    PROVIDER
        .get()
        .filter(|_| STARTED.load(Ordering::Acquire))
        .map(|provider| provider.as_ref())
}

/// Record the code the process exits with on the active trace.
pub fn exit_code(code: i32) {
    if let Some(provider) = active() {
        provider.exit_code(code);
    }
}

/// Propagation headers for an outgoing HTTP request to `url`.
pub fn outgoing_http(url: &str) -> Headers {
    active().map_or_else(Vec::new, |provider| provider.outgoing_http(url))
}

/// Environment variables that continue the trace in a child process.
pub fn outgoing_env() -> Headers {
    active().map_or_else(Vec::new, |provider| provider.outgoing_env())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicI32;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    static STARTS: AtomicUsize = AtomicUsize::new(0);
    static FINISHES: AtomicUsize = AtomicUsize::new(0);
    static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

    struct TestProvider;

    impl Provider for TestProvider {
        fn start(&self) {
            STARTS.fetch_add(1, Ordering::Relaxed);
        }

        fn exit_code(&self, code: i32) {
            EXIT_CODE.store(code, Ordering::Relaxed);
        }

        fn finish(&self) {
            FINISHES.fetch_add(1, Ordering::Relaxed);
        }

        fn outgoing_http(&self, _url: &str) -> Headers {
            vec![("traceparent".to_owned(), "propagated".to_owned())]
        }

        fn outgoing_env(&self) -> Headers {
            vec![("CHILD".to_owned(), "traced".to_owned())]
        }
    }

    #[test]
    fn outermost_scope_traces_http_and_children() {
        register_provider(TestProvider);
        assert!(scope(false).is_none());
        assert_eq!(outgoing_env(), Headers::new());
        exit_code(1);
        assert_eq!(EXIT_CODE.load(Ordering::Relaxed), 0, "no trace, no code");

        let outer = scope(true).expect("outermost command should trace");
        assert!(
            scope(true).is_none(),
            "nested commands join the outer trace"
        );
        assert_eq!(STARTS.load(Ordering::Relaxed), 1);
        assert_eq!(
            outgoing_env(),
            vec![("CHILD".to_owned(), "traced".to_owned())]
        );

        let mut server = mockito::Server::new();
        let traced = server
            .mock("GET", "/")
            .match_header("traceparent", "propagated")
            .create();
        http_client::HttpClient::new()
            .get(server.url().parse().expect("mock URL should parse"))
            .send()
            .expect("traced request should succeed");
        traced.assert();

        exit_code(3);
        assert_eq!(EXIT_CODE.load(Ordering::Relaxed), 3);
        drop(outer);
        assert_eq!(FINISHES.load(Ordering::Relaxed), 1);
        exit_code(4);
        assert_eq!(EXIT_CODE.load(Ordering::Relaxed), 3, "no code after finish");
        assert_eq!(outgoing_env(), Headers::new(), "inactive after the scope");
        drop(scope(true).expect("a later command traces again"));
        assert_eq!(STARTS.load(Ordering::Relaxed), 2);
    }
}
