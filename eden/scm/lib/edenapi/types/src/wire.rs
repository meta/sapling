/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! This module contains wire representation structs for external types we'd
//! like to avoid explicitly depending on. Types will be added as they are
//! used.
//!
//! These types should all be `pub(crate)`. They're used extensively inside the
//! crate, but should never appear outside it. The methods on the request /
//! response objects should accept and return the public types from
//! `eden/scm/lib/types`.
//!
//! To maintain wire-protocol compatibility, we have some important conventions
//! and requirements for all types defined inside this module:
//!
//! 1. Every field should be renamed to a unique natural number using
//!    `#[serde(rename = "0")]`. New fields should never reuse a field identifier
//!    that has been used before. If a field changes semantically, it should be
//!    considered a new field, and be given a new identifier.
//!
//! 2. Every enum should have an "Unknown" variant as the last variant in the
//!    enum. This variant should be annotated with `#[serde(other, rename = "0")]`
//!
//! 3. When practical, fields should be annotated with
//!    `#[serde(default, skip_serializing_if = "is_default")` to save space on the
//!    wire. Do not use `#[serde(default)]` on the container.
//!
//! 4. All fields should be wrapped in `Option` or in a container that may be
//!    empty, such as `Vec`. If an empty container has special semantics (other
//!    than ignoring the field), please wrap that field in an `Option` as well to
//!    distinguish between "empty" and "not present".
//!
//! Things to update when making a change to a wire type:
//!
//! 1. The Wire type definition.
//! 2. If applicable, the API type definition.
//! 3. The `ToWire` and `ToApi` implementations for the wire type.
//! 4. If the API type has changed, the `json` module.
//! 5. The `Arbitrary` implementations for the modified types.
//! 6. If a new type is introduced, add a quickcheck serialize round trip test.
//! 7. If the type has a corresponding API type, add a quickcheck wire-API round
//!    trip test.

#[macro_use]
pub mod hash;

pub mod anyid;
pub mod batch;
pub mod bookmark;
pub mod clone;
pub mod commit;
pub mod errors;
pub mod file;
pub mod history;
pub mod land;
pub mod metadata;
pub mod suffix_query;
#[cfg(test)]
pub(crate) mod tests;
pub mod token;
pub mod tree;

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt;
use std::num::NonZeroU64;

use dag_types::id::Id as DagId;
#[cfg(any(test, feature = "for-tests"))]
use quickcheck::Arbitrary;
#[cfg(any(test, feature = "for-tests"))]
use quickcheck_arbitrary_derive::Arbitrary;
use revisionstore_types::Metadata as RevisionstoreMetadata;
use serde_derive::Deserialize;
use serde_derive::Serialize;
use thiserror::Error;
use types::HgId;
use types::Key;
use types::Parents;
use types::RepoPathBuf;
use types::path::ParseError as RepoPathParseError;

