/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io::Write;

use anyhow::Context;
use anyhow::Result;
use blobstore::BlobstoreBytes;
use bytes::Bytes;
use bytes::BytesMut;
use fbthrift::compact_protocol;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use gix_hash::ObjectId;
use gix_hash::oid;
use gix_pack::data::output;
use mononoke_types::private::MononokeTypeError;
use quickcheck::Arbitrary;
use sha1::Digest;
use sha1::Sha1;

use crate::ObjectContent;
use crate::thrift;

/// The type of items that can be present in a Git packfile. Does not include RefDelta currently
/// since we do not use it
/// See: <https://fburl.com/1yaui1um>
#[derive(Debug)]
pub enum PackfileItem {
    /// The base object representing a raw git type (e.g. commit, tree, tag or blob)
    Base(BaseObject),
    /// The base object which already contains the encoded version of the raw git type
    /// as expected by the packfile format
    EncodedBase(output::Entry),
    /// The delta object which represents a change between two objects identified by their hashes
    OidDelta(DeltaOidObject),
}

impl PackfileItem {
    pub fn new_base(object_bytes: Bytes) -> Result<Self> {
        BaseObject::new(object_bytes).map(Self::Base)
    }

    pub fn new_encoded_base(entry: output::Entry) -> Self {
        Self::EncodedBase(entry)
    }

    pub fn new_delta(
        oid: ObjectId,
        base_oid: ObjectId,
        decompressed_size: u64,
        compressed_data: Vec<u8>,
    ) -> Self {
        Self::OidDelta(DeltaOidObject::new(
            oid,
            base_oid,
            decompressed_size,
            compressed_data,
        ))
    }

    /// Estimated in-memory byte weight of this item. Values are chosen to
    /// match entry_weight() in the generator for balanced add/remove on
    /// estimated_memory_bytes:
    /// - Base: uncompressed size (matches full_object_size())
    /// - EncodedBase: decompressed_size (matches full_object_size())
    /// - OidDelta: compressed_data.len() (matches instructions_compressed_size())
    pub fn weight(&self) -> usize {
        match self {
            PackfileItem::Base(base) => base.size(),
            PackfileItem::EncodedBase(entry) => entry.decompressed_size,
            PackfileItem::OidDelta(delta) => delta.compressed_data.len(),
        }
    }
}

impl TryFrom<PackfileItem> for output::Entry {
    type Error = anyhow::Error;

    fn try_from(value: PackfileItem) -> Result<Self> {
        match value {
            PackfileItem::Base(base) => base.try_into(),
            PackfileItem::EncodedBase(entry) => Ok(entry),
            PackfileItem::OidDelta(oid_delta) => oid_delta.try_into(),
        }
    }
}

/// Struct representing the DeltaOid variant of the packfile item. Used to express
/// a target object as a delta of a hash-identified base object
#[derive(Debug)]
pub struct DeltaOidObject {
    /// The ObjectId of the object that would be constructed using the delta
    oid: ObjectId,
    /// The ObjectId of the object that will be used as base to create delta
    base_oid: ObjectId,
    /// The size of the delta instructions object once it is decompressed
    decompressed_size: usize,
    /// The compressed/encoded data of the delta instructions object
    compressed_data: Vec<u8>,
}

impl DeltaOidObject {
    pub fn new(
        oid: ObjectId,
        base_oid: ObjectId,
        decompressed_size: u64,
        compressed_data: Vec<u8>,
    ) -> Self {
        Self {
            oid,
            base_oid,
            decompressed_size: decompressed_size as usize,
            compressed_data,
        }
    }

    pub fn kind(&self) -> output::entry::Kind {
        output::entry::Kind::DeltaOid {
            id: self.base_oid.clone(),
        }
    }
}

impl TryFrom<DeltaOidObject> for output::Entry {
    type Error = anyhow::Error;

