/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::iter;
use std::num::NonZeroU64;

use anyhow::Result;
use dag_types::Location;
use minibytes::Bytes;
#[cfg(any(test, feature = "for-tests"))]
use quickcheck::Arbitrary;
#[cfg(any(test, feature = "for-tests"))]
use quickcheck::Gen;
#[cfg(any(test, feature = "for-tests"))]
use quickcheck_arbitrary_derive::Arbitrary;
use serde_derive::Deserialize;
use serde_derive::Serialize;
use type_macros::auto_wire;
use types::Parents;
use types::RepoPathBuf;
use types::hgid::HgId;

use crate::BonsaiChangesetId;
use crate::CommitId;
use crate::CommitIdScheme;
use crate::FileType;
use crate::ServerError;
use crate::UploadToken;

/// Outcome of extending a bubble's TTL
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub enum ExtendBubbleTtlOutcome {
    /// The bubble's TTL was extended to the new timestamp
    Extended(i64),
    /// The bubble's TTL was not changed, current expiration timestamp
    NotChanged(i64),
}

/// Given a graph location, return `count` hashes following first parent links.
///
/// Example:
/// 0 - a - b - c
/// In this example our initial commit is `0`, then we have `a` the first commit, `b` second,
/// `c` third.
/// {
///   location: {
///     descendant: c,
///     distance: 1,
///   }
///   count: 2,
/// }
/// => [b, a]
#[auto_wire]
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitLocationToHashRequest {
    #[id(1)]
    pub location: Location<HgId>,
    #[id(2)]
    pub count: u64,
}

#[auto_wire]
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitLocationToHashResponse {
    #[id(1)]
    pub location: Location<HgId>,
    #[id(2)]
    pub count: u64,
    #[id(3)]
    pub hgids: Vec<HgId>,
}

#[auto_wire]
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitLocationToHashRequestBatch {
    #[id(1)]
    pub requests: Vec<CommitLocationToHashRequest>,
}

#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitHashToLocationRequestBatch {
    pub master_heads: Vec<HgId>,
    pub hgids: Vec<HgId>,
    pub unfiltered: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[derive(Serialize)] // used to convert to Python
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitHashToLocationResponse {
    pub hgid: HgId,
    pub result: Result<Option<Location<HgId>>, ServerError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[derive(Serialize)]
pub struct CommitKnownResponse {
    pub hgid: HgId,
    /// `Ok(true)`: The server verified that `hgid` is known.
    /// `Ok(false)`: The server does not known `hgid`.
    /// `Err`: The server cannot check `hgid`.
    pub known: Result<bool, ServerError>,
}

#[auto_wire]
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitGraphEntry {
    #[id(1)]
    pub hgid: HgId,
    #[id(2)]
    pub parents: Vec<HgId>,
    #[id(3)]
    pub is_draft: Option<bool>, // server may be able to return phases
}

#[auto_wire]
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitGraphRequest {
    #[id(1)]
    pub common: Vec<HgId>,
    #[id(2)]
    pub heads: Vec<HgId>,
}

#[auto_wire]
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitGraphSegmentsEntry {
    #[id(1)]
    pub head: HgId,
    #[id(2)]
    pub base: HgId,
    #[id(3)]
    pub length: u64,
    #[id(4)]
    pub parents: Vec<CommitGraphSegmentParent>,
}

#[auto_wire]
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitGraphSegmentParent {
    #[id(1)]
    pub hgid: HgId,
    #[id(2)]
    pub location: Option<Location<HgId>>,
}

#[auto_wire]
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitGraphSegmentsRequest {
    #[id(1)]
    pub common: Vec<HgId>,
    #[id(2)]
    pub heads: Vec<HgId>,
}

/// The list of Mercurial commit identifiers for which we want the commit data to be returned.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitRevlogDataRequest {
    pub hgids: Vec<HgId>,
}

/// A mercurial commit entry as it was serialized in the revlog.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[derive(Serialize, Deserialize)]
pub struct CommitRevlogData {
    #[serde(with = "types::serde_with::hgid::bytes")]
    pub hgid: HgId,
    pub revlog_data: Bytes,
}

impl CommitRevlogData {
    pub fn new(hgid: HgId, revlog_data: Bytes) -> Self {
        Self { hgid, revlog_data }
    }
}

/// Request commit hashes that fall in the [low..high] range.
// Note: limit is implied to be 10.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub enum CommitHashLookupRequest {
    InclusiveRange(HgId, HgId),
}

