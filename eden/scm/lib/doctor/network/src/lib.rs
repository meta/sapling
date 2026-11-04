/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::time::Duration;
use std::vec;

use auth::AuthSection;
use configmodel::Config;
use configmodel::ConfigExt;
use http_client::HttpClientError;
use http_client::curl;
use thiserror::Error;
use url::Host;
use url::Url;

#[derive(Debug, Error)]
pub enum HostError {
    #[error("DNS error: {0}")]
    DNS(io::Error),
    #[error("TCP error: {0}")]
    TCP(io::Error),
    #[error("invalid host config: {0}")]
    Config(String),
}

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("unexpected http response: {0:?}")]
    UnexpectedResponse(HttpResponse),
    #[error("http request failure: {0:?}")]
    RequestFailure(HttpClientError),
    #[error(transparent)]
    MissingCerts(#[from] auth::MissingCerts),
    #[error("{0}")]
    InvalidCert(auth::X509Error, HttpClientError),
    #[error("invalid http config: {0}")]
    Config(String),
}

#[derive(Debug, Error)]
pub enum Diagnosis {
    #[error("invalid config: {0}")]
    BadConfig(String),
    #[error("no internet connectivity: {0}")]
    NoInternet(HostError),
    #[error("no server connectivity: {0}")]
    NoServer(HostError),
    #[error("x2pagentd problem: {0}")]
    AuthProxyProblem(HttpError),
    #[error("http connection problem: {0}")]
    HttpProblem(HttpError),
}

pub struct HttpResponse {
    status: http::StatusCode,
    headers: http::HeaderMap,
    body: Vec<u8>,
}

impl fmt::Debug for HttpResponse {
    // Turn body into text based on content-type.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match header_value(&self.headers, "content-type") {
            Some("application/json") | Some("text/html") => {
                write!(
                    f,
                    "HttpResponse {{ status: {:?}, headers: {:?}, body: {:?} }}",
                    self.status,
                    self.headers,
                    String::from_utf8_lossy(&self.body)
                )
            }
            _ => {
                write!(
                    f,
                    "HttpResponse {{ status: {:?}, headers: {:?}, body: {:?} }}",
                    self.status, self.headers, self.body
                )
            }
        }
    }
}

impl Diagnosis {
    /// Whether the problem is the client's own certificate being missing or
    /// invalid, as opposed to connectivity or the server.
    pub fn is_cert_problem(&self) -> bool {
        match self {
            Self::HttpProblem(err) | Self::AuthProxyProblem(err) => {
                matches!(err, HttpError::MissingCerts(_) | HttpError::InvalidCert(..))
            }
            _ => false,
        }
    }

    pub fn treatment(&self, config: &dyn Config) -> String {
        match self {
            Self::NoServer(_) => {
                "Please check your VPN or proxy (internet okay, but can't reach server)."
                    .to_string()
            }
            Self::NoInternet(_) => {
                "Please check your internet connection (failed external connectivity test)."
                    .to_string()
            }
            Self::BadConfig(msg) => format!("Invalid config: {msg}"),
            Self::AuthProxyProblem(_) => {
                let mut msg = "Local auth proxy problem.".to_string();
                if let Some(help) = config.get("help", "auth-proxy-help") {
                    msg = format!("{} {}", msg, help.as_ref());
                }
                msg
            }
            Self::HttpProblem(err) => diagnose_http_error(config, err),
        }
    }

    fn short_name(&self) -> String {
        match self {
            Diagnosis::BadConfig(_) => "bad_config".to_string(),
            Diagnosis::NoInternet(_) => "no_internet".to_string(),
            Diagnosis::NoServer(_) => "no_corp".to_string(),
            Diagnosis::AuthProxyProblem(err) => format!("auth_proxy_problem({})", err.short_name()),
            Diagnosis::HttpProblem(err) => format!("http_problem({})", err.short_name()),
        }
    }
}

