/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::convert::AsRef;
use std::fmt::Display;

use anyhow::Context;
use anyhow::Result;
use configmodel::Config;
use configmodel::convert::FromConfig;
use fn_error_context::context;
use percent_encoding::AsciiSet;
use percent_encoding::NON_ALPHANUMERIC;
use percent_encoding::percent_decode_str;
use percent_encoding::utf8_percent_encode;
use url::Url;

#[derive(Debug)]
pub struct RepoUrl {
    // What should be saved as paths.default.
    clean_input: String,
    // URL with custom scheme resolved.
    url: Url,
    // Repo name derived from URL.
    repo_name: Option<String>,
    // Fragment from the URL
    default_bookmark: Option<String>,
}

impl RepoUrl {
    #[context("parsing repo URL {input_url}")]
    pub fn from_str(config: &dyn Config, input_url: &str) -> Result<Self> {
        // If input_url looks like a plain Windows path, first normalize with "file://" scheme.
        let url_string = if looks_like_windows_path(input_url) {
            format!("file://{input_url}")
        } else {
            input_url.to_string()
        };

        // Resolve scheme first, before special "file"/"eager" handling for Windows.
        let url_string = resolve_custom_scheme(config, url_string);

        // Do some file path normalization on Windows for schemes referencing fs paths.
        let url_string = match (cfg!(windows), url_string.split_once(':')) {
            (true, Some((scheme @ ("file" | "eager"), path))) => {
                let path = path.trim_start_matches('/');

                // URLs don't like the "?" from UNC path. We will add
                // it back later in the `path()` method.
                let path = path.trim_start_matches(r"\\?");

                // URLs are happier with forward slashes.
                let path = path.replace(r"\", "/");

                // Adding an extra forward slash seems to do the best job. It avoids "C:"
                // from getting interpreted as the host.
                format!("{scheme}:///{path}")
            }
            _ => url_string,
        };

        let base_url = Url::parse("file:///.").unwrap();
        let parse_opts = Url::options().base_url(Some(&base_url));
        let mut url = match parse_opts.parse(&url_string) {
            Ok(url) => url,
            Err(err) => {
                tracing::warn!("error parsing repo URL {url_string}: {err:?}");
                return Err(err.into());
            }
        };

        // Fragment is only used for choosing default bookmark during clone - we
        // don't want to persist it.
        let frag = url.fragment().map(|f| f.to_string());
        url.set_fragment(None);

        // Prefer to keep the exact input_url when possible.
        let clean_input = if frag.is_some() {
            url.as_str().to_string()
        } else {
            input_url.to_string()
        };

        let repo_name = repo_name_from_resolved_url(config, &url);
        tracing::debug!(input_url, output_url=%url, ?repo_name, "parsed repo URL");

        Ok(Self {
            clean_input,
            url,
            repo_name,
            default_bookmark: frag,
        })
    }

    pub fn scheme(&self) -> &str {
        self.url.scheme()
    }

    // URL path. Useful for "file" and "eager" schemes to get the file system path.
    pub fn path(&self) -> String {
        match (cfg!(windows), self.scheme()) {
            // Convert Windows path to UNC format.
            (true, "file" | "eager") => {
                let path = self.url.path().trim_start_matches('/');
                format!(r"\\?\{}", path.replace('/', r"\"))
            }
            _ => self.url.path().to_string(),
        }
    }

    pub fn repo_name(&self) -> Option<&str> {
        self.repo_name.as_deref()
    }

    pub fn default_bookmark(&self) -> Option<&str> {
        self.default_bookmark.as_deref()
    }

    /// Input string sans URL fragment.
    /// What "clone" should persist as paths.default.
    pub fn clean_str(&self) -> &str {
        &self.clean_input
    }

    /// URL with schemes resolved and fragment trimmed.
    pub fn resolved_str(&self) -> &str {
        self.url.as_str()
    }

    pub fn supports_slapi(&self) -> bool {
        matches!(self.scheme(), "mononoke" | "eager" | "test")
    }
}

fn looks_like_windows_path(s: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }

    // UNC prefix
    if s.starts_with(r"\\") {
        return true;
    }

    // Drive prefix (e.g. "c:")
    let bytes = s.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

impl Display for RepoUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.url)
    }
}

