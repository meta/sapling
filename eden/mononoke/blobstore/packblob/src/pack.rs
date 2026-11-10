/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::io;
use std::io::Cursor;
use std::io::Write;

use anyhow::Error;
use anyhow::Result;
use anyhow::bail;
use anyhow::format_err;
use ascii::AsciiString;
use blobstore::PackMetadata;
use blobstore::SizeMetadata;
use bytes::Buf;
use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use mononoke_types::BlobstoreBytes;
use mononoke_types::hash::Context as HashContext;
use mononoke_types::repo::EPH_REPO_PREFIX_REGEX;
use mononoke_types::repo::REPO_PREFIX_REGEX;
use packblob_thrift::PackedEntry;
use packblob_thrift::PackedFormat;
use packblob_thrift::PackedValue;
use packblob_thrift::SingleValue;
use packblob_thrift::StorageEnvelope;
use packblob_thrift::StorageFormat;
use packblob_thrift::ZstdFromDictValue;
use zstd::bulk::Compressor;
use zstd::dict::EncoderDictionary;
use zstd::stream::read::Decoder as ZstdDecoder;
use zstd::stream::write::Encoder as ZstdEncoder;

use crate::envelope::PackEnvelope;
use crate::store;

/// A block of data compressed on its own, rather than in pack format
pub struct SingleCompressed {
    value: SingleValue,
}

impl SingleCompressed {
    /// Tries to compress the given blob with the given zstd level; will not compress
    /// if the result of compression is an increase in size
    pub fn new(zstd_level: i32, blob: BlobstoreBytes) -> Result<SingleCompressed> {
        let value = blob.into_bytes();
        let mut compressor = Compressor::new(zstd_level)?;
        let compressed = compressor.compress(&value)?;
        let value = if compressed.len() < value.len() {
            SingleValue::Zstd(Bytes::from(compressed))
        } else {
            SingleValue::Raw(value)
        };
        Ok(Self { value })
    }
    /// Always stores the value raw and uncompressed
    pub(crate) fn new_uncompressed(blob: BlobstoreBytes) -> SingleCompressed {
        let value = SingleValue::Raw(blob.into_bytes());
        Self { value }
    }
    /// Gets the size of this blob in compressed form, minus framing overheads
    pub fn get_compressed_size(&self) -> Result<usize> {
        get_value_compressed_size(&self.value)
    }
    pub(crate) fn into_blobstore_bytes(self) -> BlobstoreBytes {
        // Wrap in thrift encoding
        PackEnvelope(StorageEnvelope {
            storage: StorageFormat::Single(self.value),
        })
        .into()
    }
}

/// An empty pack with no data. Cannot be uploaded, takes a dictionary blob
#[derive(Debug)]
pub struct EmptyPack(i32);

/// A pack containing multiple entries, ready to extend or upload
pub struct Pack {
    zstd_level: i32,
    dictionaries: HashMap<String, EncoderDictionary<'static>>,
    entries: Vec<PackedEntry>,
}

impl EmptyPack {
    /// Creates a new EmptyPack
    pub fn new(zstd_level: i32) -> Self {
        EmptyPack(zstd_level)
    }

    /// Adds the first blob to the empty pack
    pub fn add_base_blob(self, key: String, blob: BlobstoreBytes) -> Result<Pack> {
        let zstd_level = self.0;
        let bytes = blob.into_bytes();

        let dictionary = EncoderDictionary::copy(&bytes, zstd_level);

        let cursor = Cursor::new(&bytes);
        let compressed = zstd::encode_all(cursor, zstd_level)?;
        let data = PackedValue::Single(SingleValue::Zstd(Bytes::from(compressed)));

        let mut dictionaries = HashMap::new();
        dictionaries.insert(key.clone(), dictionary);
        let entries = vec![PackedEntry { key, data }];
        Ok(Pack {
            zstd_level,
            dictionaries,
            entries,
        })
    }
}

