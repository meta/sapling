/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod counted_blobstore;
mod disabled;
pub mod macros;

use std::collections::HashSet;
use std::fmt;
use std::io::Cursor;
use std::ops::Bound;
use std::ops::RangeBounds;
use std::ops::RangeFrom;
use std::ops::RangeFull;
use std::ops::RangeInclusive;
use std::ops::RangeToInclusive;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use async_trait::async_trait;
use auto_impl::auto_impl;
use bytes::Bytes;
use clap::ValueEnum;
use context::CoreContext;
use either::Either;
use futures_watchdog::WatchdogExt;
use serde_derive::Deserialize;
use serde_derive::Serialize;
use strum::AsRefStr;
use strum::Display;
use strum::EnumIter;
use strum::EnumString;
use strum::IntoStaticStr;
use thiserror::Error;
use trait_set::trait_set;

pub use crate::counted_blobstore::CountedBlobstore;
pub use crate::disabled::DisabledBlob;

// This module exists to namespace re-exported
// imports, needed for macro exports.
pub mod private {
    pub use std::convert::TryFrom;
    pub use std::convert::TryInto;

    pub use anyhow::Error;
    pub use async_trait::async_trait;
    pub use context::CoreContext;
    pub use fbthrift::compact_protocol;

    pub use crate::Blobstore;
    pub use crate::BlobstoreBytes;
    pub use crate::BlobstoreGetData;
    pub use crate::KeyedBlobstore;
    pub use crate::Loadable;
    pub use crate::LoadableError;
    pub use crate::Storable;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobstoreGetData {
    meta: BlobstoreMetadata,
    bytes: BlobstoreBytes,
}

const UNCOMPRESSED: u8 = b'0';
const COMPRESSED: u8 = b'1';

pub const BLOBSTORE_MAX_POLL_TIME_MS: u64 = 100;

impl BlobstoreGetData {
    #[inline]
    pub fn new(meta: BlobstoreMetadata, bytes: BlobstoreBytes) -> Self {
        BlobstoreGetData { meta, bytes }
    }

    #[inline]
    pub fn as_meta(&self) -> &BlobstoreMetadata {
        &self.meta
    }

    #[inline]
    pub fn into_bytes(self) -> BlobstoreBytes {
        self.bytes
    }

