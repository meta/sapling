/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use fbinit::FacebookInit;
use http::HeaderMap;
#[cfg(fbcode_build)]
use http::HeaderValue;
use metaconfig_types::Identity;
use permission_checker::MononokeIdentitySet;

static TEST_MODE: AtomicBool = AtomicBool::new(false);

/// Enable test mode for CAT verification. Adds `EnvironmentType::TEST` to the
/// set of environments accepted by `try_get_cats_idents`, and flips cryptocat
/// itself into test mode so tokens minted by an in-process test keychain
/// (which stamp `EnvironmentType::TEST`) verify locally.
///
/// Must only be called from test binaries / integration test entry points.
pub fn enable_test_mode() {
    TEST_MODE.store(true, Ordering::Relaxed);
    #[cfg(fbcode_build)]
    cryptocat::enable_test_mode();
}

pub fn try_get_cats_idents(
    fb: FacebookInit,
    headers: &HeaderMap,
    verifier_identity: &Identity,
) -> Option<MononokeIdentitySet> {
    catmod::try_get_cats_idents_impl(fb, headers, verifier_identity)
}

/// Parse a serialized CAT list without verifying any token. Returns the signer
/// identity of every token (plus the certified identity of delegated tokens)
/// and the set of verifier identities the tokens declare. Never use for authorization.
pub fn unverified_identities_from_serialized_list(
    s: &str,
) -> anyhow::Result<(MononokeIdentitySet, MononokeIdentitySet)> {
    catmod::unverified_identities_from_serialized_list_impl(s)
}

#[cfg(not(fbcode_build))]
mod catmod {
    use super::*;

    pub fn try_get_cats_idents_impl(
        _fb: FacebookInit,
        _headers: &HeaderMap,
        _verifier_identity: &Identity,
    ) -> Option<MononokeIdentitySet> {
        None
    }

    pub fn unverified_identities_from_serialized_list_impl(
        _s: &str,
    ) -> anyhow::Result<(MononokeIdentitySet, MononokeIdentitySet)> {
        Ok((MononokeIdentitySet::new(), MononokeIdentitySet::new()))
    }
}

#[cfg(fbcode_build)]
mod catmod {
    use anyhow::Error;
    use cat_common_thrift::CryptoAuthTokenType;
    use cats_constants::X_AUTH_CATS_HEADER;
    use login_objects_thrift::EnvironmentType;
    use tracing::debug;
    use tracing::trace;
    use tracing::warn;

    use super::*;

    /// Extract identities from CAT tokens in the request headers.
    ///
    /// Returns `None` when no CAT header is present or when the header itself is
    /// malformed (e.g. invalid base64). When the header parses, returns
    /// `Some(set)` containing the identities of every token that successfully
    /// verified — invalid tokens are silently dropped.
    ///
    /// The resulting identities wrap the full `AuthenticatedIdentity` thrift
    /// struct (including attributes extracted from the verified token's
    /// `metaIdUri`, matching srserver's `authenticated_identities_cats_struct`
    /// path).
    pub fn try_get_cats_idents_impl(
        fb: FacebookInit,
        headers: &HeaderMap,
        verifier_identity: &Identity,
    ) -> Option<MononokeIdentitySet> {
        let mut envs = vec![EnvironmentType::PROD, EnvironmentType::CORP];
        if TEST_MODE.load(Ordering::Relaxed) {
            envs.push(EnvironmentType::TEST);
        }
        let idents = try_get_cats_idents_impl_with_envs(fb, headers, verifier_identity, envs);
        trace!("CAT extraction: extracted identities: {idents:?}");
        idents
    }

    fn try_get_cats_idents_impl_with_envs(
        fb: FacebookInit,
        headers: &HeaderMap,
        verifier_identity: &Identity,
        allowed_environments: Vec<EnvironmentType>,
    ) -> Option<MononokeIdentitySet> {
        let Some(cats) = headers.get(X_AUTH_CATS_HEADER) else {
            debug!("CAT extraction: no {} header present", X_AUTH_CATS_HEADER);
            return None;
        };

        match parse_cat_token_list(cats) {
            Ok(None) => None,
            Ok(Some(cat_list)) => Some(verify_cat_tokens(
                fb,
                cat_list,
                verifier_identity,
                allowed_environments,
            )),
            Err(e) => {
                warn!(
                    "Error extracting CATs identities: {:?} Error: {}. Ignoring CAT token.",
                    cats, e
                );
                None
            }
        }
    }

