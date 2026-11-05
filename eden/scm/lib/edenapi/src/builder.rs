/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Context;
use anyhow::Error;
use anyhow::anyhow;
use cats::CatsSection;
use configmodel::ConfigExt;
use configmodel::convert::FromConfigValue;
use http_client::Encoding;
use http_client::HttpVersion;
use http_client::MinTransferSpeed;
use parking_lot::RwLock;
use repourl::RepoUrl;
use url::Url;

use crate::SaplingRemoteApi;
use crate::client::Client;
use crate::errors::ConfigError;
use crate::errors::SaplingRemoteApiError;

/// HTTP header used to pin requests to a specific Mononoke host behind
/// Proxygen via a `server_selection_type=Direct` rule. Its value is a
/// "host:port" that the Proxygen rule reads to route to that exact backend.
const DIRECT_HOST_HEADER: &str = "x-mononoke-direct-host";

/// External function that constructs other kinds of `SaplingRemoteApi` from config.
static CUSTOM_BUILD_FUNCS: LazyLock<
    RwLock<
        Vec<
            Box<
                dyn (Fn(
                        &dyn configmodel::Config,
                    )
                        -> Result<Option<Arc<dyn SaplingRemoteApi>>, SaplingRemoteApiError>)
                    + Send
                    + Sync
                    + 'static,
            >,
        >,
    >,
> = LazyLock::new(Default::default);

/// Builder for creating new SaplingRemoteAPI clients.
pub struct Builder<'a> {
    config: &'a dyn configmodel::Config,
    repo_name: Option<String>,
    server_url: Option<Url>,
    path_server_url: Option<Url>,
}

impl<'a> Builder<'a> {
    /// Populate a `Builder` from a Mercurial configuration.
    pub fn from_config(config: &'a dyn configmodel::Config) -> Result<Self, SaplingRemoteApiError> {
        let builder = Self {
            config,
            repo_name: None,
            server_url: None,
            path_server_url: None,
        };
        Ok(builder)
    }

    /// Use the repo's remote path for the server URL when neither `server_url` nor
    /// `edenapi.url` sets it.
    ///
    /// A `mononoke://host[:port]/<repo>` path names `https://host[:port]/slapi/`, unless
    /// `experimental.slapi-url-from-path` is false. Other paths name no server. The repo
    /// name comes from `remotefilelog.reponame`, which Sapling sets from `paths.default`.
    pub fn repo_url(mut self, url: &RepoUrl) -> Result<Self, ConfigError> {
        self.path_server_url = url
            .slapi_server_url()
            .map_err(|e| ConfigError::Invalid("paths.default".into(), e))?;
        Ok(self)
    }

    /// Configure repo name for client. This is only used by the Http Client.
    pub fn repo_name(mut self, repo_name: Option<impl ToString>) -> Self {
        self.repo_name = repo_name.map(|s| s.to_string());
        self
    }

    /// Configure server URL for client. This is only used by the Http Client.
    pub fn server_url(mut self, url: Option<Url>) -> Self {
        self.server_url = url;
        self
    }

    /// Whether an HTTP client built now would have both a server URL and a repo name.
    pub fn has_server_url_and_repo_name(&self) -> Result<bool, ConfigError> {
        Ok(self.resolved_server_url()?.is_some()
            && (self.repo_name.is_some()
                || self
                    .config
                    .get_nonempty("remotefilelog", "reponame")
                    .is_some()))
    }

    /// The server URL the client uses: `server_url`, else `edenapi.url`, else the remote
    /// path's (see `repo_url`).
    fn resolved_server_url(&self) -> Result<Option<Url>, ConfigError> {
        if let Some(url) = &self.server_url {
            return Ok(Some(url.clone()));
        }
        if let Some(url) = self.config.get_nonempty("edenapi", "url") {
            let url = url
                .parse::<Url>()
                .map_err(|e| ConfigError::Invalid("edenapi.url".into(), e.into()))?;
            return Ok(Some(url));
        }
        if get_config::<bool>(self.config, "experimental", "slapi-url-from-path")?.unwrap_or(true) {
            return Ok(self.path_server_url.clone());
        }
        Ok(None)
    }