impl Pack {
    /// Adds another data blob to a pack, delta'd against a previous key
    pub fn add_delta_blob(
        &mut self,
        dict_key: String,
        key: String,
        blob: BlobstoreBytes,
    ) -> Result<()> {
        if self.dictionaries.contains_key(&key) {
            bail!("Key {key} cannot appear in the same pack twice");
        }
        let zstd = {
            let dictionary = self
                .dictionaries
                .get(&dict_key)
                .ok_or_else(|| format_err!("Cannot find dictionary for blob {dict_key}"))?;

            let mut compressed_blob = BytesMut::with_capacity(blob.len());
            let writer = (&mut compressed_blob).writer();
            let mut encoder = ZstdEncoder::with_prepared_dictionary(writer, dictionary)?;

            encoder.write_all(blob.as_bytes())?;
            encoder.finish()?;
            compressed_blob.freeze()
        };
        // This uses `blob` (raw data) to create a dictionary that improves compression
        // at the expense of requiring the decompressor to find blob before it can
        // decompress the resulting blob
        let dictionary = EncoderDictionary::copy(blob.as_bytes(), self.zstd_level);
        let data = PackedValue::ZstdFromDict(ZstdFromDictValue { dict_key, zstd });
        self.dictionaries.insert(key.clone(), dictionary);
        self.entries.push(PackedEntry { key, data });
        Ok(())
    }

    /// Returns the compressed size of the pack contents, minus framing overheads
    pub fn get_compressed_size(&self) -> Result<usize> {
        let mut size = 0;
        for e in self.entries.iter() {
            size = size + get_entry_compressed_size(e)? + e.key.len();
        }
        Ok(size)
    }

    pub fn entries(&self) -> &[PackedEntry] {
        &self.entries
    }

    /// The key `PackBlob::put_packed` stores this pack under for `pack_prefix`.
    /// It depends only on the set of entry keys, so packs with the same entries
    /// share an identity.
    pub fn pack_key(&self, pack_prefix: &str) -> String {
        let mut link_keys: Vec<&str> = self
            .entries
            .iter()
            .map(|entry| entry.key.as_str())
            .collect();
        link_keys.sort_unstable();
        pack_key_for(pack_prefix, &link_keys)
    }

    /// Converts the pack into something that can go into a blobstore
    pub(crate) fn into_blobstore_bytes(
        self,
        pack_prefix: String,
    ) -> Result<(String, Vec<String>, BlobstoreBytes)> {
        for entry in &self.entries {
            let (prefix, _) = split_key_prefix(&entry.key);
            if !prefix.is_empty() {
                bail!(
                    "Key prefix {} found in packed blob key {}",
                    prefix,
                    entry.key
                );
            }
        }
        let mut link_keys: Vec<String> =
            self.entries.iter().map(|entry| entry.key.clone()).collect();
        link_keys.sort_unstable();
        let pack_key = pack_key_for(&pack_prefix, &link_keys);

        let pack = PackedFormat {
            key: pack_key.clone(),
            entries: self.entries,
        };

        // Wrap in thrift encoding and return as bytes
        Ok((
            pack_key,
            link_keys,
            PackEnvelope(StorageEnvelope {
                storage: StorageFormat::Packed(pack),
            })
            .into(),
        ))
    }
}

fn get_value_compressed_size(value: &SingleValue) -> Result<usize> {
    match value {
        SingleValue::Raw(bytes) | SingleValue::Zstd(bytes) => Ok(bytes.len()),
        // Can't happen, by construction - this only takes values created by this module
        SingleValue::UnknownField(_) => bail!("Unknown field"),
    }
}

pub fn get_entry_compressed_size(entry: &PackedEntry) -> Result<usize> {
    match &entry.data {
        PackedValue::Single(value) => get_value_compressed_size(value),
        PackedValue::ZstdFromDict(ZstdFromDictValue { zstd, .. }) => Ok(zstd.len()),
        // Can't happen, by construction - this only takes values created by this module
        PackedValue::UnknownField(_) => bail!("Unknown field"),
    }
}

