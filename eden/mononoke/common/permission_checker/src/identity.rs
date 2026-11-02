/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
#[cfg(fbcode_build)]
use authenticated_identity_thrift::AuthenticatedIdentity;
#[cfg(fbcode_build)]
use infrasec_authorization::Identity as ThriftIdentity;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;

#[cfg(not(fbcode_build))]
use crate::oss::AuthenticatedIdentity;
#[cfg(not(fbcode_build))]
use crate::oss::Identity as OssIdentity;

pub type MononokeIdentitySet = BTreeSet<MononokeIdentity>;

/// Newtype wrapper around `AuthenticatedIdentity`. All Mononoke identities are
/// `AuthenticatedIdentity`s now -- "thin" ones produced by
/// `MononokeIdentity::from_legacy_type_data` carry only `id_type` / `id_data` with
/// empty attributes, while ingress paths (mTLS cert, `mid://` SAN URIs, forwarded
/// JSON envelope, srserver) populate the full struct.
///
/// The inner `AuthenticatedIdentity` is private to keep ingestion centralized:
/// construct via `MononokeIdentity::from(auth_id)` (or
/// `MononokeIdentity::from_legacy_type_data(...)`), and access via
/// `inner()` / `into_inner()` / the `id_type()` / `id_data()` accessors.
#[derive(Clone, Debug)]
pub struct MononokeIdentity(AuthenticatedIdentity);

// Manual implementations for Eq, PartialEq, Hash, Ord, PartialOrd
// that compare based on id_type and id_data only -- attributes/source/etc
// are not part of identity equality (an identity with different attributes
// is still the same identity).
impl PartialEq for MononokeIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.id_type() == other.id_type() && self.id_data() == other.id_data()
    }
}

impl Eq for MononokeIdentity {}

impl std::hash::Hash for MononokeIdentity {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id_type().hash(state);
        self.id_data().hash(state);
    }
}

impl Ord for MononokeIdentity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.id_type(), self.id_data()).cmp(&(other.id_type(), other.id_data()))
    }
}

impl PartialOrd for MononokeIdentity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl MononokeIdentity {
    /// Construct a "thin" identity from a legacy `(id_type, id_data)` pair.
    ///
    /// The result is a `MononokeIdentity` wrapping an `AuthenticatedIdentity` whose
    /// only populated fields are `identity.id_type` / `identity.id_data` -- empty
    /// `attributes`, empty `loggingKey`, no `catPayload`, default `Source::UNKNOWN`.
    /// All the rich metadata that a real credential would carry (agent attributes,
    /// origin tags, transport source, CAT payload, etc.) is lost.
    ///
    /// **Prefer wrapping a full `AuthenticatedIdentity` whenever the caller actually
    /// has one** -- use `MononokeIdentity::from(auth_id)` (or `MononokeIdentity(auth_id)`)
    /// for credentials parsed via `try_from_x509` / `try_from_json_encoded` /
    /// `authenticated_identities_struct()` / CAT verification. This function should be
    /// reserved for cases where only `(id_type, id_data)` is available -- synthetic
    /// identities (allowlist entries from configerator, reviewer identities, hook
    /// author lookups by unixname, test fixtures, the OSS `X509_SUBJECT_NAME`
    /// fallback). Such identities are not distinguishable from real ones at the
    /// `MononokeIdentitySet` level, but downstream code that inspects `attributes`
    /// or `source` will see empty / default values.
    pub fn from_legacy_type_data(id_type: impl Into<String>, id_data: impl Into<String>) -> Self {
        let id_type = id_type.into();
        let id_data = id_data.into();
        #[cfg(fbcode_build)]
        let auth_id = AuthenticatedIdentity {
            identity: ThriftIdentity {
                id_type,
                id_data,
                ..Default::default()
            },
            ..Default::default()
        };
        #[cfg(not(fbcode_build))]
        let auth_id = AuthenticatedIdentity {
            identity: OssIdentity { id_type, id_data },
            attributes: vec![],
        };
        Self(auth_id)
    }

