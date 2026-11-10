/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The hg-http crate provides common utilities for dealing setting up and
//! managing http requests for the hg application. This crate specifies how
//! a topic should be treated. Topics may include monitoring, request setup,
//! paths, error handling, etc.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::sync::Once;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;

use auth::AuthGroup;
use auth::AuthSection;
use clientinfo::ClientInfo;
use configmodel::ConfigExt;
use configmodel::convert::ByteCount;
use hg_metrics::increment_counter;
use http_client::AuthResolver;
use http_client::HttpClient;
use http_client::Request;
use http_client::ResolvedAuth;
use http_client::Stats;
use progress_model::AggregatingProgressBar;
use progress_model::IoSample;
use progress_model::IoTimeSeries;
use progress_model::ProgressBar;
use progress_model::Registry;
use url::Url;

#[derive(Default)]
struct Total {
    download_bytes: AtomicUsize,
    upload_bytes: AtomicUsize,
    request_count: AtomicUsize,
}

// Total progress from all clients.
static TOTAL: Total = Total {
    download_bytes: AtomicUsize::new(0),
    upload_bytes: AtomicUsize::new(0),
    request_count: AtomicUsize::new(0),
};

pub fn current_progress() -> (usize, usize, usize) {
    (
        TOTAL.download_bytes.load(Relaxed),
        TOTAL.upload_bytes.load(Relaxed),
        TOTAL.request_count.load(Relaxed),
    )
}

pub fn http_client(client_id: impl ToString, config: http_client::Config) -> HttpClient {
    let client_id = client_id.to_string();
    let reporter = move |stats: &Stats| {
        bump_counters(&client_id, stats);
    };
    HttpClient::from_config(config).with_event_listeners(|l| {
        l.on_stats(reporter);
    })
}

/// Generate http_client::Config taking into account hg specific config/auth
/// settings. url_for_auth will should be the URL to connect to, and will be
/// used to look up TLS credentials.
pub fn http_config(
    config: &dyn configmodel::Config,
    url_for_auth: &Url,
) -> Result<http_client::Config, auth::MissingCerts> {
    let url = url_for_auth.clone();
    http_config_with_auth(config, url_for_auth, move |auth| auth.best_match_for(&url))
}

/// Like `http_config`, with `select_auth` choosing the `[auth]` group for the TLS
/// credentials to connect to `url`.
pub fn http_config_with_auth(
    config: &dyn configmodel::Config,
    url: &Url,
    select_auth: impl Fn(&AuthSection) -> Result<Option<AuthGroup>, auth::MissingCerts>
    + Send
    + Sync
    + 'static,
) -> Result<http_client::Config, auth::MissingCerts> {
    let mut hc = http_client::Config {
        client_info: ClientInfo::new().and_then(|i| i.to_json()).ok(),
        disable_tls_verification: INSECURE_MODE.load(Relaxed),
        read_buffer_size: config
            .get_opt::<ByteCount>("http", "read-buffer-size")
            .unwrap_or_default()
            .map(ByteCount::value),
        write_buffer_size: config
            .get_opt::<ByteCount>("http", "write-buffer-size")
            .unwrap_or_default()
            .map(ByteCount::value),
        unix_socket_path: config
            .get_nonempty_opt("auth_proxy", "unix_socket_path")
            .expect("Can't get auth_proxy.unix_socket_path config"),
        unix_socket_domains: HashSet::from_iter(
            config
                .get_or("auth_proxy", "unix_socket_domains", Vec::new)
                .unwrap_or_else(|_| vec![]),
        ),
        verbose: config.get_or_default("http", "verbose").unwrap_or(false),
        http_proxy_host: config
            .get_or_default("http_proxy", "host")
            .unwrap_or_default(),
        http_no_proxy: config
            .get_or_default("http_proxy", "no")
            .unwrap_or_default(),
        ..Default::default()
    };

    if let Some(convert) = config.get_opt("http", "convert-cert").unwrap_or_default() {
        hc.convert_cert = convert;
    }

    if let Some(limit) = config.get_opt("http", "limit-requests").unwrap_or_default() {
        hc.limit_requests = limit;
    }

    if let Some(follow) = config
        .get_opt("http", "follow-redirects")
        .unwrap_or_default()
    {
        hc.follow_redirects = follow;
    }

    if let Some(threads) = config
        .get_opt("http", "dispatcher-threads")
        .unwrap_or_default()
    {
        hc.http_worker_threads = threads;
    }

    if let Some(length) = config
        .get_opt("http", "response-buffer-length")
        .unwrap_or_default()
    {
        hc.response_buffer_length = length;
    }

    let using_auth_proxy = hc.unix_socket_path.is_some()
        && url
            .domain()
            .is_some_and(|d| hc.unix_socket_domains.contains(d));

    if !using_auth_proxy {
        // If we aren't using auth proxy, we need to configure client certs.
        let auth = AuthSection::from_config(config);
        hc.auth_resolver = Some(AuthResolver::new(
            move || -> Result<_, auth::MissingCerts> {
                Ok(select_auth(&auth)?
                    .map(|auth| ResolvedAuth {
                        cert_path: auth.cert,
                        key_path: auth.key,
                        ca_path: auth.cacerts,
                    })
                    .unwrap_or_default())
            },
        ));
    }

    Ok(hc)
}

static INSECURE_MODE: AtomicBool = AtomicBool::new(false);

pub fn enable_insecure_mode() {
    INSECURE_MODE.store(true, Relaxed);
}

pub fn shutdown() {
    http_client::shutdown();
}

/// Initialize OpenSSL without registering its atexit cleanup. Must run before
/// anything else in the process touches OpenSSL, including Python's ssl and
/// hashlib modules, which share libcrypto with the HTTP client.
pub fn init_openssl() {
    http_client::init_openssl();
}