    /// Build the client.
    pub fn build(self) -> Result<Arc<dyn SaplingRemoteApi>, SaplingRemoteApiError> {
        {
            // Hook in other SaplingRemoteAPI implementations such as eagerepo (used for tests).
            let funcs = CUSTOM_BUILD_FUNCS.read();
            for func in funcs.iter() {
                if let Some(client) = func(self.config)? {
                    return Ok(client);
                }
            }
        }
        let mut builder = match self.resolved_server_url()? {
            Some(server_url) => HttpClientBuilder::from_config_with_url(self.config, server_url)?,
            None => HttpClientBuilder::from_config(self.config)?,
        };

        if let Some(repo_name) = &self.repo_name {
            builder = builder.repo_name(repo_name);
        }

        Ok(Arc::new(builder.build()?))
    }

    /// Register a customized builder that can produce a non-HTTP `SaplingRemoteApi` from config.
    pub fn register_customize_build_func<F>(func: F)
    where
        F: (Fn(
                &dyn configmodel::Config,
            ) -> Result<Option<Arc<dyn SaplingRemoteApi>>, SaplingRemoteApiError>)
            + Send
            + Sync
            + 'static,
        F: Copy,
    {
        tracing::debug!(
            "registered {} to edenapi Builder",
            std::any::type_name::<F>()
        );
        CUSTOM_BUILD_FUNCS.write().push(Box::new(func))
    }
}

/// Builder for creating new HTTP SaplingRemoteAPI clients.
///
/// You probably want to use [`Builder`] instead.
#[derive(Debug, Default)]
pub struct HttpClientBuilder {
    repo_name: Option<String>,
    server_url: Option<Url>,
    headers: HashMap<String, String>,
    try_route_consistently: bool,
    disable_sequential_tree_uploads: bool,
    augmented_trees: bool,
    max_commit_data_per_batch: Option<usize>,
    max_files_per_batch: Option<usize>,
    max_trees_per_batch: Option<usize>,
    max_history_per_batch: Option<usize>,
    max_path_history_per_batch: Option<usize>,
    max_location_to_hash_per_batch: Option<usize>,
    max_commit_mutations_per_batch: Option<usize>,
    max_commit_translate_id_per_batch: Option<usize>,
    min_batch_size: Option<usize>,
    timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
    handler_timeouts: HashMap<String, Duration>,
    debug: bool,
    http_version: Option<HttpVersion>,
    log_dir: Option<PathBuf>,
    encoding: Option<Encoding>,
    min_transfer_speed: Option<MinTransferSpeed>,
    handler_min_transfer_speeds: HashMap<String, MinTransferSpeed>,
    max_retry_per_request: usize,
    stream_retries: Option<bool>,
    http_config: http_client::Config,
}

impl HttpClientBuilder {
    pub fn new() -> Self {
        Default::default()
    }

    /// Build the HTTP client.
    pub fn build(self) -> Result<Client, SaplingRemoteApiError> {
        self.try_into().map(Client::with_config)
    }