    #[inline]
    pub fn as_bytes(&self) -> &BlobstoreBytes {
        &self.bytes
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    #[inline]
    pub fn into_raw_bytes(self) -> Bytes {
        self.into_bytes().into_bytes()
    }

    #[inline]
    pub fn as_raw_bytes(&self) -> &Bytes {
        self.as_bytes().as_bytes()
    }

    #[inline]
    pub fn from_bytes<B: Into<Bytes>>(bytes: B) -> Self {
        BlobstoreGetData {
            meta: BlobstoreMetadata::default(),
            bytes: BlobstoreBytes::from_bytes(bytes.into()),
        }
    }

    #[inline]
    pub fn remove_ctime(&mut self) {
        self.meta.ctime = None;
    }
}

impl From<BlobstoreBytes> for BlobstoreGetData {
    fn from(blob_bytes: BlobstoreBytes) -> Self {
        BlobstoreGetData {
            meta: BlobstoreMetadata::default(),
            bytes: blob_bytes,
        }
    }
}

impl From<BlobstoreGetData> for BlobstoreMetadata {
    fn from(blob: BlobstoreGetData) -> Self {
        blob.meta
    }
}

impl From<BlobstoreGetData> for BlobstoreBytes {
    fn from(blob: BlobstoreGetData) -> Self {
        blob.into_bytes()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[derive(bincode::Encode, bincode::Decode)]
pub struct PackMetadata {
    /// Gives an idea what its packed with, if anything
    pub pack_key: String,
    /// How big the overall size of compressed forms was to reach this
    pub relevant_compressed_size: u64,
    /// How big the overall size of uncompressed forms was to reach this
    pub relevant_uncompressed_size: u64,
}

/// Optional information about the size of a value
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[derive(bincode::Encode, bincode::Decode)]
pub struct SizeMetadata {
    /// How much size this value has added to storage on its own.
    /// unique uncompressed size is already available from BlobstoreBytes::len()
    pub unique_compressed_size: u64,
    /// Info about packing, if its packed
    pub pack_meta: Option<PackMetadata>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[derive(bincode::Encode, bincode::Decode)]
pub struct BlobstoreMetadata {
    ctime: Option<i64>,
    sizes: Option<SizeMetadata>,
}

impl BlobstoreMetadata {
    #[inline]
    pub fn new(ctime: Option<i64>, sizes: Option<SizeMetadata>) -> BlobstoreMetadata {
        BlobstoreMetadata { ctime, sizes }
    }

    #[inline]
    pub fn ctime(&self) -> Option<i64> {
        self.ctime
    }

    pub fn sizes(&self) -> Option<&SizeMetadata> {
        self.sizes.as_ref()
    }
}

/// A type representing bytes written to or read from a blobstore. The goal here is to ensure
/// that only types that implement `From<BlobstoreBytes>` and `Into<BlobstoreBytes>` can be
/// stored in the blob store.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlobstoreBytes(Bytes);

impl BlobstoreBytes {
    /// Construct an empty BlobstoreBytes
    pub fn empty() -> Self {
        BlobstoreBytes(Bytes::new())
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// This should only be used by blobstore and `From`/`Into<BlobstoreBytes>` implementations.
    #[inline]
    pub fn from_bytes<B: Into<Bytes>>(bytes: B) -> Self {
        BlobstoreBytes(bytes.into())
    }

    /// This should only be used by blobstore and `From`/`Into<BlobstoreBytes>` implementations.
    #[inline]
    pub fn into_bytes(self) -> Bytes {
        self.0
    }

    /// This should only be used by blobstore and `From`/`Into<BlobstoreBytes>` implementations.
    #[inline]
    pub fn as_bytes(&self) -> &Bytes {
        &self.0
    }

    pub fn encode(self, encode_limit: Option<u64>) -> Option<Bytes> {
        let mut bytes = vec![UNCOMPRESSED];
        bytes.append(&mut self.0.into());

        match encode_limit {
            Some(encode_limit) if bytes.len() as u64 >= encode_limit => {
                let mut compressed = Vec::with_capacity(bytes.len());
                compressed.push(COMPRESSED);

                let mut cursor = Cursor::new(bytes);
                cursor.set_position(1);
                zstd::stream::copy_encode(cursor, &mut compressed, 0 /* use default */).ok()?;
                Some(Bytes::from(compressed))
            }
            _ => Some(Bytes::from(bytes)),
        }
    }

    pub fn decode(mut bytes: Bytes) -> Option<Self> {
        let prefix_size = 1;
        if bytes.len() < prefix_size {
            return None;
        }

        let is_compressed = bytes.split_to(prefix_size);
        if is_compressed[0] == COMPRESSED {
            let cursor = Cursor::new(bytes);
            Some(Self::from_bytes(zstd::decode_all(cursor).ok()?))
        } else {
            Some(Self::from_bytes(bytes))
        }
    }
}

#[derive(Debug)]
pub enum BlobstoreIsPresent {
    // The blob is definitely present in the blobstore
    Present,
    /// The blob is definitely not present
    Absent,
    /// The blobstore has no evidence that the blob is present,
    /// however some of the operations resulted in errors.
    ProbablyNotPresent(Error),
}

impl BlobstoreIsPresent {
    pub fn assume_not_found_if_unsure(self) -> bool {
        match self {
            BlobstoreIsPresent::Present => true,
            BlobstoreIsPresent::Absent => false,
            BlobstoreIsPresent::ProbablyNotPresent(_) => false,
        }
    }

    pub fn fail_if_unsure(self) -> Result<bool, Error> {
        match self {
            BlobstoreIsPresent::Present => Ok(true),
            BlobstoreIsPresent::Absent => Ok(false),
            BlobstoreIsPresent::ProbablyNotPresent(err) => Err(err),
        }
    }
}

/// The blobstore interface, shared across all blobstores.
/// A blobstore must provide the following guarantees:
/// 1. `get` and `put` are atomic with respect to each other; a put will either put the entire
///    value, or not put anything, and a get will return either None, or the entire value that an
///    earlier put inserted.
/// 2. Once the future returned by `put` completes, the data is durably stored. This implies that
///    a permanent failure of the backend will not lose the data unless multiple replicas in the
///    backend are lost. For example, if you have replicas in multiple datacentres, you will
///    not lose data until you lose two or more datacentres. However, losing replicas can make the
///    data inaccessible for a time.
/// 3. Once the future returned by `put` completes, calling `get` from any process will get you a
///    future that will return the data that was saved in the blobstore; this is so that after the
///    `put` completes, Mononoke can update a database table and be confident that all Mononoke
///    instances can `get` the blobs that the database refers to.
///
/// Implementations of this trait can assume that the same value is supplied if two keys are
/// equal - in other words, each key is associated with at most one globally unique value.
/// In other words, `put(key, value).and_then(put(key, value2))` implies `value == value2` for the
/// `BlobstoreBytes` definition of equality. If `value != value2`, then the implementation's
/// behaviour is implementation defined (it can overwrite or not write at all, as long as it does
/// not break the atomicity guarantee, and does not have to be consistent in its behaviour).
///
/// Implementations of Blobstore must be `Clone` if they are to interoperate with other Mononoke
/// uses of Blobstores
#[async_trait]
#[auto_impl(&, Arc, Box)]
pub trait Blobstore: fmt::Display + fmt::Debug + Send + Sync {
    /// Fetch the value associated with `key`, or None if no value is present
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>>;

    /// Adds ability to specify the put behaviour explicitly so that even if per process default was
    /// IfAbsent one could choose to OverwriteAndLog. Expected to be used only in admin tools
    async fn put_explicit<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
        put_behaviour: PutBehaviour,
    ) -> Result<OverwriteStatus>;

    /// Similar to `put`, but returns the OverwriteStatus as feedback rather than unit.
    /// Uses the default put behaviour for this blobstore.
    async fn put_with_status<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<OverwriteStatus>;

    /// Associate `value` with `key` for future gets; if `put` is called with different `value`s
    /// for the same key, the implementation may return any `value` it's been given in response
    /// to a `get` for that `key`.
    /// Default implementation calls put_with_status and discards the status.
    async fn put<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<()> {
        self.put_with_status(ctx, key, value).await?;
        Ok(())
    }

    /// Check that `get` will return a value for a given `key`, and not None. The provided
    /// implementation just calls `get`, and discards the return value; this can be overridden to
    /// avoid transferring data. In the absence of concurrent `put` calls, this must return
    /// `BlobstoreIsPresent::Absent` if `get` would return `None`, and `BlobstoreIsPresent::Present`
    /// if `get` would return `Some(_)`.
    /// In some cases, when it couldn't determine whether the key exists or not, it would
    /// return `BlobstoreIsPresent::ProbablyNotPresent`.
    async fn is_present<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent> {
        Ok(if self.get(ctx, key).await?.is_some() {
            BlobstoreIsPresent::Present
        } else {
            BlobstoreIsPresent::Absent
        })
    }
    /// Copy the value from one key to another. The default behaviour is to `get` and `put` the
    /// value, though some blobstores might have more efficient implementations that avoid
    /// transferring data.
    async fn copy<'a>(
        &'a self,
        ctx: &'a CoreContext,
        old_key: &'a str,
        new_key: String,
    ) -> Result<()> {
        let value = self
            .get(ctx, old_key)
            .await?
            .with_context(|| format!("key {old_key} not present"))?;
        Ok(self.put(ctx, new_key, value.bytes).await?)
    }

    /// Similar to unlink(2), this removes a key, resulting in content being removed if its the last key pointing to it.
    /// An error is returned if the key does not exist
    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()>;
}

#[async_trait]
#[auto_impl(&, Arc, Box)]
pub trait KeyedBlobstore: fmt::Display + fmt::Debug + Send + Sync {
    /// Fetch the value associated with `key`, or None if no value is present
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>>;
    /// Associate `value` with `key` for future gets; if `put` is called with different `value`s
    /// for the same key, the implementation may return any `value` it's been given in response
    /// to a `get` for that `key`.
    async fn put<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<()>;
    /// Check that `get` will return a value for a given `key`, and not None. The provided
    /// implementation just calls `get`, and discards the return value; this can be overridden to
    /// avoid transferring data. In the absence of concurrent `put` calls, this must return
    /// `BlobstoreIsPresent::Absent` if `get` would return `None`, and `BlobstoreIsPresent::Present`
    /// if `get` would return `Some(_)`.
    /// In some cases, when it couldn't determine whether the key exists or not, it would
    /// return `BlobstoreIsPresent::ProbablyNotPresent`.
    async fn is_present<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent>;
    /// Copy the value from one key to another. The default behaviour is to `get` and `put` the
    /// value, though some blobstores might have more efficient implementations that avoid
    /// transferring data.
    async fn copy<'a>(
        &'a self,
        ctx: &'a CoreContext,
        old_key: &'a str,
        new_key: String,
    ) -> Result<()>;

    /// Similar to unlink(2), this removes a key, resulting in content being removed if its the last key pointing to it.
    /// An error is returned if the key does not exist
    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()>;
}

/// Mononoke binaries will not overwrite existing blobstore keys by default
pub const DEFAULT_PUT_BEHAVIOUR: PutBehaviour = PutBehaviour::IfAbsent;

/// For blobstore implementors and advanced admin type users to control requested put behaviour
#[derive(
    IntoStaticStr,
    Clone,
    Copy,
    Debug,
    Display,
    EnumIter,
    EnumString,
    ValueEnum,
    Eq,
    PartialEq
)]
// Forcing backward compatibility with clap-3 for user facing CLI arguments
#[clap(rename_all = "PascalCase")]
pub enum PutBehaviour {
    /// Blobstore::put will overwrite even if key is already present
    Overwrite,
    /// Blobstore::put will overwrite even if key is already present, plus log that and overwrite occurred
    OverwriteAndLog,
    /// Blobstore::put will not overwrite if the key is already present.
    /// NB due to underlying stores TOCTOU limitations some puts might overwrite when when racing another put.
    /// This is expected, thus Blobstore::put() cannot reveal if the put wrote or not as behaviour other than
    /// logging/metrics should not depend on it.
    IfAbsent,
}

impl PutBehaviour {
    // For use inside BlobstoreWithPutBehaviour::put_with_behaviour implementations
    pub fn should_overwrite(&self) -> bool {
        match self {
            PutBehaviour::Overwrite | PutBehaviour::OverwriteAndLog => true,
            PutBehaviour::IfAbsent => false,
        }
    }