impl FromConfig for RepoUrl {
    fn try_from_str_with_config(c: &dyn Config, s: &str) -> configmodel::Result<Self> {
        Self::from_str(c, s).map_err(|err| configmodel::Error::Convert(format!("{err:?}")))
    }
}

/// Using custom "schemes" from config, resolve given url.
fn resolve_custom_scheme(config: &dyn Config, url: String) -> String {
    if let Some((scheme, rest)) = url.split_once(':') {
        if let Some(tmpl) = config.get_nonempty("schemes", scheme) {
            let rest = rest.trim_start_matches('/');
            return if tmpl.contains("{1}") {
                tmpl.replace("{1}", rest)
            } else {
                format!("{tmpl}{rest}")
            };
        }
    }

    url
}

pub fn repo_name_from_url(config: &dyn Config, s: &str) -> Option<String> {
    RepoUrl::from_str(config, s)
        .map(|url| url.repo_name().map(|name| name.to_string()))
        .ok()
        .flatten()
}

fn repo_name_from_resolved_url(config: &dyn Config, url: &Url) -> Option<String> {
    match url.scheme() {
        "mononoke" => {
            // In Mononoke URLs, the repo name is always the full path
            // with slashes trimmed.
            let path = url.path().trim_matches('/');
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
        _ => {
            // Try to remove special prefixes to guess the repo name from that
            if let Some(repo_prefix) = config.get("remotefilelog", "reponame-path-prefixes") {
                if let Some((_, reponame)) = url.path().split_once(repo_prefix.to_string().as_str())
                {
                    if !reponame.is_empty() {
                        return Some(reponame.to_string());
                    }
                }
            }
            // For URLs with specific host (like git.*), the repo name is the full path.
            if let Some(prefix) = config.get("remotefilelog", "reponame-host-prefixes") {
                if let Some(host) = url.host_str()
                    && host.starts_with(prefix.to_string().as_str())
                {
                    let reponame = url.path().trim_matches('/');
                    if !reponame.is_empty() {
                        return Some(reponame.to_string());
                    }
                }
            }
            // Try the last segment in url path.
            if let Some(last_segment) = url
                .path_segments()
                .and_then(|s| s.rev().find(|s| !s.is_empty()))
            {
                return Some(last_segment.to_string());
            }
            // Try path. `path_segment` can be `None` for URL like "test:reponame".
            let path = url.path().trim_matches('/');
            if !path.is_empty() {
                return Some(path.to_string());
            }
            // Try the hostname. ex. in "fb://fbsource", "fbsource" is a host not a path.
            // Also see https://www.mercurial-scm.org/repo/hg/help/schemes
            if let Some(host_str) = url.host_str() {
                return Some(host_str.to_string());
            }
        }
    }
    None
}

/// All non-alphanumeric characters (except hyphens, underscores, and periods)
/// found in the repo's name will be percent-encoded before being used in URLs.
/// Characters allowed in a repo name (like `+` and `/`) since they are reserved
/// characters according to RFC 3986 section 2.2 Reserved Characters (January 2005)
const RESERVED_CHARS: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'_')
    .remove(b'-')
    .remove(b'.')
    .add(b'+')
    .add(b'/');

pub fn encode_repo_name(repo_name: impl AsRef<str>) -> String {
    utf8_percent_encode(repo_name.as_ref(), RESERVED_CHARS).to_string()
}

pub fn decode_repo_name(repo_name_encoded: impl AsRef<str>) -> Result<String> {
    Ok(percent_decode_str(repo_name_encoded.as_ref())
        .decode_utf8()
        .context("Repo name must be utf-8 percent encoded")?
        .to_string())
}