/// Setup progress reporting to the main progress registry for the lifetime of
/// this process.
pub fn enable_progress_reporting() {
    let _state = LazyLock::force(&PROGRESS_REPORTING_STATE);
}

/// State for progress reporting. Lazily initialized.
static PROGRESS_REPORTING_STATE: LazyLock<()> = LazyLock::new(|| {
    let trees_bar = AggregatingProgressBar::new("downloading", "bytes");
    let files_bar = AggregatingProgressBar::new("downloading", "bytes");
    let lfs_bar = AggregatingProgressBar::new("downloading", "bytes");

    Request::on_new_request(move |req| {
        // Register the HTTP I/O time series and start sampling on the first
        // request. This defers both the progress model registration and the
        // tokio runtime initialization to commands that actually make HTTP
        // requests.
        static FIRST_REQUEST: Once = Once::new();
        FIRST_REQUEST.call_once(|| {
            let net_time_series = IoTimeSeries::new("HTTP", "requests");
            Registry::main().register_io_time_series(&net_time_series);

            let take_sample = || {
                IoSample::from_io_bytes_count(
                    TOTAL.download_bytes.load(Relaxed) as _,
                    TOTAL.upload_bytes.load(Relaxed) as _,
                    TOTAL.request_count.load(Relaxed) as _,
                )
            };
            let task = net_time_series
                .async_sampling(take_sample, IoTimeSeries::default_sample_interval());
            async_runtime::spawn(task);
        });

        TOTAL.request_count.fetch_add(1, Relaxed);
        let req_listeners = req.ctx_mut().event_listeners();
        req_listeners.on_download_bytes({
            move |_req, n| {
                TOTAL.download_bytes.fetch_add(n, Relaxed);
            }
        });
        req_listeners.on_upload_bytes({
            move |_req, n| {
                TOTAL.upload_bytes.fetch_add(n, Relaxed);
            }
        });

        // TODO: How to tell whether it is downloading or uploading?

        // Consolidate /trees and /files requests into single progress bars.
        let mut url = req.ctx_mut().url().to_string();
        let mut is_single_bar = false;
        let bar = if url.ends_with("/trees") {
            trees_bar.create_or_extend_detached(0)
        } else if url.ends_with("/files") || url.ends_with("/files2") {
            files_bar.create_or_extend_detached(0)
        } else if let Some((prefix, _)) = url.split_once("/download/") {
            // Strip out the fetch key after /download/.
            url = format!("{prefix}/download/... (LFS)");
            lfs_bar.create_or_extend_detached(0)
        } else {
            is_single_bar = true;
            ProgressBar::new("downloading", 0, "bytes")
        };

        bar.set_message(url);

        let req_listeners = req.ctx_mut().event_listeners();
        req_listeners.on_content_length({
            let bar = bar.clone();
            move |_req, n| {
                bar.increase_total(n as _);
            }
        });
        req_listeners.on_download_bytes({
            let bar = bar.clone();
            move |_req, n| {
                bar.increase_position(n as _);
            }
        });
        if is_single_bar {
            req_listeners.on_first_activity(move |_req| {
                let registry = Registry::main();
                registry.register_progress_bar(&bar);
            });
        }
    });
});

fn bump_counters(client_id: &str, stats: &Stats) {
    let n = |suffix: &'static str| -> String { format!("http.{client_id}.{suffix}") };
    // TODO: gauges: rx_bytes and tx_bytes; histograms: request_time_ms, response_delay_ms
    increment_counter(n("total_rx_bytes"), stats.downloaded as u64);
    increment_counter(n("total_tx_bytes"), stats.uploaded as u64);
    increment_counter(n("num_requests"), stats.requests as u64);
    increment_counter(n("total_request_time_ms"), stats.time.as_millis() as u64);
    increment_counter(
        n("total_response_delay_ms"),
        stats.latency.as_millis() as u64,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::Relaxed;

    use super::*;

    static TEST_ID: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn test_convert_cert_config() {
        let mut hg_config = BTreeMap::<&str, &str>::new();

        let url: Url = "https://example.com".parse().unwrap();

        hg_config.insert("http.convert-cert", "True");
        assert!(http_config(&hg_config, &url).unwrap().convert_cert);

        hg_config.insert("http.convert-cert", "false");
        assert!(!http_config(&hg_config, &url).unwrap().convert_cert);
    }

    #[test]
    fn test_auth_resolution_retries_until_first_success() {
        let test_dir = std::env::temp_dir().join(format!(
            "hg-http-auth-resolver-{}-{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Relaxed)
        ));
        fs::create_dir(&test_dir).unwrap();
        let cert_path = test_dir.join("client.pem");

        let mut hg_config = BTreeMap::<String, String>::new();
        hg_config.insert("auth.main.prefix".into(), "example.com".into());
        hg_config.insert(
            "auth.main.cert".into(),
            cert_path.to_string_lossy().into_owned(),
        );

        let url: Url = "https://example.com".parse().unwrap();
        let resolver = http_config(&hg_config, &url)
            .unwrap()
            .auth_resolver
            .unwrap();

        assert!(resolver.resolve().is_err());

        fs::write(&cert_path, "certificate").unwrap();
        let expected = ResolvedAuth {
            cert_path: Some(cert_path.clone()),
            ..Default::default()
        };
        assert_eq!(resolver.resolve().unwrap(), &expected);

        fs::remove_file(&cert_path).unwrap();
        assert_eq!(resolver.resolve().unwrap(), &expected);
        fs::remove_dir(test_dir).unwrap();
    }
}
