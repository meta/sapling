/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt::Display;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use async_trait::async_trait;
use blobstore::BlobstoreBytes;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::LoadableError;
use bytes::Bytes;
use context::CoreContext;
use fbthrift::compact_protocol;
use futures::Stream;
use futures::StreamExt;
use futures_watchdog::WatchdogExt;
use quickcheck::Arbitrary;
use quickcheck::Gen;
use strum::EnumIter;

use crate::blob::Blob;
use crate::blob::BlobstoreValue;
use crate::blob::ContentMetadataV2Blob;
use crate::errors::MononokeTypeError;
use crate::hash;
use crate::thrift;
use crate::thrift_field;
use crate::typed_hash::ContentId;
use crate::typed_hash::ContentMetadataV2Id;

const MAX_BYTES_FOR_FIRST_LINE: usize = 64;
const UTF8_BYTES_COUNT: usize = 8;

/// A hash other than the canonical `ContentId` that file content can be
/// looked up by. Its blob maps it to the content's `ContentId`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, EnumIter)]
pub enum Alias {
    Sha1(hash::Sha1),
    Sha256(hash::Sha256),
    GitSha1(hash::GitSha1),
    SeededBlake3(hash::Blake3),
}

impl Display for Alias {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sha1(sha1) => f.write_fmt(format_args!("{sha1:?}")),
            Self::Sha256(sha256) => f.write_fmt(format_args!("{sha256:?}")),
            Self::GitSha1(gitsha1) => f.write_fmt(format_args!("{gitsha1:?}")),
            Self::SeededBlake3(seeded_blake3) => f.write_fmt(format_args!("{seeded_blake3:?}")),
        }
    }
}

impl Alias {
    pub fn blobstore_key(&self) -> String {
        match self {
            Alias::GitSha1(git_sha1) => format!("alias.gitsha1.{}", git_sha1.to_hex()),
            Alias::Sha1(sha1) => format!("alias.sha1.{}", sha1.to_hex()),
            Alias::Sha256(sha256) => format!("alias.sha256.{}", sha256.to_hex()),
            Alias::SeededBlake3(blake3) => format!("alias.seeded_blake3.{}", blake3.to_hex()),
        }
    }

    #[inline]
    pub fn sampling_fingerprint(&self) -> u64 {
        match self {
            Alias::GitSha1(git_sha1) => git_sha1.sampling_fingerprint(),
            Alias::Sha1(sha1) => sha1.sampling_fingerprint(),
            Alias::Sha256(sha256) => sha256.sampling_fingerprint(),
            Alias::SeededBlake3(blake3) => blake3.sampling_fingerprint(),
        }
    }
}

#[async_trait]
impl Loadable for Alias {
    type Value = ContentId;

    async fn load<'a, B: KeyedBlobstore>(
        &'a self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Value, LoadableError> {
        let key = self.blobstore_key();
        let get = blobstore.get(ctx, &key);
        let maybe_alias = get
            .watched()
            .with_max_poll(blobstore::BLOBSTORE_MAX_POLL_TIME_MS)
            .await?;
        let blob = maybe_alias.ok_or_else(|| LoadableError::Missing(key.clone()))?;

        ContentAlias::from_bytes(blob.into_raw_bytes())
            .map(|alias| alias.content_id())
            .with_context(|| MononokeTypeError::BlobKeyError(key.clone()))
            .map_err(LoadableError::Error)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ContentAlias(ContentId);

impl ContentAlias {
    pub fn from_content_id(id: ContentId) -> Self {
        ContentAlias(id)
    }

    pub fn from_bytes(blob: Bytes) -> Result<Self> {
        let thrift_tc = compact_protocol::deserialize(blob.as_ref())
            .with_context(|| MononokeTypeError::BlobDeserializeError("ContentAlias".into()))?;
        Self::from_thrift(thrift_tc)
    }

    pub fn from_thrift(ca: thrift::content::ContentAlias) -> Result<Self> {
        match ca {
            thrift::content::ContentAlias::ContentId(id) => {
                Ok(Self::from_content_id(ContentId::from_thrift(id)?))
            }
            thrift::content::ContentAlias::UnknownField(x) => {
                bail!(MononokeTypeError::InvalidThrift(
                    "ContentAlias".into(),
                    format!("unknown content alias field: {x}")
                ))
            }
        }
    }

    pub fn into_blob(self) -> BlobstoreBytes {
        let alias = thrift::content::ContentAlias::ContentId(self.0.into_thrift());
        let data = compact_protocol::serialize(&alias);
        BlobstoreBytes::from_bytes(data)
    }