#[cfg(test)]
mod test {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn test_repo_name_from_url() {
        let config = BTreeMap::<&str, &str>::from([("schemes.fb", "mononoke://example.com/{1}")]);

        let check = |url, name| {
            let repo_url = RepoUrl::from_str(&config, url).unwrap();
            assert_eq!(repo_url.repo_name(), name);
        };

        // Ordinary schemes use the basename as the repo name
        check("repo", Some("repo"));
        check("../path/to/repo", Some("repo"));
        check("file:repo", Some("repo"));
        check("file:/path/to/repo", Some("repo"));
        check("file://server/path/to/repo", Some("repo"));
        check("ssh://user@host/repo", Some("repo"));
        check("ssh://user@host/path/to/repo", Some("repo"));
        check("file:/", None);

        // This isn't correct, but is a side-effect of earlier hacks (should
        // be `None`)
        check("ssh://user@host:100/", Some("host"));

        // Mononoke scheme uses the full path, and repo names can contain
        // slashes.
        check("mononoke://example.com/repo", Some("repo"));
        check("mononoke://example.com/path/to/repo", Some("path/to/repo"));
        check("mononoke://example.com/", None);

        // FB scheme uses the full path.
        check("fb:repo", Some("repo"));
        check("fb:path/to/repo", Some("path/to/repo"));
        check("fb:", None);

        // FB scheme works even when there are extra slashes that shouldn't be
        // there.
        check("fb://repo/", Some("repo"));
        check("fb://path/to/repo", Some("path/to/repo"));

        check("eager:/some/repo//", Some("repo"));
        check("eager:///some/repo", Some("repo"));
        if cfg!(windows) {
            check(r"eager:C:\some\repo", Some("repo"));
            check(r"eager:C:\some/repo", Some("repo"));
            check(r"eager://C:\some\repo", Some("repo"));
            check(r"eager://C:\some/repo", Some("repo"));
            check(r"eager:\\C\some\repo", Some("repo"));
        }
    }

    #[test]
    fn test_resolve_custom_scheme() {
        let config = BTreeMap::<&str, &str>::from([
            ("schemes.append", "appended://bar/"),
            ("schemes.subst", "substd://bar/{1}/baz"),
        ]);

        let check = |url, scheme, path| {
            let repo_url = RepoUrl::from_str(&config, url).unwrap();
            assert_eq!(repo_url.scheme(), scheme);
            assert_eq!(repo_url.path(), path);
        };

        check("other:foo", "other", "foo");
        check("append:one/two", "appended", "/one/two");
        check("subst://one/two", "substd", "/one/two/baz");
    }

    #[test]
    fn test_path() {
        let config = BTreeMap::<&str, &str>::from([("schemes.myeager", "eager:///")]);

        let check = |url, scheme, path| {
            let repo_url = RepoUrl::from_str(&config, url).unwrap();
            assert_eq!(repo_url.scheme(), scheme);
            assert_eq!(repo_url.path(), path);
            assert_eq!(repo_url.clean_str(), url);
        };

        if cfg!(windows) {
            check(r"C:\foo\bar", "file", r"\\?\C:\foo\bar");
            check(r"\\?\C:\foo\bar", "file", r"\\?\C:\foo\bar");
            check(r"\\?\C:foo/bar", "file", r"\\?\C:foo\bar");
            check(r"eager://C:\foo\bar", "eager", r"\\?\C:\foo\bar");
            check(r"myeager://\\?\C:\foo/bar", "eager", r"\\?\C:\foo\bar");
            check(r"test:foo\bar", "test", r"foo\bar");
        } else {
            check("file:///foo/bar", "file", "/foo/bar");
            check("eager:///foo/bar", "eager", "/foo/bar");
            check("myeager:/foo/bar", "eager", "/foo/bar");
        }
    }
}