impl HttpError {
    fn short_name<'a>(&'a self) -> &'a str {
        match self {
            HttpError::UnexpectedResponse(res) => res.status.as_str(),
            HttpError::RequestFailure(_) => "other",
            HttpError::MissingCerts(_) => "missing_certs",
            HttpError::Config(_) => "config",
            HttpError::InvalidCert(_, _) => "invalid_cert",
        }
    }
}

fn diagnose_http_error(config: &dyn Config, err: &HttpError) -> String {
    let maybe_append_help = |mut msg: String, help_name: &str| -> String {
        if let Some(help) = config.get("help", help_name) {
            msg = format!("{}\n\n{}", msg, help.as_ref());
        }
        msg
    };

    match err {
        HttpError::UnexpectedResponse(res) => diagnose_unexpected_response(res),
        HttpError::RequestFailure(HttpClientError::Tls(err)) => maybe_append_help(
            // We weren't able to diagnose a particular cert problem,
            // so give a generic TLS message. Include the error since
            // it may have something more useful.
            format!("TLS error - please check your certificates.\n\n{err}"),
            "tlshelp",
        ),
        HttpError::InvalidCert(err, _) => maybe_append_help(format!("{err}"), "tlsauthhelp"),
        HttpError::MissingCerts(err) => maybe_append_help(format!("{err}"), "tlsauthhelp"),
        HttpError::RequestFailure(HttpClientError::Curl(err)) => diagnose_curl_error(err),

        HttpError::Config(err) => err.to_string(),
        HttpError::RequestFailure(_) => format!("{err}"),
    }
}

fn diagnose_curl_error(err: &curl::Error) -> String {
    if err.is_operation_timedout() {
        "Network timeout. Please check your connection.".to_string()
    } else {
        format!("{err}")
    }
}

fn diagnose_unexpected_response(res: &HttpResponse) -> String {
    match res.status {
        http::StatusCode::FORBIDDEN => {
            "You lack permission for this repo. Please see https://fburl.com/svnuser, and make sure you have permission for this repo.".to_string()
        }
        _ => {
            let mut header_hints = vec![];
            if let Some(x2p_error_type) = header_value(&res.headers, "x-x2pagentd-error-type") {
                let mut hint = format!("x2pagentd: {x2p_error_type}");
                if let Some(x2p_msg) = header_value(&res.headers, "x-x2pagentd-error-msg") {
                    hint = format!("{hint} ({x2p_msg})");
                }
                header_hints.push(hint);
            }

            for (n, v) in res.headers.iter() {
                if let Some(advice_type) = n.as_str().strip_prefix("x-fb-validated-x2pauth-advice-")
                {
                    if let Ok(v) = v.to_str() {
                        header_hints.push(format!("x2p auth {advice_type}: {v}"));
                    }
                }
            }

            format!(
                "Unexpected server response: {}{}",
                res.status,
                header_hints
                    .iter()
                    .map(|h| format!("\n\t{h}"))
                    .collect::<Vec<String>>()
                    .join(""),
            )
        }
    }
}

fn header_value<'a>(h: &'a http::HeaderMap, key: &str) -> Option<&'a str> {
    h.get(key).and_then(|val| val.to_str().ok())
}

pub struct Doctor {
    // Allow tests to stub DNS responses.
    dns_lookup: Box<dyn Fn(&str) -> io::Result<vec::IntoIter<SocketAddr>>>,
    tcp_connect_timeout: Duration,
    stub_healthcheck_response:
        Option<Box<dyn Fn(&Url, bool) -> Result<HttpResponse, HttpClientError>>>,
}

fn real_dns_lookup(host_port: &str) -> io::Result<vec::IntoIter<SocketAddr>> {
    host_port.to_socket_addrs()
}

impl Doctor {
    pub fn new() -> Self {
        Doctor {
            dns_lookup: Box::new(real_dns_lookup),
            tcp_connect_timeout: Duration::from_secs(1),
            stub_healthcheck_response: None,
        }
    }