    pub fn content_id(&self) -> ContentId {
        self.0
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ContentMetadataV2 {
    pub content_id: ContentId,
    pub total_size: u64,
    pub sha1: hash::Sha1,
    pub sha256: hash::Sha256,
    pub git_sha1: hash::RichGitSha1,
    pub is_binary: bool,
    pub is_ascii: bool,
    pub is_utf8: bool,
    pub ends_in_newline: bool,
    pub newline_count: u64,
    pub first_line: Option<String>,
    pub is_generated: bool,
    pub is_partially_generated: bool,
    pub seeded_blake3: hash::Blake3,
}

impl ContentMetadataV2 {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let thrift_tc = compact_protocol::deserialize(bytes)
            .with_context(|| MononokeTypeError::BlobDeserializeError("ContentMetadataV2".into()))?;
        Self::from_thrift(thrift_tc)
    }

    pub fn from_thrift(metadata: thrift::content::ContentMetadataV2) -> Result<Self> {
        let newline_count = thrift_field!(ContentMetadataV2, metadata, newline_count)?;
        let newline_count: u64 = newline_count.try_into()?;

        let total_size = thrift_field!(ContentMetadataV2, metadata, total_size)?;
        let total_size: u64 = total_size.try_into()?;

        let res = ContentMetadataV2 {
            newline_count,
            total_size,
            sha1: hash::Sha1::from_bytes(&thrift_field!(ContentMetadataV2, metadata, sha1)?.0)?,
            sha256: hash::Sha256::from_bytes(
                &thrift_field!(ContentMetadataV2, metadata, sha256)?.0,
            )?,
            git_sha1: hash::RichGitSha1::from_bytes(
                &thrift_field!(ContentMetadataV2, metadata, git_sha1)?.0,
                "blob",
                total_size,
            )?,
            content_id: ContentId::from_thrift(thrift_field!(
                ContentMetadataV2,
                metadata,
                content_id
            )?)?,
            is_binary: thrift_field!(ContentMetadataV2, metadata, is_binary)?,
            is_ascii: thrift_field!(ContentMetadataV2, metadata, is_ascii)?,
            is_utf8: thrift_field!(ContentMetadataV2, metadata, is_utf8)?,
            ends_in_newline: thrift_field!(ContentMetadataV2, metadata, ends_in_newline)?,
            first_line: metadata.first_line,
            is_generated: thrift_field!(ContentMetadataV2, metadata, is_generated)?,
            is_partially_generated: thrift_field!(
                ContentMetadataV2,
                metadata,
                is_partially_generated
            )?,
            seeded_blake3: hash::Blake3::from_thrift(thrift_field!(
                ContentMetadataV2,
                metadata,
                seeded_blake3
            )?)?,
        };

        Ok(res)
    }

    fn into_thrift(self) -> thrift::content::ContentMetadataV2 {
        thrift::content::ContentMetadataV2 {
            content_id: Some(self.content_id.into_thrift()),
            newline_count: Some(self.newline_count as i64),
            total_size: Some(self.total_size as i64),
            is_binary: Some(self.is_binary),
            is_ascii: Some(self.is_ascii),
            is_utf8: Some(self.is_utf8),
            ends_in_newline: Some(self.ends_in_newline),
            sha1: Some(self.sha1.into_thrift()),
            git_sha1: Some(self.git_sha1.into_thrift()),
            sha256: Some(self.sha256.into_thrift()),
            first_line: self.first_line,
            is_generated: Some(self.is_generated),
            is_partially_generated: Some(self.is_partially_generated),
            seeded_blake3: Some(self.seeded_blake3.into_thrift()),
        }
    }
}

impl Arbitrary for ContentMetadataV2 {
    fn arbitrary(g: &mut Gen) -> Self {
        // Large u64 values can't be represented in thrift
        let total_size = u64::arbitrary(g) / 2;
        Self {
            total_size,
            newline_count: u64::arbitrary(g) / 2,
            content_id: ContentId::arbitrary(g),
            is_binary: bool::arbitrary(g),
            is_ascii: bool::arbitrary(g),
            is_utf8: bool::arbitrary(g),
            ends_in_newline: bool::arbitrary(g),
            sha1: hash::Sha1::arbitrary(g),
            sha256: hash::Sha256::arbitrary(g),
            git_sha1: hash::RichGitSha1::from_sha1(hash::GitSha1::arbitrary(g), "blob", total_size),
            first_line: Option::arbitrary(g),
            is_generated: bool::arbitrary(g),
            is_partially_generated: bool::arbitrary(g),
            seeded_blake3: hash::Blake3::arbitrary(g),
        }
    }
}

impl BlobstoreValue for ContentMetadataV2 {
    type Key = ContentMetadataV2Id;

    fn into_blob(self) -> ContentMetadataV2Blob {
        let id = From::from(self.content_id.clone());
        let thrift = self.into_thrift();
        let data = compact_protocol::serialize(&thrift);
        Blob::new(id, data)
    }