    // For use inside OverwriteHandler::on_put_overwrite implementations
    pub fn should_log(&self) -> bool {
        match self {
            PutBehaviour::Overwrite => false,
            PutBehaviour::IfAbsent | PutBehaviour::OverwriteAndLog => true,
        }
    }
}

/// For use from logging blobstores so they can record the overwrite status
/// `Blobstore::put_with_status`, and eventually `Blobstore::copy()` will return this.
#[derive(AsRefStr, Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverwriteStatus {
    // We did not check if the key existed before writing it
    NotChecked,
    // This key did not exist before, therefore we did not overwrite
    New,
    // This did exist before, but we wrote over it
    Overwrote,
    // This did exist before, and the overwrite was prevented
    Prevented,
}

/// BlobstoreKeySource Interface
/// Abstract for use with populate_healer
#[async_trait]
#[auto_impl(Arc, Box)]
pub trait BlobstoreKeySource: Blobstore {
    async fn enumerate<'a>(
        &'a self,
        ctx: &'a CoreContext,
        range: &'a BlobstoreKeyParam,
    ) -> Result<BlobstoreEnumerationData>;
}

trait_set! {
    /// A trait alias that represents blobstores that can be enumerated,
    /// updated and have their keys unlinked.
    #[auto_impl(Arc, Box)]
    pub trait BlobstoreEnumerableWithUnlink = BlobstoreKeySource + Blobstore;
}