    fn check_host_tcp(&self, url: &Url) -> Result<(), HostError> {
        let port = url.port().unwrap_or(443);

        let sock_addrs: Vec<SocketAddr> = match url.host() {
            None => {
                return Err(HostError::Config(format!("url {url} has no host")));
            }
            Some(Host::Domain(host)) => {
                // Let tests stub out host check results.
                if host == "test_fail" {
                    return Err(HostError::TCP(io::Error::other("test")));
                } else if host == "test_succeed" {
                    return Ok(());
                }

                match (self.dns_lookup)(&format!("{host}:{port}")) {
                    Err(err) => return Err(HostError::DNS(err)),
                    Ok(addrs) => addrs.collect(),
                }
            }
            Some(Host::Ipv4(ip)) => vec![SocketAddr::new(ip.into(), port)],
            Some(Host::Ipv6(ip)) => vec![SocketAddr::new(ip.into(), port)],
        };

        if sock_addrs.is_empty() {
            return Err(HostError::DNS(io::Error::other(format!(
                "{:?} resolved to 0 IPs",
                url.host()
            ))));
        }

        for i in 0..sock_addrs.len() {
            match TcpStream::connect_timeout(&sock_addrs[i], self.tcp_connect_timeout) {
                Err(err) => {
                    if i == sock_addrs.len() - 1 {
                        return Err(HostError::TCP(err));
                    }
                }
                Ok(_) => break,
            }
        }

        Ok(())
    }

    pub fn diagnose(&self, config: &dyn Config) -> Result<(), Diagnosis> {
        let res = || -> Result<(), Diagnosis> {
            let host_error = self.check_server_connectivity(config);

            let http_error = self.check_http_connectivity(config);
            if matches!(
                http_error,
                Err(Diagnosis::HttpProblem(
                    HttpError::UnexpectedResponse(_)
                        | HttpError::MissingCerts(_)
                        | HttpError::InvalidCert(_, _)
                ))
            ) {
                return http_error;
            }

            host_error.and(http_error)
        }();

        let short_res = match &res {
            Err(err) => err.short_name(),
            Ok(()) => "ok".to_string(),
        };
        tracing::debug!(target: "network_doctor_diagnosis", network_doctor_diagnosis=&short_res[..]);

        res
    }

    fn check_server_connectivity(&self, config: &dyn Config) -> Result<(), Diagnosis> {
        let repo_url = config_url(config, "edenapi", "url", None)?;

        // First check for server connectivity.
        let server_err = match self.check_host_tcp(&repo_url) {
            Ok(()) => return Ok(()),
            Err(err) => err,
        };

        // If we don't have server connectivity, see if we have internet connectivity.
        let external_url = config_url(
            config,
            "doctor",
            "external-host-check-url",
            Some("https://www.facebook.com"),
        )?;

        match self.check_host_tcp(&external_url) {
            Ok(()) => Err(Diagnosis::NoServer(server_err)),
            Err(err) => Err(Diagnosis::NoInternet(err)),
        }
    }

    fn check_http_connectivity(&self, config: &dyn Config) -> Result<(), Diagnosis> {
        let mut url = config_url(config, "edenapi", "url", None)?;
        let repo_name = match config
            .get("remotefilelog", "reponame")
            // Try a default reponame so network doctor works outside a repo context.
            .or_else(|| config.get("network-doctor", "default-reponame"))
        {
            Some(name) => name.to_string(),
            None => {
                return Err(Diagnosis::BadConfig(
                    "remotefilelog.reponame is not set".to_string(),
                ));
            }
        };

        // Build edenpi/:repo/capabilities URL. This will suss out permission
        // errors (as opposed to :repo/health_check).
        match url.path_segments_mut() {
            Ok(mut path) => path.pop_if_empty().push(&repo_name).push("capabilities"),
            Err(()) => return Err(Diagnosis::BadConfig("bad edenapi.url".into())),
        };

        let mut x2pagentd_err = None;
        if use_x2pagentd(config, &url) {
            x2pagentd_err = match self.check_host_http(config, &url, true) {
                Ok(()) => return Ok(()),
                Err(err) => Some(err),
            }
        }

        match (self.check_host_http(config, &url, false), x2pagentd_err) {
            (Ok(()), Some(err)) => Err(Diagnosis::AuthProxyProblem(err)),
            (Ok(()), None) => Ok(()),
            (Err(_), Some(proxy_err)) => Err(Diagnosis::HttpProblem(proxy_err)),
            (Err(err), None) => Err(Diagnosis::HttpProblem(err)),
        }
    }