    fn from_blob(blob: ContentMetadataV2Blob) -> Result<Self> {
        let thrift_tc = compact_protocol::deserialize(blob.data().as_ref())
            .with_context(|| MononokeTypeError::BlobDeserializeError("ContentMetadataV2".into()))?;
        Self::from_thrift(thrift_tc)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PartialMetadata {
    pub is_binary: bool,
    pub is_ascii: bool,
    pub is_utf8: bool,
    pub ends_in_newline: bool,
    pub newline_count: u64,
    pub first_line: Option<String>,
    pub is_generated: bool,
    pub is_partially_generated: bool,
}

enum FoldState<T> {
    InProgress(T),
    Done(bool),
}

/// Computes if the entire stream of bytes is valid UTF-8 encoded.
pub async fn is_utf8(bytes_stream: impl Stream<Item = Bytes>) -> bool {
    let output = bytes_stream
        .fold(
            FoldState::InProgress(Bytes::new()),
            |acc, bytes| async move {
                match acc {
                    FoldState::Done(_) => acc,
                    FoldState::InProgress(ref rem_bytes) => {
                        let bytes = [rem_bytes, bytes.as_ref()].concat();
                        match std::str::from_utf8(bytes.as_ref()) {
                            // The entire chunk was valid UTF8, carry on to the next chunk.
                            Ok(_) => FoldState::InProgress(Bytes::new()),
                            Err(error) => {
                                let (_, invalid) = bytes.split_at(error.valid_up_to());
                                // If the length of invalid slice is more than a UTF8 codepoint
                                // then the file isn't UTF-8 encoded.
                                if invalid.len() > UTF8_BYTES_COUNT {
                                    FoldState::Done(false)
                                } else {
                                    // The remaining invalid bytes need to be carried over to the next
                                    // chunk to be concatenated with it.
                                    FoldState::InProgress(Bytes::copy_from_slice(invalid))
                                }
                            }
                        }
                    }
                }
            },
        )
        .await;
    match output {
        // Check continued till the last chunk. If the last chunk was valid UTF-8,
        // then 'bytes' would be empty and the entire file would be valid UTF-8
        FoldState::InProgress(ref bytes) => bytes.is_empty(),
        // The UTF8 check was completed before the last chunk with `status` value
        FoldState::Done(status) => status,
    }
}

/// Computes if the entire stream of bytes is valid ASCII.
pub async fn is_ascii(bytes_stream: impl Stream<Item = Bytes>) -> bool {
    // NOTE: This can be achieved in much shorter form by using short-circuiting
    // variants like 'all' or 'any'. However, that leads to Multiplexer error due
    // to the stream getting dropped prematurely.
    let output = bytes_stream
        .fold(FoldState::InProgress(true), |acc, bytes| async move {
            match acc {
                FoldState::Done(_) => acc,
                FoldState::InProgress(val) => {
                    let is_ascii = val && bytes.as_ref().iter().all(u8::is_ascii);
                    if !is_ascii {
                        FoldState::Done(false)
                    } else {
                        FoldState::InProgress(true)
                    }
                }
            }
        })
        .await;
    match output {
        FoldState::InProgress(val) => val,
        FoldState::Done(val) => val,
    }
}

/// Computes if the stream of bytes represents binary content
pub async fn is_binary(bytes_stream: impl Stream<Item = Bytes>) -> bool {
    let output = bytes_stream
        .fold(FoldState::InProgress(false), |acc, bytes| async move {
            match acc {
                FoldState::Done(_) => acc,
                FoldState::InProgress(val) => {
                    let is_binary = val || bytes.as_ref().contains(&b'\0');
                    FoldState::InProgress(is_binary)
                }
            }
        })
        .await;
    match output {
        FoldState::InProgress(val) => val,
        FoldState::Done(val) => val,
    }
}

/// Computes if the entire stream of bytes ends in a newline.
pub async fn ends_in_newline(bytes_stream: impl Stream<Item = Bytes>) -> bool {
    bytes_stream
        .fold(false, |acc, bytes| async move {
            match bytes.as_ref().last() {
                Some(&byte) => byte == b'\n',
                None => acc,
            }
        })
        .await
}

/// Computes the count of newline characters in the entire stream of bytes.
pub async fn newline_count(bytes_stream: impl Stream<Item = Bytes>) -> u64 {
    bytes_stream
        .fold(0, |acc, bytes| async move {
            acc + bytes
                .as_ref()
                .iter()
                .fold(0, |acc, &byte| if byte == b'\n' { acc + 1 } else { acc })
        })
        .await
}

/// Gets the first UTF-8 encoded line OR the first 64 bytes of data from the input
/// data stream, whichever is shortest. Note that in case of ASCII, the cut-off will
/// always be 64 bytes but for UTF-8 the cutoff can include upto 8 additional bytes.
pub async fn first_line(bytes_stream: impl Stream<Item = Bytes>) -> Option<String> {
    let line = bytes_stream
        .fold(
            (
                String::with_capacity(MAX_BYTES_FOR_FIRST_LINE),
                Bytes::new(),
                false,
            ),
            |(mut acc, mut prev_bytes, mut done), bytes| async move {
                // We already have the first line that we are looking for,
                // no need to look at further data.
                if done || acc.len() >= MAX_BYTES_FOR_FIRST_LINE {
                    (acc, prev_bytes, true)
                } else {
                    let bytes = [prev_bytes, bytes].concat();
                    let valid_line = match std::str::from_utf8(bytes.as_ref()) {
                        Ok(text) => {
                            // Check if there is already a newline in the parsed text. If there is, then
                            // we will get our required line by the end of this iteration.
                            done |= text.contains('\n');
                            // Since the entire text got parsed, no need for carry-over bytes.
                            prev_bytes = Bytes::new();
                            text.lines().next()
                        }
                        Err(error) => {
                            let (valid, invalid) = bytes.split_at(error.valid_up_to());
                            // If the length of invalid slice is more than a UTF8 codepoint
                            // then the file isn't UTF-8 encoded. If the file isn't UTF-8
                            // encoded, then first line should be None.
                            if invalid.len() > UTF8_BYTES_COUNT {
                                return (String::new(), Bytes::new(), true);
                            }
                            // We know that the slice is valid UTF-8 by this point, so safe to do the below.
                            let valid = unsafe { std::str::from_utf8_unchecked(valid) };
                            // The remaining bytes could not be parsed as valid UTF-8. They need to be carried
                            // over to the next iteration to check if combining them with the next chunk creates
                            // a valid string.
                            prev_bytes = Bytes::copy_from_slice(invalid);
                            done |= valid.contains('\n');
                            valid.lines().next()
                        }
                    };
                    let valid_line = match valid_line {
                        Some(line) => line,
                        None => return (String::new(), Bytes::new(), true),
                    };
                    let len_to_push =
                        std::cmp::min(MAX_BYTES_FOR_FIRST_LINE - acc.len(), valid_line.len());
                    // If the input is ASCII, we can cut off at 64 bytes to get a valid string. However, for
                    // UTF-8, 64 bytes might not be a valid char boundary so we may need to extend (at max 8 bytes).
                    let len_to_push = valid_line.ceil_char_boundary(len_to_push);
                    // Push only till the end of the line or till the end of buffer, whichever is the shortest.
                    acc.push_str(valid_line[..len_to_push].as_ref());
                    (acc, prev_bytes, done)
                }
            },
        )
        .await
        .0;
    if line.is_empty() { None } else { Some(line) }
}

async fn contains_marker(bytes_stream: impl Stream<Item = Bytes>, marker: &str) -> bool {
    let output = bytes_stream
        .fold(
            FoldState::InProgress(Bytes::new()),
            |acc, bytes| async move {
                match acc {
                    FoldState::Done(_) => acc,
                    FoldState::InProgress(ref rem_bytes) => {
                        // Before processing the current chunk, prepend the carry-on bytes from the last
                        // chunk to handle cases of the marker splitting across chunks.
                        let bytes = [rem_bytes, bytes.as_ref()].concat();
                        match std::str::from_utf8(bytes.as_ref()) {
                            Ok(content) => match content.contains(marker) {
                                // The marker is present in the file content. Consider this file
                                // as generated.
                                true => FoldState::Done(true),
                                // The marker was not present. Before processing the next chunk, we need to
                                // consider scenarios where the marker was split across chunks, for example
                                // [@gener]..[ated]. There are multiple variants of this split and to accommodate
                                // all cases, we will pick the last N chars where N = len of marker (i.e. skip
                                // first M - N chars where M is the number of chars in content) and carry it
                                // to the next chunk where it will be prepended before processing it.
                                false => {
                                    let chars = content.chars();
                                    let skip_count: i32 = std::cmp::max(
                                        chars.count() as i32 - marker.len() as i32,
                                        0,
                                    );
                                    let rem_str: String =
                                        content.chars().skip(skip_count as usize).collect();
                                    FoldState::InProgress(Bytes::copy_from_slice(
                                        rem_str.as_bytes(),
                                    ))
                                }
                            },
                            Err(error) => {
                                let (valid, invalid) = bytes.split_at(error.valid_up_to());
                                // We know that the slice is valid UTF-8 by this point, so safe to do the below.
                                let valid = unsafe { std::str::from_utf8_unchecked(valid) };
                                // If the length of invalid slice is more than a UTF8 codepoint
                                // then the file isn't UTF-8 encoded. A non-UTF-8 file cannot
                                // be parsed for checking the presence of marker.
                                if invalid.len() > UTF8_BYTES_COUNT {
                                    FoldState::Done(false)
                                // The UTF-8 valid part of the content contains the required
                                // marker. The check is completed successfully.
                                } else if valid.contains(marker) {
                                    FoldState::Done(true)
                                } else {
                                    // The remaining invalid bytes need to be carried over to the next
                                    // chunk to be concatenated with it.
                                    FoldState::InProgress(Bytes::copy_from_slice(invalid))
                                }
                            }
                        }
                    }
                }
            },
        )
        .await;
    match output {
        // Check continued till the last chunk and the generated marker was not found
        // till the last chunk. Return false.
        FoldState::InProgress(_) => false,
        // Check completed with either true or false status. Return the status.
        FoldState::Done(status) => status,
    }
}

pub async fn is_generated(bytes_stream: impl Stream<Item = Bytes>) -> bool {
    contains_marker(bytes_stream, concat!("@", "generated")).await
}

pub async fn is_partially_generated(bytes_stream: impl Stream<Item = Bytes>) -> bool {
    contains_marker(bytes_stream, concat!("@", "partially-generated")).await
}

#[cfg(test)]
mod test {
    use futures::future;
    use futures::stream;
    use quickcheck::quickcheck;
    use rand::RngExt as _;
    use rand::distr::Alphanumeric;
    use rand::distr::Distribution;
    use rand::distr::StandardUniform;

    use super::*;

    quickcheck! {
        fn content_metadata_v2_thrift_roundtrip(metadata: ContentMetadataV2) -> bool {
            let thrift_metadata = metadata.clone().into_thrift();
            let from_thrift_metadata = ContentMetadataV2::from_thrift(thrift_metadata)
                .expect("thrift roundtrips should always be valid");
            println!("metadata: {:?}", metadata);
            println!("metadata_from_thrift: {:?}", from_thrift_metadata);
            metadata == from_thrift_metadata
        }

        fn content_metadata_v2_blob_roundtrip(metadata: ContentMetadataV2) -> bool {
            let blob = metadata.clone().into_blob();
            let metadata_from_blob = ContentMetadataV2::from_blob(blob)
                .expect("blob roundtrips should always be valid");
            metadata == metadata_from_blob
        }
    }

    // is_ascii tests
    #[tokio::test]
    async fn basic_is_ascii_test() {
        let input = "This is a sample ASCII_string@#$()&^/';[]`~*";
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(
            is_ascii(bytes_stream).await,
            "The input '{input}' wasn't ASCII"
        )
    }

    #[tokio::test]
    async fn negative_is_ascii_test() {
        let input = "यह एक नमूना गैर-ASCII स्ट्रिंग है";
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(
            !is_ascii(bytes_stream).await,
            "The input '{input}' was ASCII"
        )
    }

    #[tokio::test]
    async fn single_non_ascii_codepoint_test() {
        let input = "This is almost an ASCII ह string";
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(
            !is_ascii(bytes_stream).await,
            "The input '{input}' was ASCII"
        )
    }

    #[tokio::test]
    async fn arbitrary_is_ascii_test() {
        let bytes = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(1024)
            .collect::<Bytes>();
        let bytes_stream = stream::once(future::ready(bytes));
        assert!(is_ascii(bytes_stream).await);
    }

    #[tokio::test]
    async fn arbitrary_stream_is_ascii_test() {
        let mut rng = rand::rng();
        let bytes_stream = stream::iter((0..50).map(|_| {
            let chunk_size: usize = rng.random_range(20..50);
            Alphanumeric
                .sample_iter(&mut rng)
                .take(chunk_size)
                .collect::<Bytes>()
        }));
        assert!(is_ascii(bytes_stream).await);
    }

    #[tokio::test]
    async fn single_string_single_stream_is_ascii_test() {
        let mut rng = rand::rng();
        let bytes = Alphanumeric
            .sample_iter(&mut rng)
            .take(4096)
            .collect::<Bytes>();
        let bytes_stream = stream::iter(bytes.chunks(37).map(Bytes::copy_from_slice));
        assert!(is_ascii(bytes_stream).await);
    }

    #[tokio::test]
    async fn single_string_multiple_stream_is_ascii_test() {
        let mut rng = rand::rng();
        let bytes = Alphanumeric
            .sample_iter(&mut rng)
            .take(4096)
            .collect::<Bytes>();
        for chunk in [230, 10, 35, 89, 1000] {
            let bytes_stream = stream::iter(bytes.chunks(chunk).map(Bytes::copy_from_slice));
            assert!(is_ascii(bytes_stream).await);
        }
    }

    // is_utf8 tests
    #[tokio::test]
    async fn basic_is_utf8_test() {
        let input =
            "This is a sample UTF8 encoded _string_ @#$()&^/';[]`~*. यह एक नमूना UTF8 स्ट्रिंग है 😋";
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(
            is_utf8(bytes_stream).await,
            "The input '{input}' wasn't UTF8"
        )
    }

    #[tokio::test]
    async fn negative_is_utf8_test() {
        let bytes = b"C\xF4te d'Ivoire";
        let bytes_stream = stream::once(future::ready(Bytes::from_static(bytes)));
        assert!(!is_utf8(bytes_stream).await)
    }

    #[tokio::test]
    async fn arbitrary_is_utf8_test() {
        let bytes = Bytes::from(
            rand::rng()
                .sample_iter::<char, _>(&StandardUniform)
                .take(1024)
                .collect::<String>(),
        );
        let bytes_stream = stream::once(future::ready(bytes));
        assert!(is_utf8(bytes_stream).await);
    }

    #[tokio::test]
    async fn arbitrary_negative_is_utf8_test() {
        let bytes = rand::rng()
            .sample_iter(&StandardUniform)
            .take(1024)
            .collect::<Bytes>();
        let bytes_stream = stream::once(future::ready(bytes));
        let bytes_stream = bytes_stream.chain(stream::once(future::ready(Bytes::from_static(
            b"C\xF4te d'Ivoire",
        ))));
        assert!(!is_utf8(bytes_stream).await);
    }

    #[tokio::test]
    async fn arbitrary_stream_is_utf8_test() {
        let bytes_stream = stream::iter((0..50).map(|_| {
            let chunk_size: usize = rand::random_range(20..50);
            Bytes::from(
                rand::rng()
                    .sample_iter::<char, _>(&StandardUniform)
                    .take(chunk_size)
                    .collect::<String>(),
            )
        }));
        assert!(is_utf8(bytes_stream).await);
    }

    #[tokio::test]
    async fn single_string_single_stream_is_utf8_test() {
        let bytes = Bytes::from(
            rand::rng()
                .sample_iter::<char, _>(&StandardUniform)
                .take(4096)
                .collect::<String>(),
        );
        let bytes_stream = stream::iter(bytes.chunks(37).map(Bytes::copy_from_slice));
        assert!(is_utf8(bytes_stream).await);
    }

    #[tokio::test]
    async fn single_string_multiple_stream_is_utf8_test() {
        let bytes = Bytes::from(
            rand::rng()
                .sample_iter::<char, _>(&StandardUniform)
                .take(4096)
                .collect::<String>(),
        );
        for chunk in [230, 10, 35, 89, 1000] {
            let bytes_stream = stream::iter(bytes.chunks(chunk).map(Bytes::copy_from_slice));
            assert!(is_utf8(bytes_stream).await);
        }
    }

    // ends_in_newline tests
    #[tokio::test]
    async fn basic_ends_in_newline_test() {
        let input = "Random string ending in newline\n";
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(ends_in_newline(bytes_stream).await);
    }

    #[tokio::test]
    async fn negative_ends_in_newline_test() {
        let input = "Just some string";
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(!ends_in_newline(bytes_stream).await);
    }

    #[tokio::test]
    async fn non_ending_newlines_test() {
        let input = "\nThere are \n newlines in \n this string \nbut not at the en\nd";
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(!ends_in_newline(bytes_stream).await);
    }

    #[tokio::test]
    async fn ends_in_newline_with_stream_test() {
        let bytes_stream = stream::iter(
            ["This is a", " broken up", " string that ends in newline\n"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(ends_in_newline(bytes_stream).await);
    }

    #[tokio::test]
    async fn ends_in_newline_with_invalid_stream_test() {
        let bytes_stream = stream::iter(
            [
                "Each chunk\n",
                " of this string\n",
                " ends in newline\n",
                " except the last",
            ]
            .into_iter()
            .map(Bytes::from),
        );
        assert!(!ends_in_newline(bytes_stream).await);
    }

    #[tokio::test]
    async fn ends_in_newline_with_arbitrary_non_ascii_stream_test() {
        let bytes_stream = stream::iter(
            [
                "इस पाठ का प्रत्येक हिस्सा",
                "अंग्रेजी वाक्य नहीं है",
                "इसलिए इसमें कोई न्यूलाइन नहीं होनी चाहिए।",
            ]
            .into_iter()
            .map(Bytes::from),
        );
        assert!(!ends_in_newline(bytes_stream).await);
    }

    // newline_count tests
    #[tokio::test]
    async fn basic_newline_count_test() {
        let input = "Random\n string with\n newline\n embedded in bet\nween\n";
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert_eq!(5, newline_count(bytes_stream).await, "Expected 5 newlines");
    }

    #[tokio::test]
    async fn no_newline_count_test() {
        let input = "Random string with no newlines";
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert_eq!(0, newline_count(bytes_stream).await, "Expected 0 newlines");
    }

    #[tokio::test]
    async fn stream_newline_count_test() {
        let bytes_stream = stream::iter(
            [
                "This chunk has\n newline",
                "This chunk doesn't",
                "Neither does this",
                "This\n one\n has\n four\n",
                "\n",
            ]
            .into_iter()
            .map(Bytes::from),
        );
        assert_eq!(6, newline_count(bytes_stream).await, "Expected 6 newlines");
    }

    #[tokio::test]
    async fn no_newline_count_stream_test() {
        let bytes_stream = stream::iter(
            ["No", "newlines", "in", "this", "stream"]
                .into_iter()
                .map(Bytes::from),
        );
        assert_eq!(0, newline_count(bytes_stream).await, "Expected 0 newlines");
    }

    // is_binary tests
    #[tokio::test]
    async fn basic_is_binary_test() {
        let input = b"Binary input with \0 byte";
        let bytes_stream = stream::once(future::ready(Bytes::from_static(input)));
        assert!(is_binary(bytes_stream).await);
    }

    #[tokio::test]
    async fn negative_is_binary_test() {
        let input = b"Just a regular string";
        let bytes_stream = stream::once(future::ready(Bytes::from_static(input)));
        assert!(!is_binary(bytes_stream).await);
    }

    #[tokio::test]
    async fn stream_is_binary_test() {
        let bytes_stream = stream::iter(
            [
                b"This is just text",
                b"So is this       ",
                b"But not \0 this   ",
                b"not even \0 this  ",
            ]
            .iter()
            .map(|&b| Bytes::from_static(b)),
        );
        assert!(is_binary(bytes_stream).await);
    }

    #[tokio::test]
    async fn stream_negative_is_binary_test() {
        let bytes_stream = stream::iter(
            [
                b"This is just text",
                b"This is just text",
                b"This is just text",
                b"This is just text",
            ]
            .iter()
            .map(|&b| Bytes::from_static(b)),
        );
        assert!(!is_binary(bytes_stream).await);
    }

    // is_generated tests
    #[tokio::test]
    async fn basic_is_generated_test() {
        let input = concat!("Random string with @", "generated tag within it");
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(is_generated(bytes_stream).await);
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(!is_partially_generated(bytes_stream).await);
    }

    #[tokio::test]
    async fn negative_is_generated_test() {
        let input = "Random string without any generated tag";
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(!is_generated(bytes_stream).await);
    }

    #[tokio::test]
    async fn almost_is_generated_test() {
        // @lint-ignore SPELL
        let input = "@,generated @ generated @generate d @generatd @\0generated @\ngenerated @geenerated @Generated @geneRateD";
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(!is_generated(bytes_stream).await);
    }

    #[tokio::test]
    async fn stream_is_generated_test() {
        let bytes_stream = stream::iter(
            [
                "This chunk has no marker",
                "Neither does this chunk",
                "But the last chunk in this list",
                concat!("has @", "generated marker in it"),
            ]
            .into_iter()
            .map(Bytes::from),
        );
        assert!(is_generated(bytes_stream).await);
    }

    #[tokio::test]
    async fn stream_negative_is_generated_test() {
        let bytes_stream = stream::iter(
            [
                "This chunk has no marker",
                "Neither does this chunk",
                "But the last chunk in this list",
                "Also doesn't have any marker",
            ]
            .into_iter()
            .map(Bytes::from),
        );
        assert!(!is_generated(bytes_stream).await);
    }

    #[tokio::test]
    async fn broken_stream_is_generated_test() {
        let bytes_stream = stream::iter(
            ["This chunk has @gene", "rated marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(is_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            ["This chunk has @ge", "nerated marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(is_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            ["This chunk has @", "generated marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(is_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            ["This chunk has @generate", "d marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(is_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            ["This chunk has @ge", "ne", "rate", "d marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(is_generated(bytes_stream).await);

        let bytes_stream = stream::iter([
            "A much longer string that has the initial part of the required marker @",
            "generated in it. This check validates that even for longer strings, inter-chunk marker check succeeds",
        ].into_iter()
        .map(Bytes::from)
    );
        assert!(is_generated(bytes_stream).await);
    }

    #[tokio::test]
    async fn broken_stream_negative_is_generated_test() {
        let bytes_stream = stream::iter(
            ["This chunk has @gene", " rated marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(!is_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            ["This chunk has @gen", "nerated marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(!is_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            ["This chunk has @", "g", "generated marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(!is_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            ["This chunk has @generatde", "d marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(!is_generated(bytes_stream).await);
    }

    // is_partially_generated tests
    #[tokio::test]
    async fn basic_is_partially_generated_test() {
        let input = concat!("Random string with @", "partially-generated tag within it");
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(!is_generated(bytes_stream).await);
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(is_partially_generated(bytes_stream).await);
    }

    #[tokio::test]
    async fn negative_is_partially_generated_test() {
        let input = "Random string without any generated tag";
        let bytes_stream = stream::once(future::ready(Bytes::from(input)));
        assert!(!is_partially_generated(bytes_stream).await);
    }

    #[tokio::test]
    async fn broken_stream_is_partially_generated_test() {
        let bytes_stream = stream::iter(
            ["This chunk has @partially-", "generated marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(is_partially_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            ["This chunk has @pa", "rtially-generated marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(is_partially_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            ["This chunk has @", "partially-generated marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(is_partially_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            ["This chunk has @partially-generate", "d marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(is_partially_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            [
                "This chunk has @pa",
                "rti",
                "ally",
                "-",
                "gen",
                "erat",
                "ed",
                " marker in it",
            ]
            .into_iter()
            .map(Bytes::from),
        );
        assert!(is_partially_generated(bytes_stream).await);

        let bytes_stream = stream::iter([
            "A much longer string that has the initial part of the required marker @",
            "partially-generated in it. This check validates that even for longer strings, inter-chunk marker check succeeds",
        ].into_iter()
        .map(Bytes::from)
    );
        assert!(is_partially_generated(bytes_stream).await);
    }

    #[tokio::test]
    async fn broken_stream_with_empty_string_is_partially_generated_test() {
        let bytes_stream = stream::iter(
            [
                "This chunk has @pa",
                "rti",
                "",
                "ally",
                "-",
                "",
                "gen",
                "erat",
                "",
                "ed",
                " marker in it",
            ]
            .into_iter()
            .map(Bytes::from),
        );
        assert!(is_partially_generated(bytes_stream).await);
    }

    #[tokio::test]
    async fn broken_stream_negative_is_partially_generated_test() {
        let bytes_stream = stream::iter(
            ["This chunk has @partially-gene", " rated marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(!is_partially_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            ["This chunk has @partially-gen", "nerated marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(!is_partially_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            ["This chunk has @", "p", "partially-generated marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(!is_partially_generated(bytes_stream).await);

        let bytes_stream = stream::iter(
            // @lint-ignore SPELL
            ["This chunk has @partially-genarate", "d marker in it"]
                .into_iter()
                .map(Bytes::from),
        );
        assert!(!is_partially_generated(bytes_stream).await);
    }

    // first_line tests
    #[tokio::test]
    async fn basic_first_line_test() {
        let bytes_stream = stream::once(future::ready(Bytes::from(
            "This text has two lines.\nThis is the second line.",
        )));
        assert_eq!(
            Some("This text has two lines.".to_string()),
            first_line(bytes_stream).await
        );
    }

    #[tokio::test]
    async fn negative_first_line_test() {
        let bytes = b"C\xF4te d'Ivoire";
        let bytes_stream = stream::once(future::ready(Bytes::from_static(bytes)));
        assert_eq!(None, first_line(bytes_stream).await)
    }

    #[tokio::test]
    async fn too_long_without_newline_first_line_test() {
        let text = "This text is basically a very long single line without any newline characters. First line is supposed to return the part till the first newline character or the first 64 bytes.";
        let bytes_stream = stream::once(future::ready(Bytes::from(text)));
        assert_eq!(
            Some("This text is basically a very long single line without any newli".to_string()),
            first_line(bytes_stream).await
        );
    }

    #[tokio::test]
    async fn too_long_without_newline_non_ascii_first_line_test() {
        let text = "यह पाठ मूल रूप से बिना किसी न्यूलाइन वर्ण के एक बहुत लंबी एकल पंक्ति है। पहली पंक्ति को पहले न्यूलाइन वर्ण या पहले 64 बाइट्स तक भाग वापस करना चाहिए।";
        let bytes_stream = stream::once(future::ready(Bytes::from(text)));
        assert_eq!(
            Some("यह पाठ मूल रूप से बिना किस".to_string()),
            first_line(bytes_stream).await
        );
    }

    #[tokio::test]
    async fn too_long_with_newline_first_line_test() {
        let text = "This text is basically a very\n long single line, first line is supposed to return the part till the first newline character or the first 64 bytes.";
        let bytes_stream = stream::once(future::ready(Bytes::from(text)));
        assert_eq!(
            Some("This text is basically a very".to_string()),
            first_line(bytes_stream).await
        );
    }

    #[tokio::test]
    async fn too_long_with_newline_non_ascii_first_line_test() {
        let text = "यह पाठ\n मूल रूप से बिना किसी न्यूलाइन वर्ण के एक बहुत लंबी एकल पंक्ति है। पहली पंक्ति को पहले न्यूलाइन वर्ण या पहले 64 बाइट्स तक भाग वापस करना चाहिए।";
        let bytes_stream = stream::once(future::ready(Bytes::from(text)));
        assert_eq!(Some("यह पाठ".to_string()), first_line(bytes_stream).await);
    }

    #[tokio::test]
    async fn ascii_stream_without_newline_first_line_test() {
        let bytes_stream = stream::iter(
            [
                "A short chunk",
                " followed by another chunk",
                " and then yet another chunk",
                " followed by a final chunk.",
            ]
            .into_iter()
            .map(Bytes::from),
        );
        assert_eq!(
            Some("A short chunk followed by another chunk and then yet another chu".to_string()),
            first_line(bytes_stream).await
        );
    }

    #[tokio::test]
    async fn ascii_stream_with_newline_first_line_test() {
        let bytes_stream = stream::iter(
            [
                "A short chunk",
                " followed by another\n chunk",
                " and then yet another chunk",
                " followed by a final chunk.",
            ]
            .into_iter()
            .map(Bytes::from),
        );
        assert_eq!(
            Some("A short chunk followed by another".to_string()),
            first_line(bytes_stream).await
        );
    }

    #[tokio::test]
    async fn utf8_stream_without_newline_first_line_test() {
        let bytes_stream = stream::iter(
            [
                "यह पाठ मूल रूप",
                " से बिना किसी न्यूलाइन",
                " वर्ण के एक बहुत",
                " लंबी एकल पंक्ति है।",
            ]
            .into_iter()
            .map(Bytes::from),
        );
        assert_eq!(
            Some("यह पाठ मूल रूप से बिना किस".to_string()),
            first_line(bytes_stream).await
        );
    }

    #[tokio::test]
    async fn utf8_chunked_stream_without_newline_first_line_test() {
        let bytes_stream = stream::iter(
            [
                b"This buffer has a UTF-8 code point that overlaps the first buff\xc3",
                b"\xa9r's end and the second buffer's start. Let's see what is output",
            ]
            .iter()
            .map(|&b| Bytes::from_static(b)),
        );
        assert_eq!(
            Some("This buffer has a UTF-8 code point that overlaps the first buffé".to_string()),
            first_line(bytes_stream).await
        );
    }

    #[tokio::test]
    async fn utf8_stream_with_newline_first_line_test() {
        let bytes_stream = stream::iter(
            [
                "यह पाठ मूल रूप",
                " से\n बिना किसी न्यूलाइन",
                " वर्ण के एक\n बहुत",
                " लंबी एकल पंक्ति है।",
            ]
            .into_iter()
            .map(Bytes::from),
        );
        assert_eq!(
            Some("यह पाठ मूल रूप से".to_string()),
            first_line(bytes_stream).await
        );
    }

    #[tokio::test]
    async fn non_utf8_letter_in_stream_after_first_line_test() {
        let bytes = b"Only the last part of this string has non-utf8 characters. But the earlier part of the string has valid encoding. C\xF4te d'Ivoire";
        let bytes_stream = stream::iter(bytes.chunks(10).map(Bytes::from_static));
        assert_eq!(
            Some("Only the last part of this string has non-utf8 characters. But t".to_string()),
            first_line(bytes_stream).await
        )
    }

    #[tokio::test]
    async fn non_utf8_letter_in_stream_before_first_line_test() {
        let bytes = b"The first part of the string, C\xF4te d'Ivoire, contains invalid characters. The rest of the string is valid UTF-8";
        let bytes_stream = stream::iter(bytes.chunks(10).map(Bytes::from_static));
        assert_eq!(None, first_line(bytes_stream).await)
    }
}