    /// Identity of every caller of a server built for OSS. The OSS Thrift
    /// transport does not authenticate callers, so this marks requests as
    /// coming from an unverified local process. It is not a `USER` identity,
    /// so nothing infers a commit author from it.
    #[cfg(not(fbcode_build))]
    pub fn oss_local() -> Self {
        Self::from_legacy_type_data("OSS_LOCAL", "loopback")
    }

    pub fn id_type(&self) -> &str {
        self.0.identity.id_type.as_str()
    }

    pub fn id_data(&self) -> &str {
        self.0.identity.id_data.as_str()
    }

    pub fn is_of_type(&self, id_type: &str) -> bool {
        self.id_type() == id_type
    }

    /// Render the identity in the debug-friendly form produced by the C++
    /// canonical logging formatter at `access/if/AuthenticatedIdentity.cpp`:
    /// `AuthenticatedIdentity{identity=TYPE:data, source=ENUM, attributes=[{ns/name=val}, ...]}`.
    ///
    /// Used for Scuba's `client_identities_typed` column and for log lines
    /// that surface client identity. Attributes are emitted without URI
    /// escaping, structure is explicit, and `source` is included as a name --
    /// so this is suitable for human reading and Scuba grouping, but **not**
    /// for wire envelopes or anything that needs to round-trip through a URI
    /// parser. For those, serialize the underlying `AuthenticatedIdentity` via
    /// `authenticated_identity_serializer::serialize` directly.
    ///
    /// In OSS builds the C++ formatter is unavailable, so this falls back to
    /// a plain `TYPE:data` summary that drops attributes.
    pub fn to_typed_string(&self) -> String {
        #[cfg(fbcode_build)]
        {
            authenticated_identity_serializer::to_string(self.0.clone())
        }
        #[cfg(not(fbcode_build))]
        {
            format!("{}:{}", self.id_type(), self.id_data())
        }
    }

    /// Borrow the inner `AuthenticatedIdentity`.
    pub fn inner(&self) -> &AuthenticatedIdentity {
        &self.0
    }

    /// Consume the wrapper and return the inner `AuthenticatedIdentity`.
    pub fn into_inner(self) -> AuthenticatedIdentity {
        self.0
    }
}

impl From<AuthenticatedIdentity> for MononokeIdentity {
    fn from(auth_id: AuthenticatedIdentity) -> Self {
        Self(auth_id)
    }
}

pub fn merge_identity_sets(
    mut identities: MononokeIdentitySet,
    other: MononokeIdentitySet,
) -> MononokeIdentitySet {
    for identity in other {
        match identities.take(&identity) {
            Some(mut existing) => {
                for attribute in identity.0.attributes {
                    if !existing.0.attributes.contains(&attribute) {
                        existing.0.attributes.push(attribute);
                    }
                }
                identities.insert(existing);
            }
            None => {
                identities.insert(identity);
            }
        }
    }
    identities
}

impl fmt::Display for MononokeIdentity {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "{}:{}", self.id_type(), self.id_data())
    }
}

impl FromStr for MononokeIdentity {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (ty, data) = value.split_once(':').with_context(|| {
            format!("MononokeIdentity parse error, expected TYPE:data, got {value:?}")
        })?;
        Ok(Self::from_legacy_type_data(ty, data))
    }
}

impl Serialize for MononokeIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MononokeIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

pub trait MononokeIdentitySetExt {
    fn likely_an_agent(&self) -> bool;

    fn is_proxygen_test_identity(&self) -> bool;

    fn hostprefix(&self) -> Option<&str>;

    fn hostname(&self) -> Option<&str>;

    fn username(&self) -> Option<&str>;
    fn crewmate(&self) -> Option<&str>;
    fn sandcastle_job_id(&self) -> Option<&str>;
    fn on_demand_type(&self) -> Option<&str>;

    fn identity_type_filtered_concat(&self, id_type: &str) -> Option<String>;
    fn main_client_identity(&self, sandcastle_alias: Option<&str>) -> String;

