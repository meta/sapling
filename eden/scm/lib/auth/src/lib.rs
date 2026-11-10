/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::str;

use anyhow::Error;
use anyhow::Result;
use configmodel::Config;
use configmodel::ConfigExt;
use configmodel::Text;
use indexmap::IndexMap;
use repourl::RepoUrl;
use thiserror::Error;
use url::Url;
use util::path::expand_path;

pub mod x509;

pub use x509::X509Error;
pub use x509::check_certs;

#[derive(Debug, Error)]
#[error("Certificate(s) or private key(s) not found for {url}: {missing:?}\n{msg}")]
pub struct MissingCerts {
    url: String,
    missing: HashSet<PathBuf>,
    msg: String,
}

/// A group of client authentication settings from the user's config.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthGroup {
    pub name: String,
    pub prefix: String,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub cacerts: Option<PathBuf>,
    pub username: Option<String>,
    pub schemes: Vec<String>,
    pub priority: i32,
    pub extras: HashMap<String, String>,
}

impl AuthGroup {
    fn new(group: &str, mut settings: HashMap<&str, Text>) -> Result<Self> {
        let name = group.into();

        let mut prefix = settings
            .remove("prefix")
            .map(|s| s.to_string())
            .ok_or_else(|| Error::msg("auth prefix missing"))?;

        let cert = settings
            .remove("cert")
            .filter(|s| !s.is_empty())
            .map(expand_path);
        let key = settings
            .remove("key")
            .filter(|s| !s.is_empty())
            .map(expand_path);
        let cacerts = settings
            .remove("cacerts")
            .filter(|s| !s.is_empty())
            .map(expand_path);

        let username = settings.remove("username").map(|s| s.to_string());

        // If the URL prefix for this group has a scheme specified, use that
        // and ignore the contents of the "schemes" field for this group.
        let schemes = if let Some(i) = prefix.find("://") {
            let _ = settings.remove("schemes");
            let scheme = prefix[..i].into();
            prefix = prefix[i + 3..].into();
            vec![scheme]
        } else {
            // Default to HTTPS if no schemes are specified in either the
            // prefix or schemes field.
            settings.remove("schemes").map_or_else(
                || vec!["https".into()],
                |line| line.split(' ').map(String::from).collect(),
            )
        };

        let priority = settings
            .remove("priority")
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or_default();

        let extras = settings
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<HashMap<_, _>>();

        Ok(Self {
            name,
            prefix,
            cert,
            key,
            cacerts,
            username,
            schemes,
            priority,
            extras,
        })
    }
}

#[derive(Clone)]
pub struct AuthSection {
    groups: Vec<AuthGroup>,
    default_cacerts: Option<PathBuf>,
    tls_auth_help: String,
}

impl AuthSection {
    /// Parse the `[auth]` section of a Mercurial config into a map
    /// of grouped auth settings.
    ///
    /// The keys of the resulting HashMap are group names from the config;
    /// i.e., the first component of a key of the form `group.setting`.
    /// All keys in the auth section are expected to follow this format.
    ///
    /// Values are parsed `Auth` structs containing all of the values
    /// found for the given grouping.
    pub fn from_config(config: &dyn Config) -> Self {
        // Use an IndexMap to preserve ordering; needed to correctly handle precedence.
        let mut groups = IndexMap::new();

        let keys = config.keys("auth");
        for key in &keys {
            // Skip keys that aren't valid UTF-8 or that don't match
            // the expected auth key format of `group.setting`.
            let (group, setting) = match key.find('.') {
                Some(i) => (&key[..i], &key[i + 1..]),
                None => continue,
            };
            if let Some(value) = config.get("auth", key) {
                groups
                    .entry(group)
                    .or_insert_with(HashMap::new)
                    .insert(setting, value);
            }
        }

        let groups = groups
            .into_iter()
            .filter_map(|(group, settings)| AuthGroup::new(group, settings).ok())
            .collect();

        Self {
            groups,
            default_cacerts: config.must_get("web", "cacerts").ok(),
            tls_auth_help: config
                .get("help", "tlsauthhelp")
                .unwrap_or_default()
                .to_string(),
        }
    }

    /// The best usable auth group for `repo_url`'s SLAPI server URL or, failing
    /// that, for `repo_url` itself. If neither has one, missing certificate files
    /// are reported for the server URL ahead of the repo URL.
    pub fn best_match_for_repo_url(
        &self,
        repo_url: &RepoUrl,
    ) -> Result<Option<AuthGroup>, MissingCerts> {
        let primary = match repo_url.slapi_server_url() {
            Some(url) => self.best_match_for(url),
            None => Ok(None),
        };
        if let Ok(Some(_)) = primary {
            return primary;
        }

        let secondary = self.best_match_for(repo_url.url());
        if let Ok(Some(_)) = secondary {
            return secondary;
        }

        // Neither is usable: report the server URL's missing files ahead of the repo URL's.
        primary.and(secondary)
    }