    pub fn from_config_with_url(
        config: &dyn configmodel::Config,
        server_url: Url,
    ) -> Result<Self, SaplingRemoteApiError> {
        // XXX: Ideally, the repo name would be a required field, obtained from a `Repo` object from
        // the `clidispatch` crate. Unfortunately, not all callsites presently have access to a
        // populated `Repo` object, and it isn't trivial to just initialize one (requires a path to
        // the on-disk repo) or to plumb one through (which might not be possible for usage outside
        // of a Mercurial process, such as by EdenFS). For now, let's just allow setting the
        // reponame later via `repo_name` method.
        let mut repo_name = get_config::<String>(config, "remotefilelog", "reponame")?;
        if repo_name.as_deref() == Some("") {
            repo_name = None;
        }

        let mut headers = get_config::<String>(config, "edenapi", "headers")?
            .map(parse_headers)
            .transpose()
            .map_err(|e| ConfigError::Invalid("edenapi.headers".into(), e))?
            .unwrap_or_default();

        // Pin requests to a specific Mononoke host behind Proxygen. The value is
        // a "host:port" that a Proxygen `server_selection_type=Direct` rule reads
        // off this header to route the request to that exact backend.
        if let Some(direct_host) = get_config::<String>(config, "mononoke", "direct-host")? {
            if !direct_host.is_empty() {
                headers.insert(DIRECT_HOST_HEADER.to_string(), direct_host);
            }
        }

        let source = if std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.contains("edenfs"))
            })
            .unwrap_or_default()
        {
            "EdenFs"
        } else {
            "Sapling"
        };

        headers.insert(
            "User-Agent".to_string(),
            format!("{}/{}", source, version::VERSION),
        );

        let cats_section = CatsSection::from_config(&config, "cats");

        if let Ok(Some(cats)) = cats_section.get_cats_by_type(cats::CatTokenType::Forwarded) {
            headers.insert(cats_constants::X_FORWARDED_CATS_HEADER.to_string(), cats);
        }

        if let Ok(Some(auth_cats)) = cats_section.get_cats_by_type(cats::CatTokenType::Auth) {
            headers.insert(cats_constants::X_AUTH_CATS_HEADER.to_string(), auth_cats);
        }

        if config
            .get_or_default::<bool>("slacl", "server-enforcement")
            .unwrap_or(false)
        {
            headers.insert("X-Enforce-Path-Acls".to_string(), "true".to_string());
        }

        let max_requests = get_config(config, "edenapi", "max-concurrent-requests")?;

        let max_requests_per_batch =
            get_config(config, "edenapi", "max-concurrent-requests-per-batch")?;

        let try_route_consistently =
            get_config(config, "edenapi", "try-route-consistently")?.unwrap_or_default();

        let disable_sequential_tree_uploads =
            get_config(config, "edenapi", "disable-sequential-tree-uploads")?.unwrap_or_default();

        let augmented_trees = get_config(config, "edenapi", "augmented-trees")?.unwrap_or_default();

        let min_batch_size = get_config(config, "edenapi", "min-batch-size")?;
        let max_commit_data_per_batch = get_config(config, "edenapi", "maxcommitdata")?;
        let max_files_per_batch = get_config(config, "edenapi", "maxfiles")?;
        let max_trees_per_batch = get_config(config, "edenapi", "maxtrees")?;
        let max_history_per_batch = get_config(config, "edenapi", "maxhistory")?;
        let max_path_history_per_batch = get_config(config, "edenapi", "maxpathhistory")?;
        let max_location_to_hash_per_batch = get_config(config, "edenapi", "maxlocationtohash")?;
        let max_commit_mutations_per_batch = get_config(config, "edenapi", "maxcommitmutations")?;
        let max_commit_translate_id_per_batch =
            get_config(config, "edenapi", "maxcommittranslateid")?;

        let timeout: Option<Duration> = get_config(config, "edenapi", "timeout")?;
        let connect_timeout: Option<Duration> = get_config(config, "edenapi", "connect-timeout")?;

        let handler_timeouts: HashMap<String, Duration> = config
            .keys_prefixed("edenapi", "timeout.")
            .into_iter()
            .filter_map(
                |key| match get_config::<Duration>(config, "edenapi", &key) {
                    Err(err) => Some(Err(err)),
                    Ok(Some(value)) => Some(Ok((key.strip_prefix("timeout.")?.to_string(), value))),
                    Ok(None) => None,
                },
            )
            .collect::<Result<_, _>>()?;

        let debug = get_config(config, "edenapi", "debug")?.unwrap_or_default();
        let http_version =
            get_config(config, "edenapi", "http-version")?.unwrap_or_else(|| "2".to_string());
        let http_version = Some(match http_version.as_str() {
            "1.1" => HttpVersion::V11,
            "2" => HttpVersion::V2,
            x => {
                return Err(SaplingRemoteApiError::BadConfig(ConfigError::Invalid(
                    "edenapi.http-version".into(),
                    anyhow!("invalid http version {x}"),
                )));
            }
        });
        let log_dir = get_config(config, "edenapi", "logdir")?;
        let encoding =
            get_config::<String>(config, "edenapi", "encoding")?.map(|s| Encoding::from(&*s));

        let low_speed_window: Duration = match get_config(config, "edenapi", "low-speed-window")? {
            Some(window) => window,
            None => {
                get_config(config, "edenapi", "low-speed-grace-period-seconds")?.unwrap_or_default()
            }
        };
        let min_transfer_speed =
            get_config::<u32>(config, "edenapi", "low-speed-min-bytes-per-second")?.map(
                |min_bytes_per_second| MinTransferSpeed {
                    min_bytes_per_second,
                    window: low_speed_window,
                },
            );

        let handler_min_transfer_speeds: HashMap<String, MinTransferSpeed> = config
            .keys_prefixed("edenapi", "low-speed-min-bytes-per-second.")
            .into_iter()
            .filter_map(|key| {
                let handler = key.strip_prefix("low-speed-min-bytes-per-second.")?;
                match get_config(config, "edenapi", &key) {
                    Err(err) => Some(Err(err)),
                    Ok(Some(value)) => Some(Ok((
                        handler.to_string(),
                        MinTransferSpeed {
                            min_bytes_per_second: value,
                            window: match get_config(
                                config,
                                "edenapi",
                                &format!("low-speed-window.{handler}"),
                            ) {
                                Err(err) => return Some(Err(err)),
                                Ok(Some(window)) => window,
                                Ok(None) => Duration::default(),
                            },
                        },
                    ))),
                    Ok(None) => None,
                }
            })
            .collect::<Result<_, _>>()?;

        let max_retry_per_request =
            get_config::<usize>(config, "edenapi", "max-retry-per-request")?.unwrap_or(3);

        let stream_retries = get_config(config, "experimental", "slapi-stream-retries")?;

        let max_concurrent_streams =
            get_config::<usize>(config, "edenapi", "max-concurrent-streams")?;

        let mut http_config = hg_http::http_config(config, &server_url)?;
        http_config.verbose_stats |= debug;
        http_config.max_concurrent_requests = max_requests;
        http_config.max_concurrent_requests_per_batch = max_requests_per_batch;
        http_config.max_concurrent_streams = max_concurrent_streams;

        let builder = HttpClientBuilder {
            repo_name,
            server_url: Some(server_url),
            headers,
            try_route_consistently,
            disable_sequential_tree_uploads,
            augmented_trees,
            max_commit_data_per_batch,
            max_files_per_batch,
            max_trees_per_batch,
            max_history_per_batch,
            max_path_history_per_batch,
            max_location_to_hash_per_batch,
            max_commit_mutations_per_batch,
            max_commit_translate_id_per_batch,
            min_batch_size,
            timeout,
            connect_timeout,
            handler_timeouts,
            debug,
            http_version,
            log_dir,
            encoding,
            min_transfer_speed,
            handler_min_transfer_speeds,
            max_retry_per_request,
            stream_retries,
            http_config,
        };

        tracing::debug!(?builder);
        Ok(builder)
    }

    /// Populate a `HttpClientBuilder` from a Mercurial configuration.
    pub fn from_config(config: &dyn configmodel::Config) -> Result<Self, SaplingRemoteApiError> {
        let server_url = get_required_config::<String>(config, "edenapi", "url")?
            .parse::<Url>()
            .map_err(|e| ConfigError::Invalid("edenapi.url".into(), e.into()))?;
        Self::from_config_with_url(config, server_url)
    }

    /// Set the repo name.
    pub fn repo_name(mut self, repo_name: &str) -> Self {
        self.repo_name = Some(repo_name.into());
        self
    }

    /// Set the server URL.
    pub fn server_url(mut self, url: Url) -> Self {
        self.server_url = Some(url);
        self
    }

    /// Extra HTTP headers that should be sent with each request.
    pub fn headers<T, K, V>(mut self, headers: T) -> Self
    where
        T: IntoIterator<Item = (K, V)>,
        K: ToString,
        V: ToString,
    {
        let headers = headers
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()));
        self.headers.extend(headers);
        self
    }

    /// Add an extra HTTP header that should be sent with each request.
    pub fn header(mut self, name: impl ToString, value: impl ToString) -> Self {
        self.headers.insert(name.to_string(), value.to_string());
        self
    }

    /// Maximum number of concurrent HTTP requests allowed.
    pub fn max_requests(mut self, size: Option<usize>) -> Self {
        self.http_config.max_concurrent_requests = size;
        self
    }

    /// Maximum number of keys per commit data request. Larger requests will be
    /// split up into concurrently-sent batches.
    pub fn max_commit_data_per_batch(mut self, size: Option<usize>) -> Self {
        self.max_commit_data_per_batch = size;
        self
    }

    /// Maximum number of keys per file request. Larger requests will be
    /// split up into concurrently-sent batches.
    pub fn max_files_per_batch(mut self, size: Option<usize>) -> Self {
        self.max_files_per_batch = size;
        self
    }

    /// Maximum number of keys per tree request. Larger requests will be
    /// split up into concurrently-sent batches.
    pub fn max_trees_per_batch(mut self, size: Option<usize>) -> Self {
        self.max_trees_per_batch = size;
        self
    }

    /// Maximum number of keys per history request. Larger requests will be
    /// split up into concurrently-sent batches.
    pub fn max_history_per_batch(mut self, size: Option<usize>) -> Self {
        self.max_history_per_batch = size;
        self
    }

    /// Maximum number of paths per path_history request. Larger requests will be
    /// split up into concurrently-sent batches.
    pub fn max_path_history_per_batch(mut self, size: Option<usize>) -> Self {
        self.max_path_history_per_batch = size;
        self
    }

    /// Maximum number of locations per location to has request. Larger requests will be split up
    /// into concurrently-sent batches.
    pub fn max_location_to_hash_per_batch(mut self, size: Option<usize>) -> Self {
        self.max_location_to_hash_per_batch = size;
        self
    }

    /// Maximum number of retries per request.
    pub fn max_retry_per_request(mut self, max: usize) -> Self {
        self.max_retry_per_request = max;
        self
    }

    /// Enable streamed retries for history, queries, uploads, and streaming clone.
    /// Defaults to true; also set by `experimental.slapi-stream-retries`.
    pub fn stream_retries(mut self, enabled: bool) -> Self {
        self.stream_retries = Some(enabled);
        self
    }

    /// Timeout for HTTP requests sent by the client.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Set the HTTP version that the client should use.
    pub fn http_version(mut self, version: HttpVersion) -> Self {
        self.http_version = Some(version);
        self
    }

    /// If specified, the client will write a JSON version of every request
    /// it sends to the specified directory. This is primarily useful for
    /// debugging.
    pub fn log_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.log_dir = Some(dir.as_ref().into());
        self
    }

    /// If enabled, convert the user's client certificate from PEM to PKCS#12
    /// prior to use. This is required on platforms that do not natively support
    /// PEM certificates, such as Windows.
    pub fn convert_cert(mut self, enable: bool) -> Self {
        self.http_config.convert_cert = enable;
        self
    }

    /// Specify settings for the underlying HTTP client for non-Sapling
    /// edenapi clients.
    pub fn http_config(mut self, http_config: http_client::Config) -> Self {
        self.http_config = http_config;
        self
    }
}