    /// Classify this identity set into a coarse [`ClientCategory`] for
    /// rate-limit policy and Scuba logging. Sandcastle traffic without an
    /// alias is classified separately from CI traffic with an alias.
    fn client_category(&self, sandcastle_alias: Option<&str>) -> ClientCategory;

    fn to_string(&self) -> String;
}

/// Coarse client categories derived from the identity set.
///
/// Used as a per-request Scuba dimension and (eventually) the key for
/// per-category rate-limit allowances. Rules live in the `fbcode_build`
/// impl of [`MononokeIdentitySetExt::client_category`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientCategory {
    HealthCheck,
    InteractiveDev,
    DevEnv,
    CiSandcastle,
    SandcastleAutomation,
    Mast,
    FaaS,
    Automation,
    Unknown,
}

impl ClientCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::HealthCheck => "health_check",
            Self::InteractiveDev => "interactive_dev",
            Self::DevEnv => "dev_env",
            Self::CiSandcastle => "ci_sandcastle",
            Self::SandcastleAutomation => "sandcastle_automation",
            Self::Mast => "mast",
            Self::FaaS => "faas",
            Self::Automation => "automation",
            Self::Unknown => "unknown",
        }
    }
}

/// A request's tenancy dimensions, used for RIM attribution and rate-limit
/// policy.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TenantInfo {
    HealthCheck {
        client_id: Option<String>,
    },
    InteractiveDev {
        client_id: Option<String>,
    },
    DevEnv {
        client_id: Option<String>,
    },
    CiSandcastle {
        client_id: Option<String>,
        ci_purpose: Option<String>,
    },
    SandcastleAutomation {
        client_id: Option<String>,
    },
    Mast {
        client_id: Option<String>,
    },
    FaaS {
        client_id: Option<String>,
        atlas_env_id: Option<String>,
        atlas_rl: Option<bool>,
        atlas_purpose: Option<String>,
        faas_job_name: Option<String>,
    },
    Automation {
        client_id: Option<String>,
    },
    Unknown {
        client_id: Option<String>,
    },
}

impl TenantInfo {
    pub fn category(&self) -> ClientCategory {
        match self {
            Self::HealthCheck { .. } => ClientCategory::HealthCheck,
            Self::InteractiveDev { .. } => ClientCategory::InteractiveDev,
            Self::DevEnv { .. } => ClientCategory::DevEnv,
            Self::CiSandcastle { .. } => ClientCategory::CiSandcastle,
            Self::SandcastleAutomation { .. } => ClientCategory::SandcastleAutomation,
            Self::Mast { .. } => ClientCategory::Mast,
            Self::FaaS { .. } => ClientCategory::FaaS,
            Self::Automation { .. } => ClientCategory::Automation,
            Self::Unknown { .. } => ClientCategory::Unknown,
        }
    }

    pub fn client_id(&self) -> Option<&str> {
        match self {
            Self::HealthCheck { client_id }
            | Self::InteractiveDev { client_id }
            | Self::DevEnv { client_id }
            | Self::CiSandcastle { client_id, .. }
            | Self::SandcastleAutomation { client_id }
            | Self::Mast { client_id }
            | Self::FaaS { client_id, .. }
            | Self::Automation { client_id }
            | Self::Unknown { client_id } => client_id.as_deref(),
        }
    }

    pub fn ci_purpose(&self) -> Option<&str> {
        match self {
            Self::CiSandcastle { ci_purpose, .. } => ci_purpose.as_deref(),
            _ => None,
        }
    }

    pub fn atlas_env_id(&self) -> Option<&str> {
        match self {
            Self::FaaS { atlas_env_id, .. } => atlas_env_id.as_deref(),
            _ => None,
        }
    }

    pub fn atlas_rl(&self) -> Option<bool> {
        match self {
            Self::FaaS { atlas_rl, .. } => *atlas_rl,
            _ => None,
        }
    }