    fn check_host_http(
        &self,
        config: &dyn Config,
        url: &Url,
        use_x2pagentd: bool,
    ) -> Result<(), HttpError> {
        tracing::debug!(%url, use_x2pagentd, "check_host_http");

        let mut hc = hg_http::http_config(config, url)?;

        if !use_x2pagentd {
            hc.unix_socket_path = None;

            let auth = AuthSection::from_config(config).best_match_for(url)?;
            (hc.cert_path, hc.key_path, hc.ca_path) = auth
                .map(|auth| (auth.cert, auth.key, auth.cacerts))
                .unwrap_or_default();

            if url.scheme() == "https" && hc.cert_path.is_none() {
                return Err(HttpError::Config(format!("no auth section for {url}")));
            }
        }

        let result = if let Some(stub) = &self.stub_healthcheck_response {
            stub(url, use_x2pagentd)
        } else {
            let mut req = hg_http::http_client("network-doctor", hc.clone()).get(url.clone());
            req.set_timeout(Duration::from_secs(3));
            req.send().map(|res| HttpResponse {
                status: res.status(),
                headers: res.headers().clone(),
                body: res.body().to_vec(),
            })
        };

        match result {
            Ok(res) if res.status.is_success() => Ok(()),
            Ok(res) => Err(HttpError::UnexpectedResponse(res)),
            Err(err) => {
                if let HttpClientError::Tls(_) = err {
                    if let Some(cert_path) = &hc.cert_path {
                        if let Err(x509_err) = auth::check_certs(cert_path) {
                            return Err(HttpError::InvalidCert(x509_err, err));
                        }
                    }
                }

                Err(HttpError::RequestFailure(err))
            }
        }
    }
}

fn use_x2pagentd(config: &dyn Config, url: &Url) -> bool {
    let hc = match hg_http::http_config(config, url) {
        Ok(hc) => hc,
        Err(_) => return false,
    };
    if hc.unix_socket_path.is_none() {
        return false;
    }

    match url.domain() {
        None => false,
        Some(domain) => hc.unix_socket_domains.contains(domain),
    }
}