    /// Find the best matching auth group for the given URL.
    pub fn best_match_for(&self, url: &Url) -> Result<Option<AuthGroup>, MissingCerts> {
        let mut best: Option<&AuthGroup> = None;
        let mut missing = HashSet::new();

        let scheme = url.scheme().to_string();
        let username = url.username();
        let url_suffix = strip_scheme_and_user(url);

        'groups: for group in &self.groups {
            if !group.schemes.contains(&scheme) {
                continue;
            }

            // If the URL contains a username, the entry's username must
            // match if the entry's username field is non-None.
            if !username.is_empty() {
                match group.username {
                    Some(ref u) if u != username => continue,
                    _ => {}
                }
            }

            if group.prefix != "*" && !url_suffix.starts_with(&group.prefix) {
                continue;
            }

            // If there is an existing candidate, check whether the current
            // auth entry is a more specific match.
            if let Some(best) = best {
                // Take the entry with the longer prefix.
                match group.prefix.len().cmp(&best.prefix.len()) {
                    Ordering::Less => continue,
                    Ordering::Equal => {
                        // If prefixes are the same, break the tie using priority.
                        if group.priority < best.priority {
                            continue;
                        // If the priorities are the same, prefer entries with usernames.
                        } else if group.priority == best.priority
                            && best.username.is_some()
                            && group.username.is_none()
                        {
                            continue;
                        }
                    }
                    Ordering::Greater => {}
                }
            }

            // Skip this group is any of the files are missing.
            for (label, path) in &[
                ("client certificate", &group.cert),
                ("private key", &group.key),
                ("CA certificate bundle", &group.cacerts),
            ] {
                match path {
                    Some(path) if !path.is_file() => {
                        tracing::debug!(
                            "Ignoring [auth] group {:?} because of missing {}: {:?}",
                            &group.name,
                            &label,
                            &path
                        );
                        missing.insert(path.to_path_buf());
                        continue 'groups;
                    }
                    _ => {}
                }
            }

            best = Some(group);
        }

        if let Some(best) = best {
            tracing::debug!(%url, ?best, "best_auth_group");

            let mut best = best.clone();
            if best.cacerts.is_none() {
                if let Some(cacerts) = &self.default_cacerts {
                    tracing::debug!(%url, ?cacerts, "using web.cacerts bundle");
                    best.cacerts = Some(cacerts.clone());
                }
            }
            Ok(Some(best))
        } else if !missing.is_empty() {
            Err(MissingCerts {
                url: url.to_string(),
                missing,
                msg: self.tls_auth_help.clone(),
            })
        } else {
            Ok(None)
        }
    }
}

/// Given a URL, strip off the scheme and username if present.
fn strip_scheme_and_user(url: &Url) -> String {
    let url = url.as_str();
    // Find the position immediately after the '@' if a username is present.
    // or after the scheme otherwise.
    let pos = url
        .find('@')
        .map(|i| i + 1)
        .or_else(|| url.find("://").map(|i| i + 3));

    match pos {
        Some(i) => &url[i..],
        None => url,
    }
    .to_string()
}

#[cfg(test)]
mod test {
    use staticconfig::static_config;

    use super::*;