/// Range of keys.  The range is inclusive (both start and end key are
/// included in the range), which matches Manifold behaviour.  If the key is
/// empty then the range is unbounded on that end.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct BlobstoreKeyRange {
    pub begin_key: String,
    pub end_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum BlobstoreKeyParam {
    Start(BlobstoreKeyRange),
    Continuation(String),
}

impl From<RangeInclusive<String>> for BlobstoreKeyParam {
    fn from(range: RangeInclusive<String>) -> Self {
        let (start, end) = range.into_inner();
        BlobstoreKeyParam::Start(BlobstoreKeyRange {
            begin_key: start,
            end_key: end,
        })
    }
}

impl From<RangeToInclusive<String>> for BlobstoreKeyParam {
    fn from(range: RangeToInclusive<String>) -> Self {
        BlobstoreKeyParam::Start(BlobstoreKeyRange {
            begin_key: String::from(""),
            end_key: range.end,
        })
    }
}

impl From<RangeFrom<String>> for BlobstoreKeyParam {
    fn from(range: RangeFrom<String>) -> Self {
        BlobstoreKeyParam::Start(BlobstoreKeyRange {
            begin_key: range.start,
            end_key: String::from(""),
        })
    }
}

impl From<RangeFull> for BlobstoreKeyParam {
    fn from(_: RangeFull) -> Self {
        BlobstoreKeyParam::Start(BlobstoreKeyRange {
            begin_key: String::from(""),
            end_key: String::from(""),
        })
    }
}

impl RangeBounds<String> for &BlobstoreKeyRange {
    fn start_bound(&self) -> Bound<&String> {
        if self.begin_key.is_empty() {
            Bound::Unbounded
        } else {
            Bound::Included(&self.begin_key)
        }
    }

    fn end_bound(&self) -> Bound<&String> {
        if self.end_key.is_empty() {
            Bound::Unbounded
        } else {
            Bound::Included(&self.end_key)
        }
    }
}