// returns (decoded, unique_compressed_size)
pub(crate) fn decode_independent(v: SingleValue) -> Result<(BlobstoreBytes, u64)> {
    let (compressed_size, decoded) = match v {
        SingleValue::Raw(v) => (v.len() as u64, BlobstoreBytes::from_bytes(v)),
        SingleValue::Zstd(v) => (
            v.len() as u64,
            zstd::decode_all(v.reader()).map(BlobstoreBytes::from_bytes)?,
        ),
        SingleValue::UnknownField(e) => bail!("SingleValue::UnknownField {e:?}"),
    };
    Ok((decoded, compressed_size))
}

fn decode_zstd_from_dict(
    k: &str,
    v: ZstdFromDictValue,
    dicts: &HashMap<String, BlobstoreBytes>,
) -> Result<(BlobstoreBytes, u64), Error> {
    match dicts.get(&v.dict_key) {
        Some(dict) => {
            let uncompressed_size = v.zstd.len() as u64;
            let data = v.zstd.reader();
            let mut decoder = ZstdDecoder::with_dictionary(data, dict.as_bytes())?;
            let mut output_bytes = BytesMut::new();
            let mut writer = (&mut output_bytes).writer();
            io::copy(&mut decoder, &mut writer)?;

            Ok((BlobstoreBytes::from_bytes(output_bytes), uncompressed_size))
        }
        None => Err(format_err!(
            "Dictionary {} not found for key {}",
            v.dict_key,
            k
        )),
    }
}

// Unpack `key` from `packed`
pub(crate) fn decode_pack(
    packed: PackedFormat,
    key: &str,
) -> Result<(BlobstoreBytes, SizeMetadata)> {
    // Strip key prefix, if any
    let (_, key) = split_key_prefix(key);

    let PackedFormat {
        key: pack_key,
        entries: pack_entries,
    } = packed;

    let mut entry_map = HashMap::new();
    for entry in pack_entries {
        entry_map.insert(entry.key, entry.data);
        if entry_map.contains_key(key) {
            // Dictionaries must come before their users, so we don't care about the rest of the pack
            break;
        }
    }
    // Decode time
    let mut decoded_blobs = HashMap::new();
    let mut keys_to_decode = vec![key.to_string()];
    let mut unique_compressed_size = 0;
    let mut relevant_compressed_size = 0;
    let mut relevant_uncompressed_size = 0;
    while let Some(next_key) = keys_to_decode.pop() {
        match entry_map.remove(&next_key) {
            None => {
                if next_key == key {
                    // Handled below
                    break;
                }
                bail!("Key {key} needs dictionary {next_key} but it is not in the pack");
            }
            Some(PackedValue::UnknownField(e)) => {
                bail!("PackedValue::UnknownField {e:?}");
            }
            Some(PackedValue::Single(v)) => {
                let (decoded, compressed_size) = decode_independent(v)?;
                relevant_uncompressed_size += decoded.len() as u64;
                if next_key == key {
                    unique_compressed_size += compressed_size;
                }
                relevant_compressed_size += compressed_size;
                decoded_blobs.insert(next_key, decoded);
            }
            Some(PackedValue::ZstdFromDict(v)) => {
                if decoded_blobs.contains_key(&v.dict_key) {
                    let (decoded, compressed_size) =
                        decode_zstd_from_dict(&next_key, v, &decoded_blobs)?;
                    relevant_uncompressed_size += decoded.len() as u64;
                    if next_key == key {
                        unique_compressed_size += compressed_size;
                    }
                    relevant_compressed_size += compressed_size;
                    decoded_blobs.insert(next_key, decoded);
                } else if v.dict_key == next_key || keys_to_decode.contains(&v.dict_key) {
                    bail!(
                        "Key {key} needs dictionary {} but it forms a cycle in the pack",
                        v.dict_key
                    );
                } else {
                    // Can't yet decode it - push the keys we need to decode this onto the work queue in order.
                    keys_to_decode.push(next_key.clone());
                    keys_to_decode.push(v.dict_key.clone());
                    // And reinsert this for the next loop
                    entry_map.insert(next_key, PackedValue::ZstdFromDict(v));
                }
            }
        }
    }

    let decoded = decoded_blobs
        .remove(key)
        .ok_or_else(|| format_err!("Key {key} not in the pack it is pointing to {pack_key}"))?;

    let pack_meta = PackMetadata {
        pack_key,
        relevant_compressed_size,
        relevant_uncompressed_size,
    };
    let sizing = SizeMetadata {
        unique_compressed_size,
        pack_meta: Some(pack_meta),
    };

    Ok((decoded, sizing))
}