// Extract and parse URL from config with optional default if no config value.
fn config_url(
    config: &dyn Config,
    section: &str,
    field: &str,
    default: Option<&str>,
) -> Result<Url, Diagnosis> {
    let url: String = match (config.get_nonempty_opt(section, field), default) {
        (Err(err), _) => return Err(Diagnosis::BadConfig(format!("config error: {err}"))),
        (Ok(Some(url)), _) => url,
        (Ok(None), Some(default)) => default.to_string(),
        (Ok(None), None) => {
            return Err(Diagnosis::BadConfig(format!(
                "no config for {section}.{field}"
            )));
        }
    };

    match Url::parse(&url) {
        Err(err) => Err(Diagnosis::BadConfig(format!("invalid url {url}: {err}"))),
        Ok(url) => Ok(url),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::net::TcpListener;

    use http::StatusCode;
    use tempfile::tempdir;

    use super::*;

    macro_rules! response {
        ($status:expr $(, $name:tt : $val:tt)* $(,)?) => {
            response!($status, b"", $($name : $val,)*)
        };
        ($status:expr, $body:expr $(, $name:tt : $val:tt)* $(,)?) => {
            HttpResponse {
                status: $status,
                headers: (&HashMap::<String, String>::from([$(($name.to_string(), $val.to_string()),)*])).try_into().unwrap(),
                body: $body.to_vec(),
            }
        };
    }

    #[test]
    fn test_check_host() {
        let mut doc = Doctor::new();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // No DNS lookup required - can connect directly to IP.
        {
            doc.dns_lookup = Box::new(move |_host_port| -> io::Result<vec::IntoIter<SocketAddr>> {
                panic!("dns lookup!")
            });

            let url = Url::parse(&format!("https://{}:{}", addr.ip(), addr.port())).unwrap();
            assert!(doc.check_host_tcp(&url).is_ok());
        }

        // DNS lookup is okay.
        {
            doc.dns_lookup = Box::new(move |_host_port| -> io::Result<vec::IntoIter<SocketAddr>> {
                Ok(vec![addr].into_iter())
            });

            let url = Url::parse(&format!("https://localhost:{}", addr.port())).unwrap();
            assert!(doc.check_host_tcp(&url).is_ok());
        }

        // DNS lookup fails.
        {
            doc.dns_lookup = Box::new(move |_host_port| -> io::Result<vec::IntoIter<SocketAddr>> {
                Err(io::Error::other("some sort of DNS failure!"))
            });

            let url = Url::parse(&format!("https://localhost:{}", addr.port())).unwrap();
            assert!(matches!(doc.check_host_tcp(&url), Err(HostError::DNS(_))));
        }

        // No one is listening.
        {
            doc.tcp_connect_timeout = Duration::from_millis(1);

            let url = Url::parse("https://169.254.0.1:1234").unwrap();
            assert!(matches!(doc.check_host_tcp(&url), Err(HostError::TCP(_))))
        }
    }

    #[test]
    fn test_check_server_connectivity() {
        let non_working_url = "https://169.254.0.1:1234";

        // Both server and external fail.
        {
            let mut cfg = BTreeMap::new();
            cfg.insert("edenapi.url", non_working_url);
            cfg.insert("doctor.external-host-check-url", non_working_url);

            let mut doc = Doctor::new();
            doc.tcp_connect_timeout = Duration::from_millis(1);

            assert!(matches!(
                doc.check_server_connectivity(&cfg),
                Err(Diagnosis::NoInternet(_))
            ));
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let listener_addr = listener.local_addr().unwrap();
        let working_url = format!("https://{}:{}", listener_addr.ip(), listener_addr.port());

        // External works.
        {
            let mut cfg = BTreeMap::new();
            cfg.insert("edenapi.url", non_working_url);
            cfg.insert("doctor.external-host-check-url", &working_url);

            let doc = Doctor::new();
            assert!(matches!(
                doc.check_server_connectivity(&cfg),
                Err(Diagnosis::NoServer(_))
            ));
        }

        // Server works.
        {
            let mut cfg: BTreeMap<&str, &str> = BTreeMap::new();
            cfg.insert("edenapi.url", &working_url);
            cfg.insert("doctor.external-host-check-url", &working_url);

            let doc = Doctor::new();
            assert!(matches!(doc.check_server_connectivity(&cfg), Ok(())));
        }
    }

    #[test]
    fn test_check_http_connectivity() {
        let td = tempdir().unwrap();

        let mut doc = Doctor::new();

        let fake_cert_path = td.path().join("cert");
        std::fs::write(&fake_cert_path, "foo").unwrap();

        let mut cfg = BTreeMap::new();
        cfg.insert("edenapi.url", "https://example.com/edenapi/");
        cfg.insert("remotefilelog.reponame", "some_repo");
        cfg.insert("auth.test.prefix", "*");
        cfg.insert("auth.test.cert", fake_cert_path.to_str().unwrap());
        cfg.insert("auth.test.key", fake_cert_path.to_str().unwrap());

        // Happy path - server returns 200 for /capabilities.
        {
            doc.stub_healthcheck_response = Some(Box::new(|url, _x2p| {
                assert_eq!(url.path(), "/edenapi/some_repo/capabilities");
                Ok(response!(http::StatusCode::OK))
            }));
            assert!(matches!(doc.check_http_connectivity(&cfg), Ok(())));
        }

        // Unexpected HTTP response.
        {
            doc.stub_healthcheck_response = Some(Box::new(|_url, _x2p| {
                Ok(response!(http::StatusCode::FORBIDDEN))
            }));
            assert!(matches!(
                doc.check_http_connectivity(&cfg),
                Err(Diagnosis::HttpProblem(HttpError::UnexpectedResponse(
                    HttpResponse {
                        status: http::StatusCode::FORBIDDEN,
                        ..
                    }
                )))
            ));
        }

        // Simulate x2pagentd specific problem.
        {
            let mut cfg = cfg.clone();

            cfg.insert("auth_proxy.unix_socket_path", "/dev/null");
            cfg.insert("auth_proxy.unix_socket_domains", "example.com");

            // Simulate x2pagentd specific problem.
            doc.stub_healthcheck_response = Some(Box::new(|_url, x2p| {
                if x2p {
                    Ok(response!(http::StatusCode::INTERNAL_SERVER_ERROR))
                } else {
                    Ok(response!(http::StatusCode::OK))
                }
            }));
            assert!(matches!(
                doc.check_http_connectivity(&cfg),
                Err(Diagnosis::AuthProxyProblem(HttpError::UnexpectedResponse(
                    HttpResponse {
                        status: http::StatusCode::INTERNAL_SERVER_ERROR,
                        ..
                    }
                )))
            ));
        }

        // Make sure we support non-https.
        {
            let mut cfg = cfg.clone();

            cfg.insert("edenapi.url", "http://example.com/edenapi/");

            cfg.insert("auth.test.prefix", "doesnt_match");

            doc.stub_healthcheck_response =
                Some(Box::new(|_url, _x2p| Ok(response!(http::StatusCode::OK))));
            assert!(matches!(doc.check_http_connectivity(&cfg), Ok(())));
        }

        // Give a specific error for missing auth section.
        {
            let mut cfg = cfg.clone();

            cfg.insert("auth.test.prefix", "doesnt_match");

            doc.stub_healthcheck_response =
                Some(Box::new(|_url, _x2p| Ok(response!(http::StatusCode::OK))));

            match doc.check_http_connectivity(&cfg) {
                Err(Diagnosis::HttpProblem(HttpError::Config(msg))) => {
                    assert_eq!(
                        msg,
                        "no auth section for https://example.com/edenapi/some_repo/capabilities"
                    );
                }
                _ => panic!(),
            };
        }
    }

    #[test]
    fn test_diagnose_unexpected_response() {
        assert_eq!(
            diagnose_unexpected_response(&response!(
                StatusCode::INTERNAL_SERVER_ERROR,
                "x-x2pagentd-error-type": "apple",
                "x-x2pagentd-error-msg": "not crispy",
            )),
            "Unexpected server response: 500 Internal Server Error\n\tx2pagentd: apple (not crispy)"
        );

        assert_eq!(
            diagnose_unexpected_response(&response!(
                StatusCode::INTERNAL_SERVER_ERROR,
                "x-fb-validated-x2pauth-advice-access-denied": "reboot laptop",
            )),
            "Unexpected server response: 500 Internal Server Error\n\tx2p auth access-denied: reboot laptop"
        );

        assert_eq!(
            diagnose_unexpected_response(&response!(StatusCode::FORBIDDEN)),
            "You lack permission for this repo. Please see https://fburl.com/svnuser, and make sure you have permission for this repo.",
        );
    }

    #[test]
    fn test_response_body_format() {
        assert_eq!(
            format!(
                "{:?}",
                response!(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    b"some body",
                    "content-type": "application/octet-stream",
                )
            ),
            "HttpResponse { status: 500, headers: {\"content-type\": \"application/octet-stream\"}, body: [115, 111, 109, 101, 32, 98, 111, 100, 121] }"
        );

        assert_eq!(
            format!(
                "{:?}",
                response!(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    b"{}",
                    "content-type": "application/json",
                )
            ),
            "HttpResponse { status: 500, headers: {\"content-type\": \"application/json\"}, body: \"{}\" }"
        );
    }
}