fn get_config<T: FromConfigValue>(
    config: &dyn configmodel::Config,
    section: &str,
    name: &str,
) -> Result<Option<T>, ConfigError> {
    config
        .get_opt::<T>(section, name)
        .map_err(|e| ConfigError::Invalid(format!("{section}.{name}"), e.into()))
}

fn get_required_config<T: FromConfigValue>(
    config: &dyn configmodel::Config,
    section: &str,
    name: &str,
) -> Result<T, ConfigError> {
    get_config::<T>(config, section, name)?
        .ok_or_else(|| ConfigError::Missing(format!("{section}.{name}")))
}

/// Configuration for a `Client`. Essentially has the same fields as a
/// `HttpClientBuilder`, but required fields are not optional and values have
/// been appropriately parsed and validated.
#[derive(Debug)]
pub(crate) struct Config {
    #[allow(dead_code)]
    pub(crate) repo_name: String,
    pub(crate) server_url: Url,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) try_route_consistently: bool,
    pub(crate) disable_sequential_tree_uploads: bool,
    pub(crate) augmented_trees: bool,
    pub(crate) max_commit_data_per_batch: Option<usize>,
    pub(crate) max_files_per_batch: Option<usize>,
    pub(crate) max_trees_per_batch: Option<usize>,
    pub(crate) max_history_per_batch: Option<usize>,
    pub(crate) max_path_history_per_batch: Option<usize>,
    pub(crate) max_location_to_hash_per_batch: Option<usize>,
    pub(crate) max_commit_mutations_per_batch: Option<usize>,
    pub(crate) max_commit_translate_id_per_batch: Option<usize>,
    pub(crate) min_batch_size: Option<usize>,
    pub(crate) connect_timeout: Option<Duration>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) handler_timeouts: HashMap<String, Duration>,
    #[allow(dead_code)]
    pub(crate) debug: bool,
    pub(crate) http_version: Option<HttpVersion>,
    pub(crate) log_dir: Option<PathBuf>,
    pub(crate) encoding: Option<Encoding>,
    pub(crate) min_transfer_speed: Option<MinTransferSpeed>,
    pub(crate) handler_min_transfer_speeds: HashMap<String, MinTransferSpeed>,
    pub(crate) max_retry_per_request: usize,
    pub(crate) stream_retries: bool,
    pub(crate) http_config: http_client::Config,
}