use crate::SaplingRemoteApiServerErrorKind;
pub use crate::wire::anyid::WireAnyId;
pub use crate::wire::anyid::WireLookupRequest;
pub use crate::wire::anyid::WireLookupResponse;
pub use crate::wire::anyid::WireLookupResult;
pub use crate::wire::batch::WireBatch;
pub use crate::wire::bookmark::WireBookmarkEntry;
pub use crate::wire::bookmark::WireBookmarkRequest;
pub use crate::wire::bookmark::WireSetBookmarkRequest;
pub use crate::wire::clone::WireCloneData;
pub use crate::wire::clone::WireIdMapEntry;
pub use crate::wire::commit::WireCommitGraphEntry;
pub use crate::wire::commit::WireCommitGraphRequest;
pub use crate::wire::commit::WireCommitHashLookupRequest;
pub use crate::wire::commit::WireCommitHashLookupResponse;
pub use crate::wire::commit::WireCommitHashToLocationRequestBatch;
pub use crate::wire::commit::WireCommitHashToLocationResponse;
pub use crate::wire::commit::WireCommitLocation;
pub use crate::wire::commit::WireCommitLocationToHashRequest;
pub use crate::wire::commit::WireCommitLocationToHashRequestBatch;
pub use crate::wire::commit::WireCommitLocationToHashResponse;
pub use crate::wire::commit::WireEphemeralExtendRequest;
pub use crate::wire::commit::WireEphemeralExtendResponse;
pub use crate::wire::commit::WireEphemeralPrepareRequest;
pub use crate::wire::commit::WireEphemeralPrepareResponse;
pub use crate::wire::commit::WireExtendBubbleTtlOutcome;
pub use crate::wire::commit::WireExtra;
pub use crate::wire::commit::WireFetchSnapshotRequest;
pub use crate::wire::commit::WireFetchSnapshotResponse;
pub use crate::wire::commit::WireHgChangesetContent;
pub use crate::wire::commit::WireHgMutationEntryContent;
pub use crate::wire::commit::WireUploadBonsaiChangesetRequest;
pub use crate::wire::commit::WireUploadHgChangeset;
pub use crate::wire::commit::WireUploadHgChangesetsRequest;
pub use crate::wire::commit::WireUploadHgSnapshotRequest;
pub use crate::wire::commit::WireUploadHgSnapshotResponse;
pub use crate::wire::errors::WireError;
pub use crate::wire::errors::WireResult;
pub use crate::wire::file::WireFileEntry;
pub use crate::wire::file::WireFileRequest;
pub use crate::wire::file::WireUploadHgFilenodeRequest;
pub use crate::wire::file::WireUploadTokensResponse;
pub use crate::wire::history::WireHistoryRequest;
pub use crate::wire::history::WireHistoryResponseChunk;
pub use crate::wire::history::WireWireHistoryEntry;
pub use crate::wire::land::WireLandStackRequest;
pub use crate::wire::land::WireLandStackResponse;
pub use crate::wire::land::WirePushVar;
pub use crate::wire::metadata::WireAnyFileContentId;
pub use crate::wire::metadata::WireBlake3;
pub use crate::wire::metadata::WireContentId;
pub use crate::wire::metadata::WireFileMetadata;
pub use crate::wire::metadata::WireFileType;
pub use crate::wire::metadata::WireSha1;
pub use crate::wire::metadata::WireSha256;
pub use crate::wire::metadata::WireTreeAuxData;
pub use crate::wire::suffix_query::WireSuffixQueryRequest;
pub use crate::wire::suffix_query::WireSuffixQueryResponse;
pub use crate::wire::token::WireUploadToken;
pub use crate::wire::token::WireUploadTokenData;
pub use crate::wire::token::WireUploadTokenSignature;
pub use crate::wire::tree::WireTreeEntry;
pub use crate::wire::tree::WireTreeRequest;
pub use crate::wire::tree::WireUploadTreeEntry;
pub use crate::wire::tree::WireUploadTreeRequest;
pub use crate::wire::tree::WireUploadTreeResponse;

#[derive(Copy, Clone, Debug, Error)]
#[error("invalid byte slice length, expected {expected_len} found {found_len}")]
pub struct TryFromBytesError {
    pub expected_len: usize,
    pub found_len: usize,
}

