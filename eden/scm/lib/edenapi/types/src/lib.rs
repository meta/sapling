/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#![allow(unexpected_cfgs)]

//! Types shared between the SaplingRemoteAPI client and server.
//!
//! This crate exists primarily to provide a lightweight place to
//! put types that need to be used by both the client and server.
//! Types that are exclusive used by either the client or server
//! SHOULD NOT be added to this crate.
//!
//! Given that the client and server are each part of different
//! projects (Mercurial and Mononoke, respectively) which have
//! different build processes, putting shared types in their own
//! crate decreases the likelihood of build failures caused by
//! dependencies with complex or esoteric build requirements.
//!
//! Most of the types in this crate are used for data interchange
//! between the client and server. As such, CHANGES TO THE THESE
//! TYPES MAY CAUSE VERSION SKEW, so any changes should proceed
//! with caution.

#[macro_use]
pub mod hash;

pub mod anyid;
pub mod batch;
pub mod blame;
pub mod bookmark;
pub mod cloud;
pub mod commit;
pub mod commitid;
pub mod errors;
pub mod file;
pub mod git_objects;
pub mod history;
pub mod land;
pub mod legacy;
pub mod metadata;
pub mod path_history;
pub mod segments;
pub mod suffix_query;
pub mod token;
pub mod tree;
pub mod wire;

// re-export CloneData
pub use dag_types::CloneData;
pub use dag_types::FlatSegment;
pub use dag_types::Location as CommitLocation;
pub use dag_types::PreparedFlatSegments;
use minibytes::Bytes;
#[cfg(any(test, feature = "for-tests"))]
use quickcheck_arbitrary_derive::Arbitrary;
use serde::Serialize;
use thiserror::Error;
pub use types::hgid::HgId;
pub use types::key::Key;
pub use types::nodeinfo::NodeInfo;
pub use types::parents::Parents;
pub use types::path::RepoPathBuf;