/// make a hash lookup request from a hash prefix for the range of hashes that
/// could match the hash prefix
/// ex: the range 3f54ab22d87423966e0000000000000000000000:3f54ab22d87423966effffffffffffffffffffff
///     matches the prefix 3f54ab22d87423966e
pub fn make_hash_lookup_request(prefix: String) -> Result<CommitHashLookupRequest, anyhow::Error> {
    let suffix_len = HgId::hex_len() - prefix.len();
    let low_hex = prefix
        .chars()
        .chain(iter::repeat_n('0', suffix_len))
        .collect::<String>();
    let low_id = HgId::from_hex(low_hex.as_bytes())?;
    let high_hex = prefix
        .chars()
        .chain(iter::repeat_n('f', suffix_len))
        .collect::<String>();
    let high_id = HgId::from_hex(high_hex.as_bytes())?;
    Ok(CommitHashLookupRequest::InclusiveRange(low_id, high_id))
}

/// Commit hashes that are known to the server in the described range.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitHashLookupResponse {
    pub request: CommitHashLookupRequest,
    pub hgids: Vec<HgId>,
}

#[auto_wire]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct Extra {
    #[id(1)]
    pub key: Vec<u8>,
    #[id(2)]
    pub value: Vec<u8>,
}

#[auto_wire]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct HgChangesetContent {
    #[id(1)]
    pub parents: Parents,
    #[id(2)]
    pub manifestid: HgId,
    #[id(3)]
    pub user: Vec<u8>,
    #[id(4)]
    pub time: i64,
    #[id(5)]
    pub tz: i32,
    #[id(6)]
    pub extras: Vec<Extra>,
    #[id(7)]
    pub files: Vec<RepoPathBuf>,
    #[id(8)]
    pub message: Vec<u8>,
}

#[auto_wire]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct UploadHgChangeset {
    #[id(1)]
    pub node_id: HgId,
    #[id(2)]
    pub changeset_content: HgChangesetContent,
}

#[auto_wire]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct HgMutationEntryContent {
    #[id(1)]
    pub successor: HgId,
    #[id(2)]
    pub predecessors: Vec<HgId>,
    #[id(3)]
    pub split: Vec<HgId>,
    #[id(4)]
    pub op: String,
    #[id(5)]
    pub user: Vec<u8>,
    #[id(6)]
    pub time: i64,
    #[id(7)]
    pub tz: i32,
    #[id(8)]
    pub extras: Vec<Extra>,
}

#[auto_wire]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct UploadHgChangesetsRequest {
    /// list of changesets to upload, changesets must be sorted topologically (use dag.sort)
    #[id(1)]
    pub changesets: Vec<UploadHgChangeset>,
    /// list of mutation entries for the uploading changesets
    #[id(2)]
    pub mutations: Vec<HgMutationEntryContent>,
}

#[auto_wire]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct BonsaiExtra {
    #[id(1)]
    pub key: String,
    #[id(2)]
    pub value: Vec<u8>,
}

#[auto_wire]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct GitExtraHeader {
    #[id(1)]
    pub key: Vec<u8>,
    #[id(2)]
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum BonsaiFileChange {
    Change {
        /// Token proving the file was uploaded, and containing its content id and size
        upload_token: UploadToken,
        file_type: FileType,
        /// Copy information: (source_path, parent_index) where parent_index indicates
        /// which parent changeset the file was copied from (0 = first parent, 1 = second parent, etc.)
        copy_info: Option<(RepoPathBuf, usize)>,
    },
    Deletion,
    UntrackedChange {
        upload_token: UploadToken,
        file_type: FileType,
    },
    UntrackedDeletion,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct BonsaiChangesetContent {
    pub hg_parents: Parents,
    pub author: String,
    pub time: i64,
    pub tz: i32,
    pub extra: Vec<BonsaiExtra>,
    pub file_changes: Vec<(RepoPathBuf, BonsaiFileChange)>,
    pub message: String,
    pub is_snapshot: bool,
}

#[auto_wire]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct UploadBonsaiChangesetRequest {
    /// changeset to upload
    #[id(1)]
    pub changeset: BonsaiChangesetContent,
}