    #[test]
    fn test_auth_groups() {
        let config = static_config!(
            r#"[auth]
foo.prefix = foo.com
foo.cert = /foo/cert
foo.key = /foo/key
foo.cacerts = /foo/cacerts
bar.prefix = bar.com
bar.cert = /bar/cert
bar.key = /bar/key
baz.cert = /baz/cert
baz.key = /baz/key
foo.username = user
foo.schemes = http https
foo.priority = 1"#
        );
        let groups = AuthSection::from_config(&config).groups;

        // Only 2 groups because "baz" is missing the required "prefix" setting.
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0],
            AuthGroup {
                name: "foo".into(),
                prefix: "foo.com".into(),
                cert: Some("/foo/cert".into()),
                key: Some("/foo/key".into()),
                cacerts: Some("/foo/cacerts".into()),
                username: Some("user".into()),
                schemes: vec!["http".into(), "https".into()],
                priority: 1,
                extras: HashMap::new(),
            }
        );
        assert_eq!(
            groups[1],
            AuthGroup {
                name: "bar".into(),
                prefix: "bar.com".into(),
                cert: Some("/bar/cert".into()),
                key: Some("/bar/key".into()),
                cacerts: None,
                username: None,
                schemes: vec!["https".into()],
                priority: 0,
                extras: HashMap::new(),
            }
        );
    }

    #[test]
    fn test_strip_scheme_and_user() -> Result<()> {
        let url = "https://example.com/".parse()?;
        let stripped = strip_scheme_and_user(&url);
        assert_eq!(stripped, "example.com/");

        let url = "https://user@example.com:433/some/path?query=1".parse()?;
        let stripped = strip_scheme_and_user(&url);
        assert_eq!(stripped, "example.com:433/some/path?query=1");

        Ok(())
    }

    #[test]
    fn test_best_match_for() -> Result<()> {
        let config = static_config!(
            r#"[auth]
default.prefix = *
a.prefix = example.com
a.schemes = http https
b.prefix = foo.com/bar
c.prefix = foo.com/bar/baz
d.prefix = bar.com
d.priority = 1
e.prefix = bar.com
e.username = e_user
f.prefix = baz.com
f.username = f_user
g.prefix = baz.com
h.prefix = example.net
h.username = h_user
i.prefix = example.net
i.username = i_user
j.prefix = invalid.com
j.cert = /does/not/exist"#
        );
        let auth = AuthSection::from_config(&config);

        // Basic case: an exact match.
        let group = auth
            .best_match_for(&"http://example.com".parse()?)?
            .unwrap();
        assert_eq!(group.name, "a");

        // Scheme mismatch.
        let group = auth.best_match_for(&"ftp://example.com".parse()?)?;
        assert!(group.is_none());

        // Given URL's hosts matches a config prefix, but doesn't match
        // the entire prefix.
        let group = auth.best_match_for(&"https://foo.com.".parse()?)?.unwrap();
        assert_eq!(group.name, "default");

        // Matching the entire prefix works as expected.
        let group = auth
            .best_match_for(&"https://foo.com/bar".parse()?)?
            .unwrap();
        assert_eq!(group.name, "b");

        // A more specific prefix wins.
        let group = auth
            .best_match_for(&"https://foo.com/bar/baz".parse()?)?
            .unwrap();
        assert_eq!(group.name, "c");

        // Still matches even if the URL has other components in it.
        let group = auth
            .best_match_for(&"https://foo.com/bar/baz/dir?query=1#fragment".parse()?)?
            .unwrap();
        assert_eq!(group.name, "c");

        // There are two entries matching this prefix, but one has higher priority.
        let group = auth.best_match_for(&"https://bar.com".parse()?)?.unwrap();
        assert_eq!(group.name, "d");

        // Even if another entry has a username match, the higher priority should win.
        let group = auth
            .best_match_for(&"https://e_user@bar.com".parse()?)?
            .unwrap();
        assert_eq!(group.name, "d");

        // Even if no user is specified in the URL, the entry with a username should
        // take precedence all else being equal.
        let group = auth.best_match_for(&"https://baz.com".parse()?)?.unwrap();
        assert_eq!(group.name, "f");

        // If all else fails, later entries take precedence over earlier ones.
        // Even if no user is specified in the URL, the entry with a username should
        // take precedence all else being equal.
        let group = auth
            .best_match_for(&"https://example.net".parse()?)?
            .unwrap();
        assert_eq!(group.name, "i");

        // If the cert of key is missing, the entry shouldn't match.
        let group = auth
            .best_match_for(&"https://invalid.com".parse()?)?
            .unwrap();
        assert_eq!(group.name, "default");

        Ok(())
    }

    #[test]
    fn test_best_match_for_repo_url() -> Result<()> {
        let config = static_config!(
            r#"[auth]
good.prefix = good.com
good_path.prefix = mononoke://good.com
other_path.prefix = mononoke://other.com
half.prefix = half.com
half.cert = /does/not/exist
half_path.prefix = mononoke://half.com
broken.prefix = broken.com
broken.cert = /does/not/exist
broken_path.prefix = mononoke://broken.com
broken_path.cert = /does/not/exist
nohttps_path.prefix = mononoke://nohttps.com
nohttps_path.cert = /does/not/exist
ssh.prefix = ssh://other.com"#
        );
        let auth = AuthSection::from_config(&config);
        let best =
            |url: &str| auth.best_match_for_repo_url(&RepoUrl::from_str(&config, url).unwrap());
        let name = |found: Option<AuthGroup>| found.map(|group| group.name);

        // A group for the SLAPI server URL wins over one for the repo URL.
        let found = best("mononoke://good.com/repo")?;
        assert_eq!(name(found).as_deref(), Some("good"));

        // With no group for the server URL, the repo URL's is used.
        let found = best("mononoke://other.com/repo")?;
        assert_eq!(name(found).as_deref(), Some("other_path"));

        // So it is when the server URL's group has missing files.
        let found = best("mononoke://half.com/repo")?;
        assert_eq!(name(found).as_deref(), Some("half_path"));

        // A repo URL with no SLAPI server URL uses its own group.
        let found = best("ssh://other.com/repo")?;
        assert_eq!(name(found).as_deref(), Some("ssh"));

        // If neither group is usable, the server URL's missing files are reported.
        let err = best("mononoke://broken.com/repo").unwrap_err();
        assert!(err.url.starts_with("https://broken.com"), "{err}");

        // With no group for the server URL, the repo URL's missing files are reported.
        let err = best("mononoke://nohttps.com/repo").unwrap_err();
        assert!(err.url.starts_with("mononoke://nohttps.com"), "{err}");

        Ok(())
    }

    #[test]
    fn test_extras() -> Result<()> {
        let config = static_config!(
            r#"[auth]
foo.prefix = foo.com
foo.username = user
foo.hello = world
foo.bar = baz
             "#
        );
        let auth = AuthSection::from_config(&config);

        let group = auth.best_match_for(&"https://foo.com".parse()?)?.unwrap();

        assert_eq!(group.extras.get("hello").unwrap(), "world");
        assert_eq!(group.extras.get("bar").unwrap(), "baz");
        assert_eq!(group.extras.get("username"), None);

        Ok(())
    }
}