impl TryFrom<HttpClientBuilder> for Config {
    type Error = SaplingRemoteApiError;

    fn try_from(builder: HttpClientBuilder) -> Result<Self, Self::Error> {
        let HttpClientBuilder {
            repo_name,
            server_url,
            headers,
            try_route_consistently,
            disable_sequential_tree_uploads,
            augmented_trees,
            max_commit_data_per_batch,
            max_files_per_batch,
            max_trees_per_batch,
            max_history_per_batch,
            max_path_history_per_batch,
            max_location_to_hash_per_batch,
            max_commit_mutations_per_batch,
            max_commit_translate_id_per_batch,
            min_batch_size,
            connect_timeout,
            timeout,
            handler_timeouts,
            debug,
            http_version,
            log_dir,
            encoding,
            min_transfer_speed,
            handler_min_transfer_speeds,
            max_retry_per_request,
            stream_retries,
            http_config,
        } = builder;

        // Check for missing required fields.
        let repo_name = repo_name.ok_or(ConfigError::Missing("remotefilelog.reponame".into()))?;
        let mut server_url = server_url.ok_or(ConfigError::Missing("edenapi.url".into()))?;

        // Ensure the base URL's path ends with a slash so that `Url::join`
        // won't strip the final path component.
        if !server_url.path().ends_with('/') {
            let path = format!("{}/", server_url.path());
            server_url.set_path(&path);
        }

        // Setting these to 0 is the same as None.
        let max_commit_data_per_batch = max_commit_data_per_batch.filter(|n| *n > 0);
        let max_files_per_batch = max_files_per_batch.filter(|n| *n > 0);
        let max_trees_per_batch = max_trees_per_batch.filter(|n| *n > 0);
        let max_history_per_batch = max_history_per_batch.filter(|n| *n > 0);
        let max_path_history_per_batch = max_path_history_per_batch.filter(|n| *n > 0);

        Ok(Config {
            repo_name,
            server_url,
            headers,
            try_route_consistently,
            disable_sequential_tree_uploads,
            augmented_trees,
            max_commit_data_per_batch,
            max_files_per_batch,
            max_trees_per_batch,
            max_history_per_batch,
            max_path_history_per_batch,
            max_location_to_hash_per_batch,
            max_commit_mutations_per_batch,
            max_commit_translate_id_per_batch,
            min_batch_size,
            connect_timeout,
            timeout,
            handler_timeouts,
            debug,
            http_version,
            log_dir,
            encoding,
            min_transfer_speed,
            handler_min_transfer_speeds,
            max_retry_per_request,
            stream_retries: stream_retries.unwrap_or(true),
            http_config,
        })
    }
}