    pub fn unverified_identities_from_serialized_list_impl(
        s: &str,
    ) -> anyhow::Result<(MononokeIdentitySet, MononokeIdentitySet)> {
        let to_identity = |id: cryptocat::Identity| {
            permission_checker::MononokeIdentity::from_legacy_type_data(id.id_type, id.id_data)
        };
        let mut identities = MononokeIdentitySet::new();
        let mut token_verifiers = MononokeIdentitySet::new();
        for token in &cryptocat::deserialize_crypto_auth_tokens(s)?.tokens {
            let data = cryptocat::deserialize_crypto_auth_token_data(
                &token.serializedCryptoAuthTokenData,
            )?;
            // A delegated token also carries the identity it certifies; its secret is never
            // read. A malformed one fails the whole parse like any other malformed token.
            if data.cryptoAuthTokenType == Some(CryptoAuthTokenType::DELEGATED_CRYPTO_AUTH_TOKEN) {
                let info = cryptocat::get_delegated_cat_info(token)?;
                identities.insert(to_identity(info.certified_identity));
            }
            identities.insert(to_identity(data.signerIdentity));
            token_verifiers.insert(to_identity(data.verifierIdentity));
        }
        Ok((identities, token_verifiers))
    }

    fn parse_cat_token_list(
        cats: &HeaderValue,
    ) -> Result<Option<cryptocat::CryptoAuthTokenList>, Error> {
        let s_cats = cats.to_str()?;
        trace!("CAT extraction: serialized CAT list: {s_cats}");
        let cat_list = cryptocat::deserialize_crypto_auth_tokens(s_cats)?;
        debug!(
            "CAT extraction: received {} token(s) in {} header",
            cat_list.tokens.len(),
            X_AUTH_CATS_HEADER,
        );
        Ok(Some(cat_list))
    }