pub use crate::anyid::AnyId;
pub use crate::anyid::LookupRequest;
pub use crate::anyid::LookupResponse;
pub use crate::anyid::LookupResult;
pub use crate::batch::Batch;
pub use crate::blame::BlameData;
pub use crate::blame::BlameLineRange;
pub use crate::blame::BlameRequest;
pub use crate::blame::BlameResult;
pub use crate::bookmark::BookmarkEntry;
pub use crate::bookmark::BookmarkKind;
pub use crate::bookmark::BookmarkRequest;
pub use crate::bookmark::BookmarkResult;
pub use crate::bookmark::ListBookmarkPatternsRequest;
pub use crate::bookmark::ListBookmarkPatternsResponse;
pub use crate::bookmark::MirrorBookmarkMove;
pub use crate::bookmark::MirrorBookmarkUpdateReason;
pub use crate::bookmark::ReplayIdenticalMovesRequest;
pub use crate::bookmark::ReplayIdenticalMovesResponse;
pub use crate::bookmark::SetBookmarkRequest;
pub use crate::bookmark::SetBookmarkResponse;
pub use crate::cloud::CloudShareWorkspaceRequest;
pub use crate::cloud::CloudShareWorkspaceResponse;
pub use crate::cloud::CloudWorkspaceRequest;
pub use crate::cloud::CloudWorkspacesRequest;
pub use crate::cloud::GetReferencesParams;
pub use crate::cloud::GetSmartlogByVersionParams;
pub use crate::cloud::GetSmartlogFlag;
pub use crate::cloud::GetSmartlogParams;
pub use crate::cloud::HistoricalVersion;
pub use crate::cloud::HistoricalVersionsData;
pub use crate::cloud::HistoricalVersionsParams;
pub use crate::cloud::HistoricalVersionsResponse;
pub use crate::cloud::OtherRepoWorkspacesRequest;
pub use crate::cloud::ReferencesData;
pub use crate::cloud::ReferencesDataResponse;
pub use crate::cloud::RenameWorkspaceRequest;
pub use crate::cloud::RenameWorkspaceResponse;
pub use crate::cloud::RollbackWorkspaceRequest;
pub use crate::cloud::RollbackWorkspaceResponse;
pub use crate::cloud::SmartlogData;
pub use crate::cloud::SmartlogDataResponse;
pub use crate::cloud::SmartlogNode;
pub use crate::cloud::UpdateArchiveParams;
pub use crate::cloud::UpdateArchiveResponse;
pub use crate::cloud::UpdateReferencesParams;
pub use crate::cloud::WorkspaceData;
pub use crate::cloud::WorkspaceDataResponse;
pub use crate::cloud::WorkspaceSharingData;
pub use crate::cloud::WorkspacesDataResponse;
pub use crate::commit::AlterSnapshotRequest;
pub use crate::commit::AlterSnapshotResponse;
pub use crate::commit::BonsaiChangesetContent;
pub use crate::commit::BonsaiExtra;
pub use crate::commit::BonsaiFileChange;
pub use crate::commit::BonsaiParents;
pub use crate::commit::BubbleStrategy;
pub use crate::commit::BubbleUploadProperties;
pub use crate::commit::CacheableSnapshot;
pub use crate::commit::CommitGraphEntry;
pub use crate::commit::CommitGraphRequest;
pub use crate::commit::CommitGraphSegmentParent;
pub use crate::commit::CommitGraphSegmentsEntry;
pub use crate::commit::CommitGraphSegmentsRequest;
pub use crate::commit::CommitHashLookupRequest;
pub use crate::commit::CommitHashLookupResponse;
pub use crate::commit::CommitHashToLocationRequestBatch;
pub use crate::commit::CommitHashToLocationResponse;
pub use crate::commit::CommitKnownResponse;
pub use crate::commit::CommitLocationToHashRequest;
pub use crate::commit::CommitLocationToHashRequestBatch;
pub use crate::commit::CommitLocationToHashResponse;
pub use crate::commit::CommitMutationsRequest;
pub use crate::commit::CommitMutationsResponse;
pub use crate::commit::CommitRevlogData;
pub use crate::commit::CommitRevlogDataRequest;
pub use crate::commit::CommitTranslateIdRequest;
pub use crate::commit::CommitTranslateIdResponse;
pub use crate::commit::EphemeralExtendRequest;
pub use crate::commit::EphemeralExtendResponse;
pub use crate::commit::EphemeralPrepareRequest;
pub use crate::commit::EphemeralPrepareResponse;
pub use crate::commit::ExtendBubbleTtlOutcome;
pub use crate::commit::Extra;
pub use crate::commit::FetchSnapshotRequest;
pub use crate::commit::FetchSnapshotResponse;
pub use crate::commit::HgChangesetContent;
pub use crate::commit::HgMutationEntryContent;
pub use crate::commit::IdenticalChangesetContent;
pub use crate::commit::SnapshotFileContent;
pub use crate::commit::SnapshotRawData;
pub use crate::commit::SnapshotRawFiles;
pub use crate::commit::UploadBonsaiChangesetRequest;
pub use crate::commit::UploadHgChangeset;
pub use crate::commit::UploadHgChangesetsRequest;
pub use crate::commit::UploadHgSnapshotRequest;
pub use crate::commit::UploadHgSnapshotResponse;
pub use crate::commit::UploadIdenticalChangesetsRequest;
pub use crate::commit::UploadSnapshotFilenode;
pub use crate::commit::UploadSnapshotResponse;
pub use crate::commit::make_hash_lookup_request;
pub use crate::commitid::BonsaiChangesetId;
pub use crate::commitid::CommitId;
pub use crate::commitid::CommitIdScheme;
pub use crate::commitid::GitSha1;
pub use crate::errors::CODE_BOOKMARK_MOVE_ALREADY_PROCESSED;
pub use crate::errors::CODE_PERMISSION_DENIED;
pub use crate::errors::ServerError;
pub use crate::file::FileAttributes;
pub use crate::file::FileAuxData;
pub use crate::file::FileContent;
pub use crate::file::FileEntry;
pub use crate::file::FileError;
pub use crate::file::FileRequest;
pub use crate::file::FileResponse;
pub use crate::file::FileSpec;
pub use crate::file::HgFilenodeData;
pub use crate::file::UploadHgFilenodeRequest;
pub use crate::file::UploadTokensResponse;
pub use crate::git_objects::GitObjectBytes;
pub use crate::git_objects::GitObjectsRequest;
pub use crate::git_objects::GitObjectsResponse;
pub use crate::history::HistoryEntry;
pub use crate::history::HistoryRequest;
pub use crate::history::HistoryResponse;
pub use crate::history::HistoryResponseChunk;
pub use crate::history::WireHistoryEntry;
pub use crate::land::LandStackData;
pub use crate::land::LandStackRequest;
pub use crate::land::LandStackResponse;
pub use crate::land::PushVar;
pub use crate::legacy::StreamingChangelogRequest;
pub use crate::legacy::StreamingChangelogResponse;
pub use crate::metadata::AnyFileContentId;
pub use crate::metadata::Blake3;
pub use crate::metadata::ContentId;
pub use crate::metadata::DirectoryMetadata;
pub use crate::metadata::FileMetadata;
pub use crate::metadata::FileType;
pub use crate::metadata::FsnodeId;
pub use crate::metadata::Sha1;
pub use crate::metadata::Sha256;
pub use crate::path_history::PathHistoryEntries;
pub use crate::path_history::PathHistoryEntry;
pub use crate::path_history::PathHistoryRequest;
pub use crate::path_history::PathHistoryRequestPaginationCursor;
pub use crate::path_history::PathHistoryResponse;
pub use crate::segments::CommitGraphSegments;
pub use crate::suffix_query::SuffixQueryRequest;
pub use crate::suffix_query::SuffixQueryResponse;
pub use crate::token::FileContentTokenMetadata;
pub use crate::token::IndexableId;
pub use crate::token::UploadToken;
pub use crate::token::UploadTokenData;
pub use crate::token::UploadTokenMetadata;
pub use crate::token::UploadTokenSignature;
pub use crate::tree::CheckManifestPermissionRequest;
pub use crate::tree::CheckManifestPermissionResponse;
pub use crate::tree::CheckPathPermissionAclEntry;
pub use crate::tree::CheckPathPermissionData;
pub use crate::tree::CheckPathPermissionRequest;
pub use crate::tree::CheckPathPermissionResponse;
pub use crate::tree::TreeAttributes;
pub use crate::tree::TreeAuxData;
pub use crate::tree::TreeChildDirectoryEntry;
pub use crate::tree::TreeChildEntry;
pub use crate::tree::TreeChildFileEntry;
pub use crate::tree::TreeEntry;
pub use crate::tree::TreeError;
pub use crate::tree::TreeRequest;
pub use crate::tree::UploadTreeEntry;
pub use crate::tree::UploadTreeRequest;
pub use crate::tree::UploadTreeResponse;
pub use crate::wire::ToApi;
pub use crate::wire::ToWire;
pub use crate::wire::WireToApiConversionError;

