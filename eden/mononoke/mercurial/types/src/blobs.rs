/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod envelope;
pub use envelope::HgBlobEnvelope;

mod errors;
pub use errors::MononokeHgBlobError;

pub mod file;
pub use file::File;
pub use file::LFSContent;
pub use file::META_MARKER;
pub use file::META_SZ;

mod manifest;
pub use self::manifest::HgBlobManifest;
pub use self::manifest::ManifestContent;
pub use self::manifest::fetch_manifest_envelope;
pub use self::manifest::fetch_manifest_envelope_opt;
pub use self::manifest::fetch_raw_manifest_bytes;
pub use self::manifest::hg_manifests_read_from_augmented;

mod changeset;
pub use changeset::ChangesetMetadata;
pub use changeset::Extra;
pub use changeset::HgBlobChangeset;
pub use changeset::HgChangesetContent;
pub use changeset::RevlogChangeset;
pub use changeset::SNAPSHOT_METADATA_KEY;
pub use changeset::serialize_cs;
pub use changeset::serialize_extras;
pub use changeset::snapshot_state;

pub mod filenode_lookup;

mod upload;
pub use upload::ContentBlobMeta;
pub use upload::UploadHgFileContents;
pub use upload::UploadHgFileEntry;
pub use upload::UploadHgNodeHash;
pub use upload::UploadHgTreeEntry;
pub use upload::store_manifest_envelope;
