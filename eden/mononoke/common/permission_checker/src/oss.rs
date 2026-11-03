/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Stub implementation of AuthenticatedIdentity for OSS builds.
//! This provides the minimal structure needed for the MononokeIdentity enum
//! to compile in OSS mode, but the functionality is not available.

use anyhow::Result;
use anyhow::bail;
#[cfg(fbcode_build)]
use async_trait::async_trait;
use itertools::Itertools;
use openssl::x509::X509;

use crate::identity::ClientCategory;
use crate::identity::MononokeIdentity;
use crate::identity::MononokeIdentitySet;
use crate::identity::MononokeIdentitySetExt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedIdentity {
    pub identity: Identity,
    pub attributes: Vec<Attribute>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub id_type: String,
    pub id_data: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    pub identifier: AttributeKey,
    pub value: IdentityAttribute,
    pub val: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    non_snake_case,
    reason = "Field names match the internal identity Thrift structs that shared code builds"
)]
pub struct AttributeKey {
    pub attributeName: String,
    pub attributeNamespace: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    non_snake_case,
    reason = "Field names match the internal identity Thrift structs that shared code builds"
)]
pub struct IdentityAttribute {
    pub attributeValue: String,
}

impl MononokeIdentity {
    pub fn reviewer_identities(_username: &str) -> MononokeIdentitySet {
        MononokeIdentitySet::new()
    }

    pub fn try_from_json_encoded(_: &str) -> Result<MononokeIdentitySet> {
        bail!("Decoding from JSON is not yet implemented for MononokeIdentity")
    }

    pub fn try_from_legacy_encoded(_: &str) -> Result<MononokeIdentitySet> {
        bail!(
            "Decoding legacy scm_forwarded_identities is not yet implemented for MononokeIdentity"
        )
    }

    pub fn try_from_thrift_compact_bytes(_: &[u8]) -> Result<MononokeIdentitySet> {
        bail!(
            "Decoding compact-encoded AuthenticatedIdentity is not supported for MononokeIdentity in OSS builds"
        )
    }

    pub fn try_from_x509(cert: &X509) -> Result<MononokeIdentitySet> {
        let subject_vec: Result<Vec<_>> = cert
            .subject_name()
            .entries()
            .map(|entry| {
                Ok(format!(
                    "{}={}",
                    entry.object().nid().short_name()?,
                    entry.data().as_utf8()?
                ))
            })
            .collect();
        let subject_name = subject_vec?.as_slice().join(",");

        let mut idents = MononokeIdentitySet::new();
        idents.insert(MononokeIdentity::from_legacy_type_data(
            "X509_SUBJECT_NAME",
            subject_name,
        ));
        Ok(idents)
    }
}

impl MononokeIdentitySetExt for MononokeIdentitySet {
    fn likely_an_agent(&self) -> bool {
        false
    }

    fn is_proxygen_test_identity(&self) -> bool {
        false
    }

    fn hostprefix(&self) -> Option<&str> {
        None
    }

    fn hostname(&self) -> Option<&str> {
        None
    }

    fn username(&self) -> Option<&str> {
        None
    }

    fn crewmate(&self) -> Option<&str> {
        None
    }

    fn sandcastle_job_id(&self) -> Option<&str> {
        None
    }

    fn on_demand_type(&self) -> Option<&str> {
        None
    }

    fn main_client_identity(&self, _sandcastle_alias: Option<&str>) -> String {
        String::from("PLACEHOLDER_CLIENT_IDENTITY")
    }

    fn client_category(&self, _sandcastle_alias: Option<&str>) -> ClientCategory {
        ClientCategory::Unknown
    }

    fn to_string(&self) -> String {
        self.iter().map(ToString::to_string).join(",")
    }

    fn identity_type_filtered_concat(&self, _id_type: &str) -> Option<String> {
        None
    }
}
