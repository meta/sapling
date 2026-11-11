/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::default::Default;

use anyhow::Result;
use metaconfig_types::CommonConfig;
use metaconfig_types::Identity;
use permission_checker::AclProvider;
use permission_checker::BoxPermissionChecker;
use permission_checker::MononokeIdentity;
use permission_checker::MononokeIdentitySet;
use permission_checker::PermissionCheckerBuilder;

pub struct ConnectionSecurityChecker {
    checker: BoxPermissionChecker,
}

impl Default for ConnectionSecurityChecker {
    fn default() -> Self {
        let builder = PermissionCheckerBuilder::new();
        ConnectionSecurityChecker {
            checker: builder.build(),
        }
    }
}

impl ConnectionSecurityChecker {
    pub async fn new(acl_provider: &dyn AclProvider, common_config: &CommonConfig) -> Result<Self> {
        let mut builder = PermissionCheckerBuilder::new();

        if let Some(tier) = &common_config.trusted_parties_hipster_tier {
            builder = builder.allow(acl_provider.tier_acl(tier).await?);
        }

        let mut allowlisted_identities = MononokeIdentitySet::new();
        for Identity { id_type, id_data } in &common_config.trusted_parties_allowlist {
            allowlisted_identities
                .insert(MononokeIdentity::from_legacy_type_data(id_type, id_data));
        }
        if !allowlisted_identities.is_empty() {
            builder = builder.allow_allowlist(allowlisted_identities);
        }

        Ok(Self {
            checker: builder.build(),
        })
    }

    /// Check if the given identities are trusted to act as a proxy, and
    /// provide the identities of the originator of the request.
    pub async fn check_if_trusted(&self, identities: &MononokeIdentitySet) -> bool {
        self.checker
            .check_set(identities, &["trusted_parties"])
            .await
    }
}

/// The identities to check a request forwarded by a trusted party as: the
/// forwarded identities plus the request's CAT identities, except CATs that
/// restate the trusted party's own TLS identities. Counting those would give
/// every forwarded request the forwarder's permissions. `server` is the
/// killswitch's switch value.
pub fn forwarded_request_identities(
    forwarded: MononokeIdentitySet,
    cats: &MononokeIdentitySet,
    forwarder: &MononokeIdentitySet,
    server: &str,
) -> MononokeIdentitySet {
    let keep_forwarder_cats = justknobs::eval(
        "scm/mononoke:identity_forwarding_disable_forwarder_cat_filter",
        None,
        Some(server),
    );
    let mut identities = forwarded;
    identities.extend(
        cats.iter()
            .filter(|identity| keep_forwarder_cats || !forwarder.contains(identity))
            .cloned(),
    );
    identities
}

#[cfg(test)]
mod tests {
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs;
    use maplit::hashmap;
    use mononoke_macros::mononoke;

    use super::*;

    const KILLSWITCH: &str = "scm/mononoke:identity_forwarding_disable_forwarder_cat_filter";

    fn identities(ids: &[(&str, &str)]) -> MononokeIdentitySet {
        ids.iter()
            .map(|&(id_type, id_data)| MononokeIdentity::from_legacy_type_data(id_type, id_data))
            .collect()
    }

    fn forwarded_with_killswitch(killed: bool) -> MononokeIdentitySet {
        let forwarder = identities(&[("SERVICE_IDENTITY", "proxy"), ("MACHINE", "proxy-host")]);
        let cats = identities(&[
            ("SERVICE_IDENTITY", "proxy"),
            ("MACHINE", "proxy-host"),
            ("USER", "alice"),
        ]);
        with_just_knobs(
            JustKnobsInMemory::new(hashmap![KILLSWITCH.to_string() => KnobVal::Bool(killed)]),
            || {
                forwarded_request_identities(
                    identities(&[("DATA_PROJECT", "caller")]),
                    &cats,
                    &forwarder,
                    "scs",
                )
            },
        )
    }

    #[mononoke::test]
    fn test_drops_cats_restating_the_forwarder() {
        assert_eq!(
            forwarded_with_killswitch(false),
            identities(&[("DATA_PROJECT", "caller"), ("USER", "alice")]),
        );
    }

    #[mononoke::test]
    fn test_killswitch_keeps_forwarder_cats() {
        assert_eq!(
            forwarded_with_killswitch(true),
            identities(&[
                ("DATA_PROJECT", "caller"),
                ("MACHINE", "proxy-host"),
                ("SERVICE_IDENTITY", "proxy"),
                ("USER", "alice"),
            ]),
        );
    }

    #[mononoke::test]
    fn test_keeps_forwarded_identity_shared_with_forwarder() {
        let forwarder = identities(&[("SERVICE_IDENTITY", "proxy")]);
        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![KILLSWITCH.to_string() => KnobVal::Bool(false)]),
            || {
                forwarded_request_identities(
                    identities(&[("SERVICE_IDENTITY", "proxy")]),
                    &forwarder,
                    &forwarder,
                    "scs",
                )
            },
        );
        assert_eq!(result, forwarder);
    }
}