    fn verify_cat_tokens(
        fb: FacebookInit,
        cat_list: cryptocat::CryptoAuthTokenList,
        verifier_identity: &Identity,
        allowed_environments: Vec<EnvironmentType>,
    ) -> MononokeIdentitySet {
        let svc_scm_ident = cryptocat::Identity {
            id_type: verifier_identity.id_type.clone(),
            id_data: verifier_identity.id_data.clone(),
            ..Default::default()
        };

        debug!(
            "CAT extraction: bulk-verifying {} token(s) via authenticated_identity path, against {:?} verifier",
            cat_list.tokens.len(),
            svc_scm_ident,
        );
        match cryptocat::verify_and_extract_authenticated_identities(
            fb,
            cat_list,
            &svc_scm_ident,
            None,
            allowed_environments,
        ) {
            Ok(idents) => {
                let ext_idents: MononokeIdentitySet = idents
                    .into_iter()
                    .map(|auth_id| {
                        debug!(
                            "CAT extraction: extracted identity {}:{}",
                            auth_id.identity.id_type, auth_id.identity.id_data,
                        );
                        permission_checker::MononokeIdentity::from(auth_id)
                    })
                    .collect();
                debug!(
                    "CAT extraction: bulk-verified {} token(s)",
                    ext_idents.len(),
                );
                ext_idents
            }
            Err(e) => {
                warn!(
                    "CAT extraction: bulk verify failed: {}. Returning empty set.",
                    e
                );
                MononokeIdentitySet::new()
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::OnceLock;
        use std::thread;
        use std::time::Duration;

        use cryptocat::CATOptionsBuilder;
        use cryptocat::CryptoAuthToken;
        use cryptocat::CryptoAuthTokenList;
        use cryptocat::auth_consts;
        use http::HeaderValue;
        use mononoke_macros::mononoke;

        use super::*;

        // Cryptocat's test mode is a process-global singleton. Enable it once and
        // never disable, so parallel tests in this crate see a stable test keychain.
        fn ensure_test_mode() {
            static INIT: OnceLock<()> = OnceLock::new();
            INIT.get_or_init(|| {
                cryptocat::enable_test_mode();
            });
        }

        fn signer(name: &str) -> cryptocat::Identity {
            cryptocat::Identity {
                id_type: auth_consts::USER.to_string(),
                id_data: name.to_string(),
                ..Default::default()
            }
        }

        fn cat_verifier(name: &str) -> cryptocat::Identity {
            cryptocat::Identity {
                id_type: auth_consts::SERVICE_IDENTITY.to_string(),
                id_data: name.to_string(),
                ..Default::default()
            }
        }

        fn server_verifier(name: &str) -> Identity {
            Identity {
                id_type: auth_consts::SERVICE_IDENTITY.to_string(),
                id_data: name.to_string(),
            }
        }

        fn mint(
            fb: FacebookInit,
            signer: &cryptocat::Identity,
            verifier: &cryptocat::Identity,
            token_timeout: Option<Duration>,
        ) -> CryptoAuthToken {
            let mut builder = CATOptionsBuilder::default();
            if let Some(t) = token_timeout {
                builder.token_timeout(t);
            }
            cryptocat::get_crypto_auth_token(fb, signer, verifier, builder.build().unwrap())
                .expect("failed to mint test CAT")
        }

        fn header(tokens: Vec<CryptoAuthToken>) -> HeaderMap {
            let list = CryptoAuthTokenList {
                tokens,
                ..Default::default()
            };
            let s = cryptocat::serialize_crypto_auth_tokens(&list)
                .expect("failed to serialize CAT list");
            let mut h = HeaderMap::new();
            h.insert(
                X_AUTH_CATS_HEADER,
                HeaderValue::from_str(&s).expect("serialized CAT list is not valid header"),
            );
            h
        }

        #[mononoke::fbinit_test]
        fn no_header_returns_none(fb: FacebookInit) {
            let result = try_get_cats_idents_impl_with_envs(
                fb,
                &HeaderMap::new(),
                &server_verifier("scm.test"),
                vec![EnvironmentType::TEST],
            );
            assert_eq!(result, None);
        }

        #[mononoke::fbinit_test]
        fn unparseable_header_returns_none(fb: FacebookInit) {
            // Valid base64 but not a serialized CryptoAuthTokenList — same shape as
            // the failure tested by `tests/integration/facebook/test-cat-auth.t`.
            let mut headers = HeaderMap::new();
            headers.insert(X_AUTH_CATS_HEADER, HeaderValue::from_static("12345=="));
            let result = try_get_cats_idents_impl_with_envs(
                fb,
                &headers,
                &server_verifier("scm.test"),
                vec![EnvironmentType::TEST],
            );
            assert_eq!(result, None);
        }

        #[mononoke::fbinit_test]
        fn verifier_mismatch_drops_token(fb: FacebookInit) {
            ensure_test_mode();
            let token = mint(fb, &signer("alice"), &cat_verifier("scm.other"), None);
            let result = try_get_cats_idents_impl_with_envs(
                fb,
                &header(vec![token]),
                &server_verifier("scm.test"),
                vec![EnvironmentType::TEST],
            )
            .expect("header was present, expected Some");
            assert!(result.is_empty(), "expected dropped token, got {result:?}");
        }

        #[mononoke::fbinit_test]
        fn expired_token_dropped(fb: FacebookInit) {
            ensure_test_mode();
            let verifier_name = "scm.test";
            let token = mint(
                fb,
                &signer("alice"),
                &cat_verifier(verifier_name),
                Some(Duration::from_secs(1)),
            );
            // Cryptocat token timeouts have second-level granularity; sleep past the
            // expiry so verification rejects this token.
            thread::sleep(Duration::from_secs(2));
            let result = try_get_cats_idents_impl_with_envs(
                fb,
                &header(vec![token]),
                &server_verifier(verifier_name),
                vec![EnvironmentType::TEST],
            )
            .expect("header was present, expected Some");
            assert!(result.is_empty(), "expected dropped token, got {result:?}");
        }

        #[mononoke::fbinit_test]
        fn mix_valid_and_invalid_returns_only_valid(fb: FacebookInit) {
            ensure_test_mode();
            let verifier_name = "scm.test";
            let valid = mint(fb, &signer("alice"), &cat_verifier(verifier_name), None);
            let wrong_verifier = mint(fb, &signer("bob"), &cat_verifier("scm.other"), None);
            // Mint two short-lived tokens before the sleep so they actually expire
            // by the time we call verify.
            let expired_right_verifier = mint(
                fb,
                &signer("carol"),
                &cat_verifier(verifier_name),
                Some(Duration::from_secs(1)),
            );
            let expired_wrong_verifier = mint(
                fb,
                &signer("dave"),
                &cat_verifier("scm.other"),
                Some(Duration::from_secs(1)),
            );
            thread::sleep(Duration::from_secs(2));
            let result = try_get_cats_idents_impl_with_envs(
                fb,
                &header(vec![
                    valid,
                    wrong_verifier,
                    expired_right_verifier,
                    expired_wrong_verifier,
                ]),
                &server_verifier(verifier_name),
                vec![EnvironmentType::TEST],
            )
            .expect("header was present, expected Some");
            assert_eq!(
                result.len(),
                1,
                "verification of bad tokens must not drop the valid one, got {result:?}"
            );
            let only = result.iter().next().unwrap();
            assert_eq!(only.id_type(), auth_consts::USER);
            assert_eq!(only.id_data(), "alice");
        }
    }
}