    fn try_from(value: DeltaOidObject) -> Result<Self> {
        let kind = value.kind();
        let entry = Self {
            id: value.oid,
            decompressed_size: value.decompressed_size,
            compressed_data: value.compressed_data,
            kind,
        };
        anyhow::Ok(entry)
    }
}

/// Struct representing a base Git object that can be included in packfiles
#[derive(Debug)]
pub struct BaseObject {
    pub object: ObjectContent,
    pub hash: ObjectId,
}

impl BaseObject {
    /// Creates a new packfile item from the raw object bytes of the Git object.
    pub fn new(object_bytes: Bytes) -> Result<Self> {
        let mut hasher = Sha1::new();
        hasher.update(&object_bytes);
        let hash_bytes = hasher.finalize();
        let hash = oid::try_from_bytes(hash_bytes.as_ref())
            .context("Failed to convert packfile item hash to Git Object ID")?
            .into();
        // Create the Git object from raw bytes
        let object = ObjectContent::try_from_loose(object_bytes)?;
        // Create the packfile item from the object and the hash
        anyhow::Ok(Self { object, hash })
    }

    /// The kind of the packfile item.
    pub fn kind(&self) -> output::entry::Kind {
        output::entry::Kind::Base(self.object.kind())
    }

    /// The uncompressed size of the Git object contained within the pack item.
    pub fn size(&self) -> usize {
        self.object.size() as usize
    }

    /// The 20-byte SHA1 hash (ObjectId) of the Git object contained within the
    /// pack item.
    pub fn hash(&self) -> &gix_hash::oid {
        self.hash.as_ref()
    }

    /// Zlib encode the raw bytes of the Git object and write it to `out`.
    pub fn write_encoded(&self, out: &mut BytesMut, include_header: bool) -> Result<()> {
        let object_bytes = match include_header {
            true => to_vec_bytes(&self.object)?,
            false => to_vec_bytes_without_header(&self.object)?,
        };
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&object_bytes)
            .context("Failure in writing raw Git object data to ZLib buffer")?;
        let compressed_object = encoder
            .finish()
            .context("Failure in ZLib encoding Git object data")?;
        out.extend(&compressed_object);
        anyhow::Ok(())
    }
}

impl TryFrom<BaseObject> for output::Entry {
    type Error = anyhow::Error;

    fn try_from(value: BaseObject) -> Result<Self> {
        let id = value.hash().into();
        let decompressed_size = value.size();
        let kind = value.kind();
        let mut encoded_bytes = BytesMut::new();
        // No need to include the loose format header since the objects are
        // provided a different header format when being included in packfiles.
        value.write_encoded(&mut encoded_bytes, false)?;
        let compressed_data = encoded_bytes.freeze().to_vec();
        let entry = Self {
            id,
            kind,
            decompressed_size,
            compressed_data,
        };
        anyhow::Ok(entry)
    }
}

impl TryFrom<BaseObject> for GitPackfileBaseItem {
    type Error = anyhow::Error;

    fn try_from(value: BaseObject) -> Result<Self> {
        let kind = match value.kind() {
            output::entry::Kind::Base(kind) => kind,
            _ => anyhow::bail!(
                "Cannot convert non-base output entry object into GitPackfileBaseItem"
            ),
        };
        let output_entry: output::Entry = value.try_into()?;
        anyhow::Ok(Self {
            id: output_entry.id,
            decompressed_size: output_entry.decompressed_size,
            compressed_data: output_entry.compressed_data,
            kind,
        })
    }
}

/// Struct representing the raw packfile item for base objects in Git
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPackfileBaseItem {
    id: ObjectId,
    decompressed_size: usize,
    kind: gix_object::Kind,
    compressed_data: Vec<u8>,
}

impl TryFrom<thrift::GitPackfileBaseItem> for GitPackfileBaseItem {
    type Error = anyhow::Error;