/// What a physical `.pack` key holds.
pub enum StoredValue {
    /// The value is stored on its own.
    Single,
    /// The value is one entry of a pack that is linked from its entries' keys.
    Packed(PackContents),
}

/// The contents of a pack, as read through any key linking to it.
pub struct PackContents(PackedFormat);

impl PackContents {
    /// The identity the pack was stored with.  Every link to the same pack
    /// reports it, and a pack with a different set of entries has a different one.
    pub fn pack_key(&self) -> &str {
        &self.0.key
    }

    /// Keys of the pack's entries, without repository prefix, in stored order.
    /// An entry may only be present as a dictionary for other entries, with no
    /// key linking to the pack for it.
    pub fn entry_keys(&self) -> impl Iterator<Item = &str> {
        self.0.entries.iter().map(|entry| entry.key.as_str())
    }

    /// Decode the given entries (keys without repository prefix), resolving
    /// their dictionaries through the other entries of the pack.
    pub fn decode_entries(&self, keys: &[&str]) -> Result<HashMap<String, BlobstoreBytes>> {
        let entries: HashMap<&str, &PackedValue> = self
            .0
            .entries
            .iter()
            .map(|entry| (entry.key.as_str(), &entry.data))
            .collect();
        let mut decoded: HashMap<String, BlobstoreBytes> = HashMap::new();
        for key in keys {
            let mut to_decode = vec![key.to_string()];
            while let Some(next_key) = to_decode.last().cloned() {
                if decoded.contains_key(&next_key) {
                    to_decode.pop();
                    continue;
                }
                let value = entries.get(next_key.as_str()).ok_or_else(|| {
                    format_err!("Key {key} needs entry {next_key}, which is not in the pack")
                })?;
                match value {
                    PackedValue::Single(v) => {
                        let (value, _) = decode_independent(v.clone())?;
                        decoded.insert(next_key, value);
                        to_decode.pop();
                    }
                    PackedValue::ZstdFromDict(v) if decoded.contains_key(&v.dict_key) => {
                        let (value, _) = decode_zstd_from_dict(&next_key, v.clone(), &decoded)?;
                        decoded.insert(next_key, value);
                        to_decode.pop();
                    }
                    PackedValue::ZstdFromDict(v) => {
                        if to_decode.contains(&v.dict_key) {
                            bail!(
                                "Key {key} needs dictionary {} but it forms a cycle in the pack",
                                v.dict_key
                            );
                        }
                        to_decode.push(v.dict_key.clone());
                    }
                    PackedValue::UnknownField(e) => bail!("PackedValue::UnknownField {e:?}"),
                }
            }
        }
        keys.iter()
            .map(|key| {
                let value = decoded
                    .get(*key)
                    .ok_or_else(|| format_err!("Key {key} was not decoded"))?;
                Ok((key.to_string(), value.clone()))
            })
            .collect()
    }
}