#[derive(Debug, Error)]
#[error("Invalid hash: {expected} (expected) != {computed} (computed)")]
pub struct InvalidHgId {
    expected: HgId,
    computed: HgId,
    data: Bytes,
    parents: Parents,
}

#[derive(Clone, Debug, Error, Eq, PartialEq, Serialize)]
#[error("Error fetching key {key:?}: {err}")]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct SaplingRemoteApiServerError {
    pub err: SaplingRemoteApiServerErrorKind,
    pub key: Option<Key>,
}

impl SaplingRemoteApiServerError {
    pub fn new(err: impl std::fmt::Debug) -> SaplingRemoteApiServerError {
        SaplingRemoteApiServerError {
            err: SaplingRemoteApiServerErrorKind::OpaqueError(format!("{err:?}")),
            key: None,
        }
    }

    pub fn with_key(key: Key, err: impl std::fmt::Debug) -> SaplingRemoteApiServerError {
        SaplingRemoteApiServerError {
            err: SaplingRemoteApiServerErrorKind::OpaqueError(format!("{err:?}")),
            key: Some(key),
        }
    }

    pub fn with_path(path: RepoPathBuf, err: impl std::fmt::Debug) -> SaplingRemoteApiServerError {
        SaplingRemoteApiServerError {
            err: SaplingRemoteApiServerErrorKind::OpaqueError(format!("{err:?}")),
            key: Some(Key {
                path,
                hgid: *HgId::null_id(),
            }),
        }
    }

    pub fn with_hgid(hgid: HgId, err: impl std::fmt::Debug) -> SaplingRemoteApiServerError {
        SaplingRemoteApiServerError {
            err: SaplingRemoteApiServerErrorKind::OpaqueError(format!("{err:?}")),
            key: Some(Key {
                hgid,
                path: RepoPathBuf::new(),
            }),
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq, Serialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub enum SaplingRemoteApiServerErrorKind {
    #[error("SaplingRemoteAPI server returned an error with message: {0}")]
    OpaqueError(String),
    #[error(
        "Unauthorized access to manifest under restricted path: {tree_id}. Request access via ACL {request_acl}.{}",
        .denial_message.as_deref().map_or_else(String::new, |message| format!("\n{message}"))
    )]
    PermissionDenied {
        /// ID of the tree to which the user does not have access.
        tree_id: HgId,
        /// ACL to direct users for access requests.
        request_acl: String,
        /// Repo-configured text to show with the denial, if any.
        denial_message: Option<String>,
    },
}
