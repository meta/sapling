/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Base types used throughout Mononoke.
#![feature(debug_closure_helpers)]

pub mod acl_manifest;
pub mod basename_suffix_skeleton_manifest_v3;
pub mod blame_v2;
pub mod blame_v3;
pub mod blob;
pub mod bonsai_changeset;
pub mod case_conflict_skeleton_manifest;
pub mod content_chunk;
pub mod content_manifest;
pub mod content_metadata_v2;
pub mod datetime;
pub mod debug;
pub mod deleted_manifest_common;
pub mod deleted_manifest_v2;
pub mod derivable_type;
pub mod directory_branch_cluster_manifest;
pub mod errors;
pub mod fastlog_batch;
pub mod file_change;
pub mod file_contents;
pub mod fsnode;
pub mod generation;
pub mod globalrev;
pub mod hash;
pub mod history_manifest;
pub mod inferred_copy_from;
pub mod p4_changelist_id;
pub mod p4_file_revs;
pub mod path;
pub mod prefix_tree;
pub mod rawbundle2;
pub mod redaction_key_list;
pub mod repo;
pub mod restricted_manifest_id;
pub mod sha1_hash;
pub mod sharded_map;
pub mod sharded_map_v2;
pub mod skeleton_manifest;
pub mod skeleton_manifest_v2;
pub mod sorted_vector_trie_map;
pub mod sql_types;
pub mod subtree_change;
pub mod svnrev;
pub mod test_manifest;
pub mod test_sharded_manifest;
pub mod trie_map;
pub mod typed_hash;
pub mod unode;

pub use blame_v2::BlameRejected;
pub use blame_v2::BlameV2Id;
pub use blob::Blob;
pub use blob::BlobstoreValue;
pub use blob::ChangesetBlob;
pub use blob::ContentBlob;
pub use blob::RawBundle2Blob;
pub use blobstore::BlobstoreBytes;
pub use bonsai_changeset::BonsaiChangeset;
pub use bonsai_changeset::BonsaiChangesetMut;
pub use content_chunk::ContentChunk;
pub use content_metadata_v2::ContentAlias;
pub use content_metadata_v2::ContentMetadataV2;
pub use content_metadata_v2::ends_in_newline;
pub use content_metadata_v2::first_line;
pub use content_metadata_v2::is_ascii;
pub use content_metadata_v2::is_binary;
pub use content_metadata_v2::is_generated;
pub use content_metadata_v2::is_partially_generated;
pub use content_metadata_v2::is_utf8;
pub use content_metadata_v2::newline_count;
pub use datetime::DateTime;
pub use datetime::Timestamp;
pub use derivable_type::DerivableType;
pub use derivable_type::DerivableUntopologicallyVariant;
pub use derivable_type::PipelineDerivableVariant;
pub use file_change::BasicFileChange;
pub use file_change::FileChange;
pub use file_change::FileType;
pub use file_change::GitLfs;
pub use file_change::TrackedFileChange;
pub use file_contents::ChunkedFileContents;
pub use file_contents::ContentChunkPointer;
pub use file_contents::FileContents;
pub use generation::FIRST_GENERATION;
pub use generation::Generation;
pub use globalrev::Globalrev;
pub use hash::MononokeDigest;
pub use p4_changelist_id::P4ChangelistId;
pub use p4_file_revs::P4_FILE_REVS_EXTRA;
pub use p4_file_revs::P4FileRevs;
pub use path::CaseConflictTrie;
pub use path::MPath;
pub use path::MPathHash;
pub use path::NonRootMPath;
pub use path::PrefixTrie;
pub use path::RepoPath;
pub use path::check_case_conflicts;
pub use path::find_path_conflicts;
pub use path::mpath_element::MPathElement;
pub use path::mpath_element::MPathElementPrefix;
pub use path::mpath_element_iter;
pub use path::non_root_mpath_element_iter;
pub use path::path_bytes_from_mpath;
pub use rawbundle2::RawBundle2;
pub use redaction_key_list::RedactionKeyList;
pub use repo::REPO_PREFIX_REGEX;
pub use repo::RepositoryId;
pub use restricted_manifest_id::RestrictedManifestId;
pub use sorted_vector_trie_map::SortedVectorTrieMap;
pub use subtree_change::SubtreeChange;
pub use svnrev::Svnrev;
pub use thrift_convert::ThriftConvert;
pub use trie_map::TrieMap;
pub use typed_hash::BlobstoreKey;
pub use typed_hash::BssmV3DirectoryId;
pub use typed_hash::CaseConflictSkeletonManifestId;
pub use typed_hash::ChangesetId;
pub use typed_hash::ChangesetIdPrefix;
pub use typed_hash::ChangesetIdsResolvedFromPrefix;
pub use typed_hash::ContentChunkId;
pub use typed_hash::ContentId;
pub use typed_hash::ContentManifestId;
pub use typed_hash::ContentMetadataV2Id;
pub use typed_hash::DeletedManifestV2Id;
pub use typed_hash::FastlogBatchId;
pub use typed_hash::FileUnodeId;
pub use typed_hash::FsnodeId;
pub use typed_hash::HistoryManifestDeletedNodeId;
pub use typed_hash::HistoryManifestDirectoryId;
pub use typed_hash::HistoryManifestFileId;
pub use typed_hash::InferredCopyFromId;
pub use typed_hash::ManifestUnodeId;
pub use typed_hash::MononokeId;
pub use typed_hash::RawBundle2Id;
pub use typed_hash::SkeletonManifestId;
pub use typed_hash::SkeletonManifestV2Id;
pub use typed_hash::TestManifestId;
pub use typed_hash::TestShardedManifestId;