/// Strategy for bubble management during snapshot creation.
///
/// This enum defines three distinct strategies for how snapshots interact with ephemeral bubbles:
/// 1. Creating a completely new bubble with no relationship to previous ones
/// 2. Creating a new bubble but hinting to the backend to copy data from a previous bubble for performance
/// 3. Reusing an existing bubble by extending its TTL and reusing its storage
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BubbleStrategy {
    /// Create a new bubble with no relationship to previous bubbles.
    /// All file data will be uploaded fresh.
    #[default]
    CreateNew,
    /// Create a new bubble, but hint to the backend to copy data from a previous bubble.
    /// This allows the server to avoid re-uploading identical files by copying from
    /// the previous bubble's storage transparently (via `copy_file_to_bubble` on the server),
    /// improving performance while still creating a new bubble.
    /// see: fbcode/eden/mononoke/edenapi_service/src/handlers/lookup.rs
    CreateWithCopyHint { bubble_id: NonZeroU64 },
    /// Reuse an existing bubble: extends its TTL and reuses its storage.
    /// This is for continuation scenarios where snapshots build on each other.
    /// When keep_ttl is true, skip the TTL extension for performance.
    Reuse {
        bubble_id: NonZeroU64,
        #[serde(default)]
        keep_ttl: bool,
        snapshot_id: BonsaiChangesetId,
    },
}

/// Properties for ephemeral bubble management during snapshot creation.
///
/// These properties control how the snapshot is associated with an ephemeral
/// bubble and how the bubble's lifetime and storage are managed.
#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
pub struct BubbleUploadProperties {
    /// Custom TTL duration in seconds for the bubble.
    /// If None, server default is used.
    pub lifetime_secs: Option<u64>,
    /// Strategy for bubble management: create new, copy from previous, or reuse existing.
    #[serde(default)]
    pub strategy: BubbleStrategy,
    /// Labels to associate with the bubble for categorization, querying and preservation.
    pub labels: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct SnapshotRawFiles {
    /// Absolute root of the repository, where all files are
    /// relative to. Can be different from cwd.
    pub root: RepoPathBuf,
    /// Tracked files modified in local changes
    pub modified: Vec<(RepoPathBuf, FileType)>,
    /// Files added with "hg add"
    pub added: Vec<(RepoPathBuf, FileType)>,
    /// Files that are not tracked but are in the local changes
    pub untracked: Vec<(RepoPathBuf, FileType)>,
    /// Files removed with "hg rm"
    pub removed: Vec<RepoPathBuf>,
    /// Files that are not in the local changes but were tracked
    pub missing: Vec<RepoPathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct SnapshotRawData {
    pub hg_parents: Parents,
    pub files: SnapshotRawFiles,
    pub time: i64,
    pub tz: i32,
    pub author: String,
}

#[auto_wire]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct FetchSnapshotRequest {
    #[id(1)]
    pub cs_id: BonsaiChangesetId,
}

#[auto_wire]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct FetchSnapshotResponse {
    #[id(1)]
    pub hg_parents: Parents,
    #[id(2)]
    pub file_changes: Vec<(RepoPathBuf, BonsaiFileChange)>,
    #[id(3)]
    pub author: String,
    #[id(4)]
    pub time: i64,
    #[id(5)]
    pub tz: i32,
    #[id(6)]
    pub bubble_id: Option<NonZeroU64>,
    #[id(7)]
    pub labels: Vec<String>,
}

/// A cacheable version of snapshot data compatible with FetchSnapshotResponse
/// This can be constructed from local bonsai data and cached using CBOR serialization
/// This is a separate type with an ability to diverge from the FetchSnapshotResponse
/// All changes to this type should be backward compatible as it is used for caching
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CacheableSnapshot {
    /// Parent commit hashes from Mercurial
    pub hg_parents: Parents,
    /// List of file changes included in this snapshot
    pub file_changes: Vec<(RepoPathBuf, BonsaiFileChange)>,
    /// Author who created the snapshot
    pub author: String,
    /// Unix timestamp when the snapshot was created
    pub time: i64,
    /// Timezone offset in seconds from UTC
    pub tz: i32,
    /// Optional identifier for ephemeral commits/bubbles
    pub bubble_id: Option<NonZeroU64>,
    /// Labels associated with this snapshot
    pub labels: Vec<String>,
    /// Whether this snapshot data was retrieved from local cache bypassing the server
    pub cached: Option<bool>,
}

impl From<CacheableSnapshot> for FetchSnapshotResponse {
    fn from(cacheable: CacheableSnapshot) -> Self {
        Self {
            hg_parents: cacheable.hg_parents,
            file_changes: cacheable.file_changes,
            author: cacheable.author,
            time: cacheable.time,
            tz: cacheable.tz,
            bubble_id: cacheable.bubble_id,
            labels: cacheable.labels,
        }
    }
}