/// Parse headers from a JSON object.
fn parse_headers(headers: impl AsRef<str>) -> Result<HashMap<String, String>, Error> {
    serde_json::from_str(headers.as_ref())
        .context(format!("Not a valid JSON object: {:?}", headers.as_ref()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn make_config(entries: Vec<(&str, &str)>) -> BTreeMap<String, String> {
        let mut base: Vec<(&str, &str)> = vec![
            ("edenapi.url", "https://example.com/edenapi"),
            ("remotefilelog.reponame", "testrepo"),
        ];
        base.extend(entries);
        base.into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn test_stream_retries_config() {
        for (value, enabled) in [(None, true), (Some("true"), true), (Some("false"), false)] {
            let entries = value
                .map(|value| vec![("experimental.slapi-stream-retries", value)])
                .unwrap_or_default();
            let config: Config = HttpClientBuilder::from_config(&make_config(entries))
                .unwrap()
                .try_into()
                .unwrap();
            assert_eq!(config.stream_retries, enabled);
        }
        let config: Config = HttpClientBuilder::new()
            .repo_name("test")
            .server_url("https://example.com/".parse().unwrap())
            .try_into()
            .unwrap();
        assert!(config.stream_retries);
        assert!(
            HttpClientBuilder::from_config(&make_config(vec![(
                "experimental.slapi-stream-retries",
                "invalid"
            )]))
            .is_err()
        );
    }

    #[test]
    fn test_auth_cats_header_from_file() {
        let dir = std::env::temp_dir().join("test_auth_cats_header_from_file");
        std::fs::create_dir_all(&dir).unwrap();
        let cats_file = dir.join("cats_token");
        std::fs::write(
            &cats_file,
            r#"{"crypto_auth_tokens":"my-secret-cat-token"}"#,
        )
        .unwrap();

        let config = make_config(vec![
            ("cats.authentry.path", cats_file.to_str().unwrap()),
            ("cats.authentry.type", "auth"),
        ]);

        let builder = HttpClientBuilder::from_config(&config).unwrap();
        assert_eq!(
            builder
                .headers
                .get(cats_constants::X_AUTH_CATS_HEADER)
                .map(|s| s.as_str()),
            Some("my-secret-cat-token"),
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_auth_cats_missing_file_no_header() {
        let config = make_config(vec![
            ("cats.authentry.path", "/nonexistent/path/to/cats_token"),
            ("cats.authentry.type", "auth"),
        ]);

        let builder = HttpClientBuilder::from_config(&config).unwrap();
        assert!(
            !builder
                .headers
                .contains_key(cats_constants::X_AUTH_CATS_HEADER)
        );
    }

    #[test]
    fn test_auth_cats_no_config_no_header() {
        let config = make_config(vec![]);

        let builder = HttpClientBuilder::from_config(&config).unwrap();
        assert!(
            !builder
                .headers
                .contains_key(cats_constants::X_AUTH_CATS_HEADER)
        );
    }

    #[test]
    fn test_wanted_key_picks_entry() {
        let dir = std::env::temp_dir().join("test_wanted_key_picks_entry");
        std::fs::create_dir_all(&dir).unwrap();
        let cats_file = dir.join("preminted_cats");
        std::fs::write(
            &cats_file,
            r#"{"crypto_auth_tokens":"merged-token","interngraph":"ig-token"}"#,
        )
        .unwrap();

        let config = make_config(vec![
            ("cats.myauth.path", cats_file.to_str().unwrap()),
            ("cats.myauth.type", "auth"),
            ("cats.myauth.wanted-key", "interngraph"),
        ]);

        let builder = HttpClientBuilder::from_config(&config).unwrap();
        assert_eq!(
            builder
                .headers
                .get(cats_constants::X_AUTH_CATS_HEADER)
                .map(|s| s.as_str()),
            Some("ig-token"),
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_direct_host_header_set() {
        let config = make_config(vec![("mononoke.direct-host", "some.host:1234")]);

        let builder = HttpClientBuilder::from_config(&config).unwrap();
        assert_eq!(
            builder.headers.get(DIRECT_HOST_HEADER).map(|s| s.as_str()),
            Some("some.host:1234"),
        );
    }

    #[test]
    fn test_direct_host_header_absent_by_default() {
        let config = make_config(vec![]);

        let builder = HttpClientBuilder::from_config(&config).unwrap();
        assert!(!builder.headers.contains_key(DIRECT_HOST_HEADER));
    }

    fn config_from(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn resolved_server_url(config: &BTreeMap<String, String>, url: &str) -> Option<String> {
        let repo_url = RepoUrl::from_str(config, url).unwrap();
        Builder::from_config(config)
            .unwrap()
            .repo_url(&repo_url)
            .unwrap()
            .resolved_server_url()
            .unwrap()
            .map(String::from)
    }

    #[test]
    fn test_resolved_server_url_from_mononoke_path() {
        let config = config_from(&[("schemes.mono", "mononoke://localhost:8443/{1}")]);
        assert_eq!(
            resolved_server_url(&config, "mono:repo").as_deref(),
            Some("https://localhost:8443/slapi/"),
            "an unconfigured mononoke path names the server"
        );
        assert_eq!(
            resolved_server_url(&config, "mononoke://[::1]/path/to/repo").as_deref(),
            Some("https://[::1]/slapi/"),
            "the server URL does not include the repo name"
        );
    }

    #[test]
    fn test_resolved_server_url_prefers_config() {
        let config = config_from(&[("edenapi.url", "https://configured.example.com/edenapi/")]);
        assert_eq!(
            resolved_server_url(&config, "mononoke://example.com/repo").as_deref(),
            Some("https://configured.example.com/edenapi/"),
            "an explicit edenapi.url wins over the path"
        );

        let repo_url = RepoUrl::from_str(&config, "mononoke://example.com/repo").unwrap();
        let explicit = Url::parse("https://explicit.example.com/edenapi/").unwrap();
        let builder = Builder::from_config(&config)
            .unwrap()
            .repo_url(&repo_url)
            .unwrap()
            .server_url(Some(explicit.clone()));
        assert_eq!(
            builder.resolved_server_url().unwrap(),
            Some(explicit),
            "an explicit server URL wins over edenapi.url"
        );

        let config = config_from(&[("edenapi.url", "")]);
        assert_eq!(
            resolved_server_url(&config, "mononoke://example.com/repo").as_deref(),
            Some("https://example.com/slapi/"),
            "an empty edenapi.url counts as unset"
        );
    }

    #[test]
    fn test_resolved_server_url_ignores_other_paths() {
        let config = config_from(&[]);
        for url in [
            "https://example.com/edenapi/",
            "ssh://user@host/repo",
            "eager:///path/to/repo",
        ] {
            assert_eq!(
                resolved_server_url(&config, url),
                None,
                "{url} names no server"
            );
        }
    }

    #[test]
    fn test_resolved_server_url_kill_switch() {
        let config = config_from(&[("experimental.slapi-url-from-path", "false")]);
        assert_eq!(
            resolved_server_url(&config, "mononoke://example.com/repo"),
            None,
            "experimental.slapi-url-from-path=false ignores the path"
        );
    }

    #[test]
    fn test_has_server_url_and_repo_name() {
        let has_both = |entries: &[(&str, &str)], url: Option<&str>| {
            let config = config_from(entries);
            let builder = Builder::from_config(&config).unwrap();
            let builder = match url {
                Some(url) => builder
                    .repo_url(&RepoUrl::from_str(&config, url).unwrap())
                    .unwrap(),
                None => builder,
            };
            builder.has_server_url_and_repo_name().unwrap()
        };

        assert!(has_both(
            &[("remotefilelog.reponame", "repo")],
            Some("mononoke://example.com/repo")
        ));
        assert!(has_both(
            &[
                ("edenapi.url", "https://example.com/edenapi/"),
                ("remotefilelog.reponame", "repo"),
            ],
            None
        ));
        assert!(
            !has_both(&[], Some("mononoke://example.com/repo")),
            "the repo name comes from config, not the path"
        );
        assert!(
            !has_both(&[("remotefilelog.reponame", "repo")], None),
            "nothing names a server URL"
        );
        assert!(!has_both(
            &[
                ("experimental.slapi-url-from-path", "false"),
                ("remotefilelog.reponame", "repo"),
            ],
            Some("mononoke://example.com/repo")
        ));
    }

    #[test]
    fn test_build_uses_path_server_url() {
        let config = config_from(&[("remotefilelog.reponame", "repo")]);
        let repo_url = RepoUrl::from_str(&config, "mononoke://example.com:8443/repo").unwrap();
        let client = Builder::from_config(&config)
            .unwrap()
            .repo_url(&repo_url)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            client.url().as_deref(),
            Some("https://example.com:8443/slapi/")
        );
    }

    #[test]
    fn test_wanted_key_missing_falls_back() {
        let dir = std::env::temp_dir().join("test_wanted_key_fallback");
        std::fs::create_dir_all(&dir).unwrap();
        let cats_file = dir.join("preminted_cats");
        std::fs::write(
            &cats_file,
            r#"{"crypto_auth_tokens":"merged-token","interngraph":"ig-token"}"#,
        )
        .unwrap();

        let config = make_config(vec![
            ("cats.myauth.path", cats_file.to_str().unwrap()),
            ("cats.myauth.type", "auth"),
            ("cats.myauth.wanted-key", "nonexistent_key"),
        ]);

        let builder = HttpClientBuilder::from_config(&config).unwrap();
        assert_eq!(
            builder
                .headers
                .get(cats_constants::X_AUTH_CATS_HEADER)
                .map(|s| s.as_str()),
            Some("merged-token"),
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