mod macros;

pub mod thrift {
    pub use derived_data_type_if::DerivedDataType;
    pub use mononoke_types_serialization::acl_manifest;
    pub use mononoke_types_serialization::blame;
    pub use mononoke_types_serialization::bonsai;
    pub use mononoke_types_serialization::bssm;
    pub use mononoke_types_serialization::ccsm;
    pub use mononoke_types_serialization::changeset_info;
    pub use mononoke_types_serialization::content;
    pub use mononoke_types_serialization::content_manifest;
    pub use mononoke_types_serialization::data;
    pub use mononoke_types_serialization::deleted_manifest;
    pub use mononoke_types_serialization::directory_branch_cluster_manifest;
    pub use mononoke_types_serialization::fastlog;
    pub use mononoke_types_serialization::fsnodes;
    pub use mononoke_types_serialization::history_manifest;
    pub use mononoke_types_serialization::id;
    pub use mononoke_types_serialization::inferred_copy_from;
    pub use mononoke_types_serialization::path;
    pub use mononoke_types_serialization::raw_bundle2;
    pub use mononoke_types_serialization::redaction;
    pub use mononoke_types_serialization::sharded_map;
    pub use mononoke_types_serialization::skeleton_manifest;
    pub use mononoke_types_serialization::test_manifest;
    pub use mononoke_types_serialization::time;
    pub use mononoke_types_serialization::unodes;
}

pub mod private {
    pub use anyhow;
    pub use ascii::AsciiStr;
    pub use ascii::AsciiString;
    pub use bytes::Bytes;
    pub use quickcheck::Arbitrary;
    pub use quickcheck::Gen;
    pub use quickcheck::empty_shrinker;
    pub use serde::Serialize;
    pub use serde::Serializer;
    pub use serde::de::Deserialize;
    pub use serde::de::Deserializer;
    pub use serde::de::Error as DeError;

    pub use crate::errors::MononokeTypeError;
    pub use crate::hash::Blake2;
    pub use crate::thrift;
    pub use crate::typed_hash::Blake2HexVisitor;
}