impl From<FetchSnapshotResponse> for CacheableSnapshot {
    fn from(response: FetchSnapshotResponse) -> Self {
        Self {
            hg_parents: response.hg_parents,
            file_changes: response.file_changes,
            author: response.author,
            time: response.time,
            tz: response.tz,
            bubble_id: response.bubble_id,
            labels: response.labels,
            cached: Some(false), // Mark as not cached when converting from FetchSnapshotResponse
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
#[auto_wire]
pub struct AlterSnapshotRequest {
    #[id(1)]
    pub cs_id: BonsaiChangesetId,
    #[id(2)]
    pub labels_to_add: Vec<String>,
    #[id(3)]
    pub labels_to_remove: Vec<String>,
}

#[auto_wire]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct AlterSnapshotResponse {
    #[id(1)]
    pub current_labels: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct UploadSnapshotResponse {
    pub changeset_token: UploadToken,
    pub bubble_id: NonZeroU64,
    // bubble expiration timestamp if known
    pub bubble_expiration_timestamp: Option<i64>,
    // total bytes of snapshot content
    pub snapshot_content_bytes: u64,
}

#[auto_wire]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct EphemeralPrepareRequest {
    #[id(1)]
    pub custom_duration_secs: Option<u64>,
    #[id(2)]
    pub labels: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct EphemeralPrepareResponse {
    pub bubble_id: NonZeroU64,
    pub expiration_timestamp: Option<i64>,
}

#[auto_wire]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct EphemeralExtendRequest {
    #[id(2)]
    pub custom_duration_secs: Option<u64>,
    #[id(3)]
    #[wire_option]
    pub snapshot_id: BonsaiChangesetId,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct EphemeralExtendResponse {
    pub bubble_id: NonZeroU64,
    pub result: ExtendBubbleTtlOutcome,
}

#[cfg(any(test, feature = "for-tests"))]
impl Arbitrary for BonsaiFileChange {
    fn arbitrary(g: &mut Gen) -> Self {
        match u64::arbitrary(g) % 100 {
            0..=49 => Self::Change {
                upload_token: Arbitrary::arbitrary(g),
                file_type: Arbitrary::arbitrary(g),
                copy_info: Arbitrary::arbitrary(g),
            },
            50..=79 => Self::Deletion,
            80..=94 => Self::UntrackedChange {
                upload_token: Arbitrary::arbitrary(g),
                file_type: Arbitrary::arbitrary(g),
            },
            95..=99 => Self::UntrackedDeletion,
            _ => unreachable!(),
        }
    }
}

#[auto_wire]
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitMutationsRequest {
    #[id(1)]
    pub commits: Vec<HgId>,
}

#[auto_wire]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitMutationsResponse {
    #[id(1)]
    pub mutation: HgMutationEntryContent,
}

#[auto_wire]
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitTranslateIdRequest {
    #[id(1)]
    pub commits: Vec<CommitId>,
    #[id(2)]
    pub scheme: CommitIdScheme,
    #[id(3)]
    pub from_repo: Option<String>,
    #[id(4)]
    pub to_repo: Option<String>,
    #[id(5)]
    pub lookup_behavior: Option<String>,
}

#[auto_wire]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct CommitTranslateIdResponse {
    #[id(1)]
    pub commit: CommitId,
    #[id(2)]
    pub translated: CommitId,
}

#[auto_wire]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct UploadIdenticalChangesetsRequest {
    /// list of changesets to upload, changesets must be sorted topologically
    #[id(1)]
    pub changesets: Vec<IdenticalChangesetContent>,
}

#[auto_wire]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct HgInfo {
    #[id(1)]
    pub node_id: HgId,
    #[id(2)]
    pub manifestid: HgId,
    #[id(3)]
    pub extras: Vec<Extra>,
}

#[auto_wire]
#[derive(
    Clone,
    Debug,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    Deserialize,
    Default
)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub enum BonsaiParents {
    #[id(1)]
    #[default]
    None,
    #[id(2)]
    One(BonsaiChangesetId),
    #[id(3)]
    Two((BonsaiChangesetId, BonsaiChangesetId)),
}

impl BonsaiParents {
    pub fn new(p1: Option<BonsaiChangesetId>, p2: Option<BonsaiChangesetId>) -> Self {
        match (p1, p2) {
            (None, None) => Self::None,
            (Some(p1), None) => Self::One(p1),
            (None, Some(p2)) => Self::One(p2),
            (Some(p1), Some(p2)) => Self::Two((p1, p2)),
        }
    }

    pub fn to_vec(&self) -> Vec<BonsaiChangesetId> {
        match self {
            Self::None => vec![],
            Self::One(p1) => vec![p1.clone()],
            Self::Two((p1, p2)) => vec![p1.clone(), p2.clone()],
        }
    }
}