#[derive(Debug, Error)]
#[error("Failed to convert from wire to API representation")]
pub enum WireToApiConversionError {
    UnrecognizedEnumVariant(&'static str),
    CannotPopulateRequiredField(&'static str),
    PathValidationError(RepoPathParseError),
    InvalidUploadTokenType(&'static str),
    MissingField(&'static str),
}

impl From<Infallible> for WireToApiConversionError {
    fn from(v: Infallible) -> Self {
        match v {}
    }
}

impl From<RepoPathParseError> for WireToApiConversionError {
    fn from(v: RepoPathParseError) -> Self {
        WireToApiConversionError::PathValidationError(v)
    }
}

/// Convert from an SaplingRemoteAPI API type to Wire type
pub trait ToWire: Sized {
    type Wire: ToApi<Api = Self> + serde::Serialize + serde::de::DeserializeOwned;

    fn to_wire(self) -> Self::Wire;
}

/// Convert from an SaplingRemoteAPI Wire type to API type
pub trait ToApi: Send + Sized {
    type Api: ToWire<Wire = Self>;
    type Error: Into<WireToApiConversionError> + Send + Sync + std::error::Error;

    fn to_api(self) -> Result<Self::Api, Self::Error>;
}

impl<A: ToWire> ToWire for Vec<A> {
    type Wire = Vec<<A as ToWire>::Wire>;

    fn to_wire(self) -> Self::Wire {
        let mut out = Vec::with_capacity(self.len());
        for v in self.into_iter() {
            out.push(v.to_wire())
        }
        out
    }
}

impl<W: ToApi> ToApi for Vec<W> {
    type Api = Vec<<W as ToApi>::Api>;
    type Error = <W as ToApi>::Error;

    fn to_api(self) -> Result<Self::Api, Self::Error> {
        let mut out = Vec::with_capacity(self.len());
        for v in self.into_iter() {
            out.push(v.to_api()?)
        }
        Ok(out)
    }
}

// if needed, use macros to implement for more tuples
impl<A: ToWire, B: ToWire> ToWire for (A, B) {
    type Wire = (<A as ToWire>::Wire, <B as ToWire>::Wire);

    fn to_wire(self) -> Self::Wire {
        (self.0.to_wire(), self.1.to_wire())
    }
}

impl<A: ToApi, B: ToApi> ToApi for (A, B) {
    type Api = (<A as ToApi>::Api, <B as ToApi>::Api);
    type Error = WireToApiConversionError;

    fn to_api(self) -> Result<Self::Api, Self::Error> {
        Ok((
            self.0.to_api().map_err(|e| e.into())?,
            self.1.to_api().map_err(|e| e.into())?,
        ))
    }
}

impl<A: ToWire> ToWire for Option<A> {
    type Wire = Option<<A as ToWire>::Wire>;

    fn to_wire(self) -> Self::Wire {
        self.map(|a| a.to_wire())
    }
}

impl<W: ToApi> ToApi for Option<W> {
    type Api = Option<<W as ToApi>::Api>;
    type Error = <W as ToApi>::Error;
    fn to_api(self) -> Result<Self::Api, Self::Error> {
        self.map(|w| w.to_api()).transpose()
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WireMap<K, V>(Vec<(K, V)>);

// This is a bit more restrictive than usual hashmap, as we require Ord
// That is because in tests we want the order of keys to be consistent
impl<K: ToWire + Eq + std::hash::Hash + Ord, V: ToWire> ToWire for HashMap<K, V> {
    type Wire = WireMap<<K as ToWire>::Wire, <V as ToWire>::Wire>;

    fn to_wire(self) -> Self::Wire {
        let iter = self.into_iter();
        #[cfg(test)]
        let iter = std::collections::BTreeMap::from_iter(iter).into_iter();
        WireMap(iter.map(|(k, v)| (k.to_wire(), v.to_wire())).collect())
    }
}

impl<K: ToApi, V: ToApi> ToApi for WireMap<K, V>
where
    <K as ToApi>::Api: Eq + std::hash::Hash + Ord,
{
    type Api = HashMap<<K as ToApi>::Api, <V as ToApi>::Api>;
    type Error = WireToApiConversionError;

    fn to_api(self) -> Result<Self::Api, Self::Error> {
        self.0
            .into_iter()
            .map(|(k, v)| {
                Ok((
                    k.to_api().map_err(|e| e.into())?,
                    v.to_api().map_err(|e| e.into())?,
                ))
            })
            .collect()
    }
}

// This allows using these objects as pure requests and responses
// Only use it for very simple objects which serializations don't
// incur extra costs
macro_rules! transparent_wire {
    ( $($name: ty),* $(,)? ) => {
        $(
        impl ToWire for $name {
            type Wire = $name;

            fn to_wire(self) -> Self::Wire {
                self
            }
    }

        impl ToApi for $name {
            type Api = $name;
            type Error = std::convert::Infallible;

            fn to_api(self) -> Result<Self::Api, Self::Error> {
                Ok(self)
            }
    }
     )*
    }
}

transparent_wire!(
    bool,
    u8,
    i8,
    u16,
    i16,
    u32,
    i32,
    u64,
    i64,
    usize,
    isize,
    bytes::Bytes,
    minibytes::Bytes,
    String,
    (),
);

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WrapNonZero<T>(T);

impl ToWire for NonZeroU64 {
    type Wire = WrapNonZero<u64>;

    fn to_wire(self) -> Self::Wire {
        WrapNonZero(self.get())
    }
}

impl ToApi for WrapNonZero<u64> {
    type Api = NonZeroU64;
    type Error = WireToApiConversionError;

    fn to_api(self) -> Result<Self::Api, Self::Error> {
        match NonZeroU64::new(self.0) {
            Some(val) => Ok(val),
            None => Err(WireToApiConversionError::CannotPopulateRequiredField(
                "Invalid value provided for NonZeroU64",
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub enum WireSaplingRemoteApiServerError {
    #[serde(rename = "1")]
    OpaqueError(String),

    #[serde(rename = "2")]
    PermissionDenied {
        tree_id: WireHgId,
        request_acl: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        denial_message: Option<String>,
    },

    #[serde(other, rename = "0")]
    Unknown,
}

impl ToWire for SaplingRemoteApiServerErrorKind {
    type Wire = WireSaplingRemoteApiServerError;

    fn to_wire(self) -> Self::Wire {
        use SaplingRemoteApiServerErrorKind::*;
        match self {
            OpaqueError(s) => WireSaplingRemoteApiServerError::OpaqueError(s),
            PermissionDenied {
                tree_id,
                request_acl,
                denial_message,
            } => WireSaplingRemoteApiServerError::PermissionDenied {
                tree_id: tree_id.to_wire(),
                request_acl,
                denial_message,
            },
        }
    }
}

impl ToApi for WireSaplingRemoteApiServerError {
    type Api = SaplingRemoteApiServerErrorKind;
    type Error = WireToApiConversionError;

    fn to_api(self) -> Result<Self::Api, Self::Error> {
        use WireSaplingRemoteApiServerError::*;
        Ok(match self {
            Unknown => {
                return Err(WireToApiConversionError::UnrecognizedEnumVariant(
                    "WireSaplingRemoteApiServerError",
                ));
            }
            OpaqueError(s) => SaplingRemoteApiServerErrorKind::OpaqueError(s),
            PermissionDenied {
                tree_id,
                request_acl,
                denial_message,
            } => SaplingRemoteApiServerErrorKind::PermissionDenied {
                tree_id: tree_id.to_api()?,
                request_acl,
                denial_message,
            },
        })
    }
}

wire_hash! {
    wire => WireHgId,
    api  => HgId,
    size => 20,
}

impl fmt::Display for WireHgId {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self.to_api() {
            Ok(api) => fmt::Display::fmt(&api, fmt),
            Err(e) => match e {},
        }
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRepoPathBuf(
    #[serde(rename = "0", default, skip_serializing_if = "is_default")] String,
);

impl ToWire for RepoPathBuf {
    type Wire = WireRepoPathBuf;

    fn to_wire(self) -> Self::Wire {
        WireRepoPathBuf(self.into_string())
    }
}

impl ToApi for WireRepoPathBuf {
    type Api = RepoPathBuf;
    type Error = RepoPathParseError;

    fn to_api(self) -> Result<Self::Api, Self::Error> {
        RepoPathBuf::from_string(self.0)
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireKey {
    #[serde(rename = "0", default, skip_serializing_if = "is_default")]
    path: WireRepoPathBuf,

    #[serde(rename = "1", default, skip_serializing_if = "is_default")]
    hgid: WireHgId,
}

impl ToWire for Key {
    type Wire = WireKey;

    fn to_wire(self) -> Self::Wire {
        WireKey {
            path: self.path.to_wire(),
            hgid: self.hgid.to_wire(),
        }
    }
}

impl ToApi for WireKey {
    type Api = Key;
    type Error = WireToApiConversionError;

    fn to_api(self) -> Result<Self::Api, Self::Error> {
        Ok(Key {
            path: self.path.to_api()?,
            hgid: self.hgid.to_api()?,
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
pub enum WireParents {
    #[serde(rename = "1")]
    #[default]
    None,

    #[serde(rename = "2")]
    One(WireHgId),

    #[serde(rename = "3")]
    Two(WireHgId, WireHgId),

    #[serde(other, rename = "0")]
    Unknown,
}

impl ToWire for Parents {
    type Wire = WireParents;

    fn to_wire(self) -> Self::Wire {
        use Parents::*;
        match self {
            None => WireParents::None,
            One(id) => WireParents::One(id.to_wire()),
            Two(id1, id2) => WireParents::Two(id1.to_wire(), id2.to_wire()),
        }
    }
}

impl ToApi for WireParents {
    type Api = Parents;
    type Error = WireToApiConversionError;

    fn to_api(self) -> Result<Self::Api, Self::Error> {
        use WireParents::*;
        Ok(match self {
            Unknown => {
                return Err(WireToApiConversionError::UnrecognizedEnumVariant(
                    "WireParents",
                ));
            }
            None => Parents::None,
            One(id) => Parents::One(id.to_api()?),
            Two(id1, id2) => Parents::Two(id1.to_api()?, id2.to_api()?),
        })
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct WireRevisionstoreMetadata {
    #[serde(rename = "0", default, skip_serializing_if = "is_default")]
    size: Option<u64>,

    #[serde(rename = "1", default, skip_serializing_if = "is_default")]
    flags: Option<u64>,
}

impl ToWire for RevisionstoreMetadata {
    type Wire = WireRevisionstoreMetadata;

    fn to_wire(self) -> Self::Wire {
        WireRevisionstoreMetadata {
            size: self.size,
            flags: self.flags,
        }
    }
}

impl ToApi for WireRevisionstoreMetadata {
    type Api = RevisionstoreMetadata;
    type Error = Infallible;

    fn to_api(self) -> Result<Self::Api, Self::Error> {
        Ok(RevisionstoreMetadata {
            size: self.size,
            flags: self.flags,
        })
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Ord, PartialOrd)]
#[derive(Serialize, Deserialize)]
pub struct WireDagId(u64);

impl ToWire for DagId {
    type Wire = WireDagId;

    fn to_wire(self) -> Self::Wire {
        WireDagId(self.0)
    }
}

impl ToApi for WireDagId {
    type Api = DagId;
    type Error = Infallible;

    fn to_api(self) -> Result<Self::Api, Self::Error> {
        Ok(DagId(self.0))
    }
}

pub(crate) fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    v == &T::default()
}

#[cfg(any(test, feature = "for-tests"))]
impl Arbitrary for WireRepoPathBuf {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        RepoPathBuf::arbitrary(g).to_wire()
    }
}

#[cfg(any(test, feature = "for-tests"))]
impl Arbitrary for WireKey {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        Key::arbitrary(g).to_wire()
    }
}

#[cfg(any(test, feature = "for-tests"))]
impl Arbitrary for WireParents {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        Parents::arbitrary(g).to_wire()
    }
}

#[cfg(any(test, feature = "for-tests"))]
impl Arbitrary for WireDagId {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        DagId::arbitrary(g).to_wire()
    }
}

#[cfg(test)]
pub mod local_tests {
    use super::*;
    use crate::wire::tests::wire_json_hashes;

    #[test]
    fn test_wire_json() {
        assert_eq!(
            wire_json_hashes![
                WireHgId,
                WireKey,
                WireRepoPathBuf,
                WireParents,
                WireRevisionstoreMetadata,
                WireSaplingRemoteApiServerError,
                WireDagId,
            ],
            [
                6316582354165972683,
                3066223164220783360,
                9456629424509987447,
                4507767538966787345,
                14380116313707050612,
                15686871036843111287,
                6480712246590099299
            ]
        );
    }
}