    pub fn atlas_purpose(&self) -> Option<&str> {
        match self {
            Self::FaaS { atlas_purpose, .. } => atlas_purpose.as_deref(),
            _ => None,
        }
    }

    pub fn faas_job_name(&self) -> Option<&str> {
        match self {
            Self::FaaS { faas_job_name, .. } => faas_job_name.as_deref(),
            _ => None,
        }
    }

    /// Legacy RIM tenancy path: root -> category -> client id. `None` when
    /// there is no `client_id` to attribute to (no meaningful RIM path).
    pub fn legacy_tenancy_path(&self) -> Option<Vec<String>> {
        let client_id = self.client_id()?;
        Some(vec![
            "root".to_string(),
            self.category().as_str().to_string(),
            client_id.to_string(),
        ])
    }
}

impl fmt::Display for TenantInfo {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            fmt,
            "{}",
            self.legacy_tenancy_path().unwrap_or_default().join("/")
        )
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[cfg(not(fbcode_build))]
    #[mononoke::test]
    fn test_oss_local_identity() {
        let id = MononokeIdentity::oss_local();
        assert_eq!(id.id_type(), "OSS_LOCAL");
        assert_eq!(id.id_data(), "loopback");
        // Must not collide with an empty identity or a real user identity.
        assert_ne!(id, MononokeIdentity::from_legacy_type_data("", ""));
        assert!(!id.is_of_type("USER"));
    }

    #[mononoke::test]
    fn test_ipv6_identity() {
        let id = MononokeIdentity::from_str("MACHINE:2621:10d:c1a8:12c9::1162").unwrap();
        assert_eq!(id.id_data(), "2621:10d:c1a8:12c9::1162");
    }

    #[mononoke::test]
    fn test_legacy_tenancy_path() {
        let interactive = TenantInfo::InteractiveDev {
            client_id: Some("USER:alice".to_owned()),
        };
        assert_eq!(
            interactive.legacy_tenancy_path(),
            Some(vec![
                "root".to_string(),
                "interactive_dev".to_string(),
                "USER:alice".to_string(),
            ])
        );
    }

    #[mononoke::test]
    fn test_to_typed_string_thin() {
        let id = MononokeIdentity::from_legacy_type_data("SERVICE", "some_service");
        // In fbcode the identity is rendered through the C++ canonical
        // logging formatter at `access/if/AuthenticatedIdentity.cpp`,
        // producing the `AuthenticatedIdentity{...}` debug shape with the
        // default source (UNKNOWN) and an empty attribute list. In OSS the
        // formatter is unavailable, so the fallback emits a plain
        // `TYPE:data` summary.
        #[cfg(fbcode_build)]
        assert_eq!(
            id.to_typed_string(),
            "AuthenticatedIdentity{identity=SERVICE:some_service, source=UNKNOWN, attributes=[]}",
        );
        #[cfg(not(fbcode_build))]
        assert_eq!(id.to_typed_string(), "SERVICE:some_service");
    }

    #[cfg(not(fbcode_build))]
    #[mononoke::test]
    fn test_to_typed_string_with_attributes() {
        let auth_id = AuthenticatedIdentity {
            identity: crate::oss::Identity {
                id_type: "USER".to_string(),
                id_data: "mzr".to_string(),
            },
            attributes: vec![crate::oss::Attribute {
                identifier: crate::oss::AttributeKey {
                    attributeName: "id".to_string(),
                    attributeNamespace: "agent".to_string(),
                },
                value: crate::oss::IdentityAttribute {
                    attributeValue: "AGENT:devmate".to_string(),
                },
                val: "AGENT:devmate".to_string(),
            }],
        };
        let id = MononokeIdentity::from(auth_id);
        // OSS build: C++ formatter unavailable, so the fallback emits a
        // plain `TYPE:data` summary and drops attributes (the OSS path has
        // no way to mirror the C++ debug form without the formatter).
        assert_eq!(id.to_typed_string(), "USER:mzr");
    }
}