#[derive(Debug, Clone)]
pub struct BlobstoreEnumerationData {
    pub keys: HashSet<String>,
    /// current range being iterated, this range can be used to resume enumeration
    pub next_token: Option<BlobstoreKeyParam>,
}

#[derive(Debug, Error)]
pub enum LoadableError {
    #[error(transparent)]
    Error(#[from] Error),
    #[error("Blob is missing: {0}")]
    Missing(String),
}

#[async_trait]
pub trait Loadable {
    type Value: Sized + 'static;

    async fn load<'a, B: KeyedBlobstore>(
        &'a self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Value, LoadableError>;
}

#[async_trait]
pub trait Storable: Sized {
    type Key: 'static;

    async fn store<'a, B: KeyedBlobstore>(
        self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Key>;
}

#[async_trait]
impl<L, R> Loadable for Either<L, R>
where
    L: Loadable + Send + Sync,
    R: Loadable + Send + Sync,
{
    type Value = Either<L::Value, R::Value>;

    async fn load<'a, B: KeyedBlobstore>(
        &'a self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Value, LoadableError> {
        match self {
            Either::Left(id) => Ok(Either::Left(
                id.load(ctx, blobstore)
                    .watched()
                    .with_max_poll(BLOBSTORE_MAX_POLL_TIME_MS)
                    .await?,
            )),
            Either::Right(id) => Ok(Either::Right(
                id.load(ctx, blobstore)
                    .watched()
                    .with_max_poll(BLOBSTORE_MAX_POLL_TIME_MS)
                    .await?,
            )),
        }
    }
}

/// StoreLoadable represents an object that be loaded asynchronously through a given store of type
/// S. This offers a bit more flexibility over Blobstore's Loadable, which requires that the object
/// be asynchronously load loadable from a Blobstore. This level of indirection allows for using
/// Manifest's implementations with Manifests that are not backed by a Blobstore.
#[async_trait]
pub trait StoreLoadable<S> {
    type Value;

    async fn load<'a>(
        &'a self,
        ctx: &'a CoreContext,
        store: &'a S,
    ) -> Result<Self::Value, LoadableError>;
}

/// For convenience, all Blobstore Loadables are StoreLoadable through any Blobstore.
#[async_trait]
impl<L: Loadable + Sync + std::fmt::Debug, S: KeyedBlobstore> StoreLoadable<S> for L {
    type Value = <L as Loadable>::Value;

    async fn load<'a>(
        &'a self,
        ctx: &'a CoreContext,
        store: &'a S,
    ) -> Result<Self::Value, LoadableError> {
        self.load(ctx, store)
            .watched()
            .with_label(format!("{__self:?}").as_str())
            .with_max_poll(BLOBSTORE_MAX_POLL_TIME_MS)
            .await
    }
}

/// Trait to copy the value of the key between different blobstores. In the generic
/// case, we need to load from one blobstore to memory, then store in the other.
/// But it may be specialised to be more efficient in specific blobstore pairs.
#[async_trait]
pub trait BlobCopier {
    async fn copy(&self, ctx: &CoreContext, key: String) -> Result<()>;
}

/// Works for any pair of blobstores, does no optimisation at all.
pub struct GenericBlobstoreCopier<'a, A, B> {
    pub source: &'a A,
    pub target: &'a B,
}

#[async_trait]
impl<'a, A: KeyedBlobstore, B: KeyedBlobstore> BlobCopier for GenericBlobstoreCopier<'a, A, B> {
    async fn copy(&self, ctx: &CoreContext, key: String) -> Result<()> {
        let value = self
            .source
            .get(ctx, &key)
            .await?
            .with_context(|| format!("key {key} not present"))?;
        self.target.put(ctx, key, value.into_bytes()).await?;
        Ok(())
    }
}

// Implement Loadable when additional data is carried alongside the blobstore key,
// usually used for (FileType, LeafId) in bonsai manifests.
#[async_trait]
impl<T, L> Loadable for (T, L)
where
    T: Copy + Send + Sync + 'static,
    L: Loadable + Sync + fmt::Debug,
{
    type Value = (T, L::Value);

    async fn load<'a, B: KeyedBlobstore>(
        &'a self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Value, LoadableError> {
        let (t, id) = self;
        Ok((
            *t,
            id.load(ctx, blobstore)
                .watched()
                .with_label(format!("{id:?}").as_str())
                .with_max_poll(BLOBSTORE_MAX_POLL_TIME_MS)
                .await?,
        ))
    }
}