impl FromIterator<BonsaiChangesetId> for BonsaiParents {
    fn from_iter<I: IntoIterator<Item = BonsaiChangesetId>>(iter: I) -> Self {
        let mut iter = iter.into_iter();
        let p1 = iter.next();
        let p2 = iter.next();
        Self::new(p1, p2)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub enum SubtreeChange {
    DeepCopy {
        from_path: RepoPathBuf,
        from_cs_id: BonsaiChangesetId,
    },
    Merge {
        from_path: RepoPathBuf,
        from_cs_id: BonsaiChangesetId,
    },
    Import {
        from_path: RepoPathBuf,
        from_commit: String,
        from_repo_url: String,
    },
    CrossRepoMerge {
        from_path: RepoPathBuf,
        from_commit: String,
        from_repo_url: String,
    },
}

#[auto_wire]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "for-tests"), derive(Arbitrary))]
pub struct IdenticalChangesetContent {
    #[id(1)]
    pub bcs_id: BonsaiChangesetId,
    #[id(2)]
    pub hg_parents: Parents,
    #[id(3)]
    pub bonsai_parents: BonsaiParents,
    #[id(4)]
    pub author: String,
    #[id(5)]
    pub time: i64,
    #[id(6)]
    pub tz: i32,
    #[id(7)]
    pub extras: Vec<BonsaiExtra>,
    #[id(8)]
    pub bonsai_file_changes: Vec<(RepoPathBuf, BonsaiFileChange)>,
    #[id(9)]
    pub hg_file_changes: Vec<RepoPathBuf>,
    #[id(10)]
    pub message: String,
    #[id(11)]
    pub is_snapshot: bool,
    #[id(12)]
    pub hg_info: HgInfo,
    #[id(13)]
    pub committer: Option<String>,
    #[id(14)]
    pub committer_time: Option<i64>,
    #[id(15)]
    pub committer_tz: Option<i32>,
    #[id(16)]
    pub git_extra_headers: Option<Vec<GitExtraHeader>>,
    #[id(17)]
    pub subtree_changes: Option<Vec<(RepoPathBuf, SubtreeChange)>>,
}

impl From<IdenticalChangesetContent> for HgChangesetContent {
    fn from(changeset: IdenticalChangesetContent) -> Self {
        Self {
            parents: changeset.hg_parents,
            manifestid: changeset.hg_info.manifestid,
            user: changeset.author.into_bytes(),
            time: changeset.time,
            tz: changeset.tz,
            // Use raw HG extras directly to preserve exact bytes for hash computation
            extras: changeset.hg_info.extras,
            files: changeset.hg_file_changes,
            message: changeset.message.into_bytes(),
        }
    }
}

impl From<IdenticalChangesetContent> for BonsaiChangesetContent {
    fn from(changeset: IdenticalChangesetContent) -> Self {
        Self {
            hg_parents: changeset.hg_parents,
            author: changeset.author,
            time: changeset.time,
            tz: changeset.tz,
            extra: changeset.extras,
            file_changes: changeset.bonsai_file_changes,
            message: changeset.message,
            is_snapshot: changeset.is_snapshot,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_range(start: &[u8], end: &[u8]) -> Result<CommitHashLookupRequest> {
        let start_id = HgId::from_hex(start)?;
        let end_id = HgId::from_hex(end)?;
        Ok(CommitHashLookupRequest::InclusiveRange(start_id, end_id))
    }

    #[test]
    fn test_make_hash_lookup_request() -> Result<()> {
        let short_hash = String::from("3f54ab22d87423966e");
        let prefix_req = make_hash_lookup_request(short_hash)?;
        let expected_req = make_range(
            b"3f54ab22d87423966e0000000000000000000000",
            b"3f54ab22d87423966effffffffffffffffffffff",
        )?;
        assert_eq!(prefix_req, expected_req);

        let empty_hash = String::new();
        let empty_req = make_hash_lookup_request(empty_hash)?;
        let expected_empty_req = make_range(
            b"0000000000000000000000000000000000000000",
            b"ffffffffffffffffffffffffffffffffffffffff",
        )?;
        assert_eq!(empty_req, expected_empty_req);

        let full_hash = String::from("3f54ab22d87423966e27374e6c0f8112a269999b");
        let full_req = make_hash_lookup_request(full_hash)?;
        let expected_full_req = make_range(
            b"3f54ab22d87423966e27374e6c0f8112a269999b",
            b"3f54ab22d87423966e27374e6c0f8112a269999b",
        )?;
        assert_eq!(full_req, expected_full_req);

        Ok(())
    }
}