    fn try_from(t: thrift::GitPackfileBaseItem) -> Result<Self> {
        let id = oid::try_from_bytes(&t.id)?.to_owned();
        let decompressed_size = t.decompressed_size as usize;
        let kind = match t.kind {
            thrift::GitObjectKind::Blob => gix_object::Kind::Blob,
            thrift::GitObjectKind::Tree => gix_object::Kind::Tree,
            thrift::GitObjectKind::Commit => gix_object::Kind::Commit,
            thrift::GitObjectKind::Tag => gix_object::Kind::Tag,
            thrift::GitObjectKind(x) => anyhow::bail!("Unsupported object kind: {x}"),
        };
        anyhow::Ok(Self {
            id,
            decompressed_size,
            kind,
            compressed_data: t.compressed_data,
        })
    }
}

impl From<GitPackfileBaseItem> for thrift::GitPackfileBaseItem {
    fn from(packfile_item: GitPackfileBaseItem) -> thrift::GitPackfileBaseItem {
        let id = packfile_item.id.as_ref().as_bytes().to_vec();
        let decompressed_size = packfile_item.decompressed_size as i64;
        let kind = match packfile_item.kind {
            gix_object::Kind::Blob => thrift::GitObjectKind::Blob,
            gix_object::Kind::Tree => thrift::GitObjectKind::Tree,
            gix_object::Kind::Commit => thrift::GitObjectKind::Commit,
            gix_object::Kind::Tag => thrift::GitObjectKind::Tag,
        };
        thrift::GitPackfileBaseItem {
            id,
            decompressed_size,
            kind,
            compressed_data: packfile_item.compressed_data,
        }
    }
}

impl TryFrom<GitPackfileBaseItem> for output::Entry {
    type Error = anyhow::Error;

    fn try_from(value: GitPackfileBaseItem) -> Result<Self> {
        let entry = Self {
            id: value.id,
            kind: output::entry::Kind::Base(value.kind),
            decompressed_size: value.decompressed_size,
            compressed_data: value.compressed_data,
        };
        anyhow::Ok(entry)
    }
}

impl GitPackfileBaseItem {
    pub fn from_encoded_bytes(encoded_bytes: Vec<u8>) -> Result<Self> {
        let thrift_item: thrift::GitPackfileBaseItem = compact_protocol::deserialize(encoded_bytes)
            .with_context(|| {
                MononokeTypeError::BlobDeserializeError("GitPackfileBaseItem".into())
            })?;
        thrift_item.try_into()
    }

    pub fn into_blobstore_bytes(self) -> BlobstoreBytes {
        let thrift_item: thrift::GitPackfileBaseItem = self.into();
        BlobstoreBytes::from_bytes(compact_protocol::serialize(thrift_item))
    }
}

impl Arbitrary for GitPackfileBaseItem {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        let compressed_data: Vec<u8> = Vec::arbitrary(g);
        let id = oid::try_from_bytes(mononoke_types::hash::Sha1::arbitrary(g).as_ref())
            .unwrap()
            .into();
        let decompressed_size = usize::arbitrary(g) / 2;
        let kind = g
            .choose(&[
                gix_object::Kind::Blob,
                gix_object::Kind::Tree,
                gix_object::Kind::Commit,
                gix_object::Kind::Tag,
            ])
            .unwrap()
            .clone();
        Self {
            id,
            decompressed_size,
            kind,
            compressed_data,
        }
    }
}

/// Free function responsible for writing only the Git object data to a Vec
/// without including the loose format headers
pub(crate) fn to_vec_bytes_without_header(git_object: &ObjectContent) -> Result<Vec<u8>> {
    let mut object_bytes = Vec::new();
    git_object.write_to(object_bytes.by_ref())?;
    anyhow::Ok(object_bytes)
}

/// Free function responsible for writing Git object data to a Vec
/// in loose format
pub fn to_vec_bytes(git_object: &ObjectContent) -> Result<Vec<u8>> {
    let mut object_bytes = git_object.loose_header().into_vec();
    git_object.write_to(object_bytes.by_ref())?;
    anyhow::Ok(object_bytes)
}
