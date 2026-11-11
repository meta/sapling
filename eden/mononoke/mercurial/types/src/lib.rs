/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Mercurial Types
//!
//! This crate contains useful definitions for types that occur in Mercurial. Or more generally,
//! in a source control system that is based on Mercurial and extensions.
//!
//! The top-most level is the Repo, which is a container for changesets.
//!
//! A changeset represents a snapshot of a file tree at a specific moment in time. Changesets
//! can (and commonly do) have parent-child relationships with other changesets; if once changeset
//! is the child of another one, then it is interpreted as an incremental change in the history of
//! a single namespace. Changesets can have multiple parents (currently limited to 2), which
//! represents the merging of history. A changeset can have no parents, which represents the
//! creation of a new namespace. There's no requirement that all (or any) changeset within a
//! repo be connected at all via parent-child relationships.
//!
//! Each changeset has a tree of manifests, which represent their namespace. A manifest is
//! equivalent to a directory in a filesystem, mapping names to other objects. Those other
//! objects can be other manifests (subdirectories), files, or symlinks. Manifest objects can
//! be shared by multiple changesets - if the only difference between two changesets is a
//! single file, then all other files and directories will be the same and shared.
//!
//! Changesets, manifests and files are uniformly represented by a `Node`. A `Node` has
//! 0-2 parents and some content. A node's identity is computed by hashing over (p1, p2, content),
//! resulting in `HgNodeHash` (TODO: rename HgNodeHash -> NodeId?). This means manifests and files
//! have a notion of history independent of the changeset(s) they're embedded in.
//!
//! Nodes are stored as blobs in the blobstore, but with their content in a separate blob. This
//! is because it's very common for the same file content to appear either under different names
//! (copies) or multiple times within the same history (reverts), or both (rebase, amend, etc).
//!
//! Blobs are the underlying raw storage for all immutable objects in Mononoke. Their primary
//! storage key is a hash (TBD, stronger than SHA1) over their raw bit patterns, but they can
//! have other keys to allow direct access via multiple aliases. For example, file content may be
//! shared by multiple nodes, but can be access directly without having to go via a node.
//!
//! Delta and bdiff are used in revlogs and on the wireprotocol to represent inter-file
//! differences. These are for interfacing at the edges, but are not used within Mononoke's core
//! structures at all.

pub mod bdiff;
pub mod blob;
pub mod blobnode;
pub mod blobs;
pub mod delta;
pub mod delta_apply;
pub mod envelope;
pub mod errors;
pub mod file;
pub mod flags;
pub mod fsencode;
pub mod manifest;
pub mod nodehash;
pub mod preloaded_augmented_manifest;
pub mod remotefilelog;
pub mod sharded_augmented_manifest;
pub mod snapshot;
pub mod sql_types;
pub mod subtree;
pub mod utils;

// Re-exports from mononoke_types. Eventually these should go away and everything should depend
// directly on mononoke_types;
pub use mononoke_types::FileType;
pub use mononoke_types::Globalrev;
pub use mononoke_types::MPathElement;
pub use mononoke_types::NonRootMPath;
pub use mononoke_types::RepoPath;
pub use mononoke_types::path::MPath;
pub use mononoke_types::sha1_hash;

pub use crate::blob::HgBlob;
pub use crate::blobnode::HgBlobNode;
pub use crate::blobnode::HgParents;
pub use crate::blobnode::calculate_hg_node_id;
pub use crate::blobnode::calculate_hg_node_id_stream;
pub use crate::blobnode::calculate_hg_node_ids_multi;
pub use crate::blobs::HgBlobEnvelope;
pub use crate::blobs::fetch_manifest_envelope;
pub use crate::blobs::fetch_manifest_envelope_opt;
pub use crate::blobs::fetch_raw_manifest_bytes;
pub use crate::delta::Delta;
pub use crate::envelope::HgChangesetEnvelope;
pub use crate::envelope::HgChangesetEnvelopeMut;
pub use crate::envelope::HgFileEnvelope;
pub use crate::envelope::HgFileEnvelopeMut;
pub use crate::envelope::HgManifestEnvelope;
pub use crate::envelope::HgManifestEnvelopeMut;
pub use crate::errors::MononokeHgError;
pub use crate::file::FileBytes;
pub use crate::flags::RevFlags;
pub use crate::flags::parse_rev_flags;
pub use crate::fsencode::fncache_fsencode;
pub use crate::fsencode::simple_fsencode;
pub use crate::manifest::Type;
pub use crate::nodehash::HgAugmentedManifestId;
pub use crate::nodehash::HgChangesetId;
pub use crate::nodehash::HgChangesetIdPrefix;
pub use crate::nodehash::HgChangesetIdsResolvedFromPrefix;
pub use crate::nodehash::HgEntryId;
pub use crate::nodehash::HgFileNodeId;
pub use crate::nodehash::HgManifestId;
pub use crate::nodehash::HgNodeHash;
pub use crate::nodehash::HgNodeKey;
pub use crate::nodehash::NULL_CSID;
pub use crate::nodehash::NULL_HASH;
pub use crate::preloaded_augmented_manifest::HgPreloadedAugmentedManifest;
pub use crate::remotefilelog::HgFileHistoryEntry;
pub use crate::remotefilelog::convert_parents_to_remotefilelog_format;
pub use crate::sharded_augmented_manifest::HgAugmentedManifestEntry;
pub use crate::sharded_augmented_manifest::HgAugmentedManifestEnvelope;
pub use crate::sharded_augmented_manifest::ShardedHgAugmentedManifest;
pub use crate::sharded_augmented_manifest::fetch_augmented_manifest_envelope_opt;
pub use crate::utils::percent_encode;

#[cfg(test)]
mod test;

mod thrift {
    pub use mercurial_thrift::*;
    #[cfg(test)]
    pub use mononoke_types_serialization::id;
}
