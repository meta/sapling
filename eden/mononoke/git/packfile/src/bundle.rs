/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt::Display;

use anyhow::Result;
use futures::Stream;
use futures::TryStreamExt;
use git_types::PackfileItem;
use gix_hash::ObjectId;
use sha1::Digest;
use sha1::Sha1;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use weight_observer::WeightedItem;

use crate::pack::DeltaForm;
use crate::pack::PackfileWriter;

/// The message/comment associated with the pre-requisite objects
const BUNDLE_PREREQ_MSG: &str = "bundled object";

/// Enum representing the supported bundle versions
/// Currently only version 2 is supported.
pub enum BundleVersion {
    V2,
}

/// Enum representing the naming strategy for the refs in the bundle
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefNaming {
    AsIs,
    RenameToHeads,
}

impl Display for BundleVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BundleVersion::V2 => f.write_str("# v2 git bundle\n"),
        }
    }
}

/// Struct responsible for writing a Git bundle with format <https://git-scm.com/docs/bundle-format>
/// to the underlying writer.
pub struct BundleWriter<T>
where
    T: AsyncWrite + Unpin,
{
    /// List of objects that are NOT included in the bundle but are
    /// required to be present for unbundling to work.
    pub prereqs: Vec<ObjectId>,
    /// The version of bundle format
    pub version: BundleVersion,
    /// List of ref-names with the commits IDs that they point to along with
    /// optional metadata associated to the refs
    pub refs: Vec<(String, ObjectId)>,
    /// Packfile writer created over the underlying raw writer
    pub pack_writer: PackfileWriter<T>,
    /// Number of bytes written to the underlying writer
    bytes_written: usize,
    /// Fingerprint of the bundle based on the included refs in sorted order
    bundle_fingerprint: String,
}

impl<T: AsyncWrite + Unpin> BundleWriter<T> {
    /// Create a new BundleWriter instance with the header of the bundle written to the
    /// underlying writer.
    pub async fn new_with_header(
        mut writer: T,
        refs: Vec<(String, ObjectId)>,
        prereqs: Vec<ObjectId>,
        num_objects: u32,
        concurrency: usize,
        delta_form: DeltaForm,
        ref_naming: RefNaming,
    ) -> Result<Self> {
        // Git can't parse tag and remote refs in bundles, so we need to replace them with heads
        // TODO(rajshar): Remove this once Git version in Meta includes https://github.com/git/git/pull/1897
        let mut refs: Vec<(String, ObjectId)> = match ref_naming {
            RefNaming::AsIs => refs,
            RefNaming::RenameToHeads => refs
                .into_iter()
                .map(|(ref_name, ref_target)| {
                    let ref_name = ref_name.replacen("refs/tags/", "refs/heads/replaced_tags/", 1);
                    let ref_name =
                        ref_name.replacen("refs/remotes/", "refs/heads/replaced_remotes/", 1);
                    (ref_name, ref_target)
                })
                .collect(),
        };
        refs.sort_by(|a, b| a.0.cmp(&b.0));
        // Calculate the bundle fingerprint based on the included refs to uniquely identify the bundle
        let bundle_fingerprint = calculate_bundle_fingerprint(&refs);

        let mut bytes_written = 0;
        // Append the bundle header
        let bundle_header = format!("{}", BundleVersion::V2);
        writer.write_all(bundle_header.as_bytes()).await?;
        bytes_written += bundle_header.len();

        // Append the pre-requisite objects, if present
        for prereq in prereqs.iter() {
            let line = format!("-{prereq} {BUNDLE_PREREQ_MSG}\n");
            writer.write_all(line.as_bytes()).await?;
            bytes_written += line.len();
        }
        // Append the refs
        for (ref_name, id) in &refs {
            let line = format!("{id} {ref_name}\n");
            writer.write_all(line.as_bytes()).await?;
            bytes_written += line.len();
        }
        // Newline before starting packfile
        writer.write_all(b"\n").await?;
        bytes_written += 1;
        let pack_writer = PackfileWriter::new(writer, num_objects, concurrency, delta_form, None);
        Ok(Self {
            version: BundleVersion::V2,
            refs,
            prereqs,
            pack_writer,
            bytes_written,
            bundle_fingerprint,
        })
    }

    /// Write the stream of input items to the bundle
    pub async fn write(
        &mut self,
        objects_stream: impl Stream<Item = Result<WeightedItem<PackfileItem>>>,
    ) -> Result<()> {
        self.pack_writer.write(objects_stream).await
    }

    /// Write the stream of unweighted items to the bundle
    pub async fn write_unweighted(
        &mut self,
        objects_stream: impl Stream<Item = Result<PackfileItem>>,
    ) -> Result<()> {
        self.pack_writer
            .write(objects_stream.map_ok(WeightedItem::untracked))
            .await
    }

    /// Returns the number of bytes written to the underlying writer
    pub fn bytes_written(&self) -> usize {
        self.bytes_written + self.pack_writer.size as usize
    }

    /// Returns the bundle fingerprint
    pub fn bundle_fingerprint(&self) -> &str {
        &self.bundle_fingerprint
    }

    /// Finish the bundle and flush it to the underlying writer
    /// returning the checksum of the written packfile
    pub async fn finish(&mut self) -> Result<ObjectId> {
        self.pack_writer.finish().await
    }

    /// Consumes the instance after writing the bundle and returns
    /// the underlying raw writer.
    pub fn into_write(self) -> T {
        self.pack_writer.into_write()
    }
}

fn calculate_bundle_fingerprint(refs: &Vec<(String, ObjectId)>) -> String {
    let mut hasher = Sha1::new();
    for (r, id) in refs {
        hasher.update(r);
        hasher.update(id.as_bytes());
    }
    // Return the hash as a hexadecimal string
    format!("{:x}", hasher.finalize())
}