/// Interpret the raw bytes stored under a physical `.pack` key.
pub fn inspect_stored_value(raw: BlobstoreBytes) -> Result<StoredValue> {
    let envelope: PackEnvelope = raw.try_into()?;
    match envelope.0.storage {
        StorageFormat::Single(_) => Ok(StoredValue::Single),
        StorageFormat::Packed(packed) => Ok(StoredValue::Packed(PackContents(packed))),
        StorageFormat::UnknownField(e) => Err(format_err!("StorageFormat::UnknownField {e:?}")),
    }
}

// The identity of a pack holding entries with these sorted keys.
fn pack_key_for<K: AsRef<str>>(pack_prefix: &str, sorted_keys: &[K]) -> String {
    format!(
        "{pack_prefix}{}{}",
        compute_pack_hash(sorted_keys),
        store::ENVELOPE_SUFFIX
    )
}

// Hash the keys as they themselves are hashes
fn compute_pack_hash<K: AsRef<str>>(keys: &[K]) -> AsciiString {
    let mut hash_context = HashContext::new(b"pack");
    for key in keys {
        let key = key.as_ref();
        hash_context.update(key.len().to_le_bytes());
        hash_context.update(key.as_bytes());
    }
    hash_context.finish().to_hex()
}

/// Find the key prefix for a given key.  Key prefixes are removed when
/// keys are stored in packs.  Returns the key prefix and the remainder
/// of the key.
fn split_key_prefix(key: &str) -> (&str, &str) {
    if let Some(m) = REPO_PREFIX_REGEX.find(key) {
        key.split_at(m.end())
    } else if let Some(m) = EPH_REPO_PREFIX_REGEX.find(key) {
        key.split_at(m.end())
    } else {
        ("", key)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use mononoke_macros::mononoke;
    use rand::Rng as _;
    use rand::RngExt as _;
    use rand::SeedableRng;
    use rand_xorshift::XorShiftRng;

    use super::*;

    #[mononoke::test]
    fn decode_independent_zstd_test() -> Result<()> {
        // Highly compressible!
        let bytes_in = vec![7u8; 65535];

        // Prepare a compressed blob
        let input = Cursor::new(bytes_in.clone());
        let bytes = Bytes::from(zstd::encode_all(input, 0 /* default */)?);
        assert!(bytes.len() < bytes_in.len());
        let expected_compressed_size = bytes.len() as u64;

        // Test the decoder
        let (decoded, compressed_size) = decode_independent(SingleValue::Zstd(bytes))?;
        assert_eq!(decoded.as_bytes(), &Bytes::from(bytes_in));

        // Check the metadata
        assert_eq!(expected_compressed_size, compressed_size);

        Ok(())
    }

    #[mononoke::test]
    fn decode_zstd_from_dict_test() -> Result<()> {
        let mut rng = XorShiftRng::seed_from_u64(0); // reproducible Rng

        // Some partially incompressible data as base version
        let mut base_version = vec![7u8; 65535];
        rng.fill_bytes(&mut base_version[0..60000]);

        // Extend it with incompressible new data
        let mut next_version = base_version.clone();
        rng.fill_bytes(&mut next_version[60000..]);

        let diff = {
            let mut compressed_blob = BytesMut::new();
            let writer = (&mut compressed_blob).writer();
            let mut encoder = ZstdEncoder::with_dictionary(writer, 0, &base_version)?;

            encoder.write_all(&next_version)?;
            encoder.finish()?;
            compressed_blob.freeze()
        };
        let expected_compressed_size = diff.len() as u64;

        let base_key = "base".to_string();

        let mut dicts = HashMap::new();
        dicts.insert(base_key.clone(), BlobstoreBytes::from_bytes(base_version));

        // Test the decoder
        let (decoded, compressed_size) = decode_zstd_from_dict(
            "appkey",
            ZstdFromDictValue {
                dict_key: base_key,
                zstd: diff,
            },
            &dicts,
        )?;
        assert_eq!(decoded.as_bytes(), &Bytes::from(next_version));
        assert_eq!(expected_compressed_size, compressed_size);

        Ok(())
    }

    #[mononoke::test]
    fn pack_zstd_from_dict_chain_test() -> Result<()> {
        let mut rng = XorShiftRng::seed_from_u64(0); // reproducible Rng

        let mut raw_data = vec![];
        let pack = EmptyPack::new(0);

        // Some partially compressible data as base version
        let mut base_version = vec![7u8; 65535];
        rng.fill_bytes(&mut base_version[0..30000]);
        let base_key = "0".to_string();
        raw_data.push(base_version.clone());
        let mut pack =
            pack.add_base_blob(base_key, BlobstoreBytes::from_bytes(base_version.clone()))?;

        // incrementally build a pack from seeded random data
        let mut prev_version = base_version;
        for i in 1..20 {
            let mut this_version = prev_version;
            let start = 30000 + i * 1000;
            let end = start + 1000;
            rng.fill(&mut this_version[start..end]);
            raw_data.push(this_version.clone());
            prev_version = this_version.clone();
            pack.add_delta_blob(
                (i - 1).to_string(),
                i.to_string(),
                BlobstoreBytes::from_bytes(this_version),
            )?;
        }

        let (key, links, blob) = pack.into_blobstore_bytes(String::new())?;

        let mut expected_links: Vec<String> = (0..20_u32).map(|i| i.to_string()).collect();
        expected_links.sort_unstable();
        assert_eq!(expected_links, links);

        let packed = {
            let envelope: PackEnvelope = blob.try_into()?;
            if let StorageFormat::Packed(pack) = envelope.0.storage {
                pack
            } else {
                bail!("Packing resulted in a single value, not a pack");
            }
        };

        assert_eq!(packed.key, key);

        // Check for any change of hashing approach
        assert_eq!(
            "49df56ffd39791720f4d4b5c05d3156dfeab994fcb6b0000b3c2343280b95975.pack",
            packed.key
        );

        // Test reads roundtrip back to the raw form and metadata is populated
        for (raw_data, i) in raw_data.into_iter().zip(0..20) {
            let (value, size_meta) = decode_pack(packed.clone(), &i.to_string())?;
            assert_eq!(value.into_bytes(), Bytes::from(raw_data));
            assert!(size_meta.unique_compressed_size > 0);
            assert!(size_meta.pack_meta.is_some());
            if let Some(pack_meta) = size_meta.pack_meta {
                assert_eq!(&packed.key, &pack_meta.pack_key);
                assert!(pack_meta.relevant_compressed_size > 0);
                assert!(pack_meta.relevant_uncompressed_size > 0);
                assert!(pack_meta.relevant_compressed_size >= size_meta.unique_compressed_size);
            }
        }

        Ok(())
    }

    #[mononoke::test]
    fn pack_zstd_from_dict_unchained_test() -> Result<()> {
        let mut rng = XorShiftRng::seed_from_u64(0); // reproducible Rng

        let mut raw_data = vec![];
        let pack = EmptyPack::new(0);

        // Some partially compressible data as base version
        let mut base_version = vec![7u8; 65535];
        rng.fill_bytes(&mut base_version[0..30000]);
        let base_key = "0".to_string();
        raw_data.push(base_version.clone());
        let mut pack =
            pack.add_base_blob(base_key, BlobstoreBytes::from_bytes(base_version.clone()))?;

        // incrementally build a pack from seeded random data
        let mut prev_version = base_version;
        for i in 1..20 {
            let mut this_version = prev_version;
            let start = 30000 + i * 1000;
            let end = start + 1000;
            rng.fill(&mut this_version[start..end]);
            raw_data.push(this_version.clone());
            prev_version = this_version.clone();
            pack.add_delta_blob(
                "0".to_string(),
                i.to_string(),
                BlobstoreBytes::from_bytes(this_version),
            )?;
        }

        let (key, links, blob) = pack.into_blobstore_bytes(String::new())?;

        let mut expected_links: Vec<String> = (0..20_u32).map(|i| i.to_string()).collect();
        expected_links.sort_unstable();
        assert_eq!(expected_links, links);

        let packed = {
            let envelope: PackEnvelope = blob.try_into()?;
            if let StorageFormat::Packed(pack) = envelope.0.storage {
                pack
            } else {
                bail!("Packing resulted in a single value, not a pack");
            }
        };

        assert_eq!(packed.key, key);

        // Check for any change of hashing approach
        assert_eq!(
            "49df56ffd39791720f4d4b5c05d3156dfeab994fcb6b0000b3c2343280b95975.pack",
            packed.key
        );

        // Test reads roundtrip back to the raw form and metadata is populated
        for (raw_data, i) in raw_data.into_iter().zip(0..20) {
            let (value, size_meta) = decode_pack(packed.clone(), &i.to_string())?;
            assert_eq!(value.into_bytes(), Bytes::from(raw_data));
            assert!(size_meta.unique_compressed_size > 0);
            assert!(size_meta.pack_meta.is_some());
            if let Some(pack_meta) = size_meta.pack_meta {
                assert_eq!(&packed.key, &pack_meta.pack_key);
                assert!(pack_meta.relevant_compressed_size > 0);
                assert!(pack_meta.relevant_uncompressed_size > 0);
                assert!(pack_meta.relevant_compressed_size >= size_meta.unique_compressed_size);
            }
        }

        Ok(())
    }

    #[mononoke::test]
    fn pack_size_test() -> Result<()> {
        let mut rng = XorShiftRng::seed_from_u64(0); // reproducible Rng

        let mut raw_data = vec![];
        let pack = EmptyPack::new(19);

        // Some partially compressible data as base version
        let mut base_version = vec![7u8; 65535];
        rng.fill_bytes(&mut base_version[0..30000]);
        let base_key = "0".to_string();
        raw_data.push(base_version.clone());
        let mut pack =
            pack.add_base_blob(base_key, BlobstoreBytes::from_bytes(base_version.clone()))?;

        // Check the compressed size is reasonable
        let base_compressed_size = pack.get_compressed_size()?;
        assert!(
            base_compressed_size > 1024,
            "Compression turned 64 KiB into {base_compressed_size} bytes - suspiciously small"
        );
        assert!(
            base_compressed_size < 65535,
            "Compression turned 64 KiB into {base_compressed_size} bytes - expansion unexpected"
        );

        // incrementally build a pack from seeded random data
        let mut prev_version = base_version;
        for i in 1..20 {
            let mut this_version = prev_version;
            let start = 30000 + i * 1000;
            let end = start + 1000;
            rng.fill(&mut this_version[start..end]);
            raw_data.push(this_version.clone());
            prev_version = this_version.clone();
            pack.add_delta_blob(
                (i - 1).to_string(),
                i.to_string(),
                BlobstoreBytes::from_bytes(this_version),
            )?;
        }

        // And check it's grown, but not too far given how compressible this should be.
        let compressed_size = pack.get_compressed_size()?;
        assert!(
            compressed_size > base_compressed_size,
            "Pack shrank as it gained data"
        );
        let limit = base_compressed_size + 20 * 1000;
        assert!(
            compressed_size < limit,
            "Pack grew by more than the size of added data. Expected {compressed_size} < {limit}",
        );

        Ok(())
    }

    fn chained_pack() -> Result<(Pack, Vec<(String, Bytes)>)> {
        let mut rng = XorShiftRng::seed_from_u64(0); // reproducible Rng
        let mut version = vec![7u8; 4096];
        rng.fill_bytes(&mut version[0..2048]);
        let mut values = vec![("a".to_string(), Bytes::from(version.clone()))];
        let mut pack = EmptyPack::new(0)
            .add_base_blob("a".to_string(), BlobstoreBytes::from_bytes(version.clone()))?;
        for (dict_key, key) in [("a", "b"), ("b", "c"), ("a", "d")] {
            rng.fill_bytes(&mut version[2048..2560]);
            pack.add_delta_blob(
                dict_key.to_string(),
                key.to_string(),
                BlobstoreBytes::from_bytes(version.clone()),
            )?;
            values.push((key.to_string(), Bytes::from(version.clone())));
        }
        Ok((pack, values))
    }

    #[mononoke::test]
    fn inspect_and_decode_pack_entries() -> Result<()> {
        let (pack, values) = chained_pack()?;
        let expected_key = pack.pack_key("multiblob-");
        let (pack_key, _, blob) = pack.into_blobstore_bytes("multiblob-".to_string())?;
        assert_eq!(
            expected_key, pack_key,
            "pack_key must predict the stored identity"
        );

        let StoredValue::Packed(contents) = inspect_stored_value(blob)? else {
            bail!("a pack must be inspected as packed");
        };
        assert_eq!(contents.pack_key(), pack_key);
        assert_eq!(
            contents.entry_keys().collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"]
        );

        // "c" is only decodable through "b" and then "a".
        let decoded = contents.decode_entries(&["c", "d", "c"])?;
        assert_eq!(
            decoded.len(),
            2,
            "only the requested entries are returned, once each"
        );
        for (key, value) in values.iter().filter(|(key, _)| key == "c" || key == "d") {
            assert_eq!(decoded[key].as_bytes(), value);
        }

        let err = match contents.decode_entries(&["e"]) {
            Ok(_) => bail!("decoding an absent entry must fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("not in the pack"), "got: {err}");
        Ok(())
    }

    #[mononoke::test]
    fn inspect_single_and_invalid_values() -> Result<()> {
        let single = SingleCompressed::new_uncompressed(BlobstoreBytes::from_bytes(
            Bytes::from_static(b"value"),
        ))
        .into_blobstore_bytes();
        assert!(matches!(inspect_stored_value(single)?, StoredValue::Single));
        assert!(
            inspect_stored_value(BlobstoreBytes::from_bytes(Bytes::from_static(b"ab"))).is_err(),
            "a value too short for an envelope header must be rejected"
        );
        Ok(())
    }

    #[mononoke::test]
    fn decode_entries_cyclic_dict_chain() -> Result<()> {
        let contents = PackContents(PackedFormat {
            key: "multiblob-cycle.pack".to_string(),
            entries: vec![
                PackedEntry {
                    key: "0".to_string(),
                    data: PackedValue::ZstdFromDict(ZstdFromDictValue {
                        dict_key: "1".to_string(),
                        zstd: Bytes::new(),
                    }),
                },
                PackedEntry {
                    key: "1".to_string(),
                    data: PackedValue::ZstdFromDict(ZstdFromDictValue {
                        dict_key: "0".to_string(),
                        zstd: Bytes::new(),
                    }),
                },
            ],
        });
        let err = match contents.decode_entries(&["0"]) {
            Ok(_) => bail!("a cyclic dictionary chain must be rejected"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("cycle"), "got: {err}");
        Ok(())
    }

    #[mononoke::test]
    fn decode_pack_cyclic_dict_chain() {
        // A pack whose only entry says "my dictionary is myself". The in-tree Pack builder cannot
        // produce this, but decode_pack reads PackedFormat straight from the blobstore.
        let packed = PackedFormat {
            key: "repo0000.packed.pack".to_string(),
            entries: vec![PackedEntry {
                key: "0".to_string(),
                data: PackedValue::ZstdFromDict(ZstdFromDictValue {
                    dict_key: "0".to_string(),
                    zstd: Bytes::new(),
                }),
            }],
        };
        let err = decode_pack(packed, "repo0000.0")
            .expect_err("decode_pack must reject a cyclic dictionary chain");
        assert!(
            err.to_string().contains("cycle"),
            "expected a cycle error, got: {err}"
        );
    }
}
