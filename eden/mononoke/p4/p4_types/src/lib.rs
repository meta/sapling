/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Perforce's data model, independent of how it is read.

use std::collections::HashMap;
use std::fmt;

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changelist {
    pub number: u64,
    pub user: String,
    /// Seconds since the epoch at which the changelist was submitted.
    pub time: u64,
    pub description: String,
    /// The description as p4 sent it, only when that isn't UTF-8 and `description` has
    /// replacement characters instead.
    pub description_bytes: Option<Vec<u8>>,
    pub files: Vec<FileRevision>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRevision {
    /// As the repo names the file: the bytes p4 sent, decoded.
    pub depot_path: String,
    /// The depot path as p4 sent it, only when `depot_path` was decoded from another charset
    /// than UTF-8.
    pub depot_path_bytes: Option<Vec<u8>>,
    pub action: Action,
    pub file_type: String,
    pub revision: u32,
    /// `None` for `delete` and `move/delete`.
    pub size: Option<u64>,
    /// Of the content `p4 print` sends. Never without `size`, but may be missing with it.
    pub digest: Option<Md5Digest>,
}

impl FileRevision {
    /// The depot path to pass back to p4: never `depot_path` re-encoded, which may not give
    /// back the bytes the server stores.
    pub fn p4_depot_path(&self) -> &[u8] {
        self.depot_path_bytes
            .as_deref()
            .unwrap_or(self.depot_path.as_bytes())
    }
}

/// A file's head revision at some point in history, from `p4 fstat`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileAt {
    pub file: FileRevision,
    pub change: u64,
}

/// Shown as uppercase hex, as p4 reports it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Md5Digest([u8; 16]);

impl Md5Digest {
    pub fn new(digest: [u8; 16]) -> Self {
        Self(digest)
    }

    /// 32 hex digits in either case.
    pub fn from_hex(hex: &[u8]) -> Option<Self> {
        if hex.len() != 32 {
            return None;
        }
        let nibble = |digit: u8| char::from(digit).to_digit(16);
        let mut digest = [0; 16];
        for (byte, [high, low]) in digest.iter_mut().zip(hex.as_chunks::<2>().0) {
            *byte = u8::try_from(nibble(*high)? << 4 | nibble(*low)?).ok()?;
        }
        Some(Self(digest))
    }
}

impl fmt::Display for Md5Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02X}"))
    }
}

impl fmt::Debug for Md5Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Md5Digest({self})")
    }
}

/// The revision's current state, not necessarily the action submitted: Perforce rewrites
/// it to `purge` or `archive` afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Add,
    Edit,
    Delete,
    Branch,
    Integrate,
    MoveAdd,
    MoveDelete,
    Import,
    Purge,
    Archive,
}

impl Action {
    const ALL: [Action; 10] = [
        Action::Add,
        Action::Edit,
        Action::Delete,
        Action::Branch,
        Action::Integrate,
        Action::MoveAdd,
        Action::MoveDelete,
        Action::Import,
        Action::Purge,
        Action::Archive,
    ];

    pub fn from_p4(action: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|known| known.as_p4().as_bytes() == action)
    }

    /// As p4 writes it.
    pub fn as_p4(self) -> &'static str {
        match self {
            Action::Add => "add",
            Action::Edit => "edit",
            Action::Delete => "delete",
            Action::Branch => "branch",
            Action::Integrate => "integrate",
            Action::MoveAdd => "move/add",
            Action::MoveDelete => "move/delete",
            Action::Import => "import",
            Action::Purge => "purge",
            Action::Archive => "archive",
        }
    }

    /// Purged and archived revisions keep their size and digest, but p4 prints nothing.
    pub fn has_content(self) -> bool {
        match self {
            Action::Add
            | Action::Edit
            | Action::Branch
            | Action::Integrate
            | Action::MoveAdd
            | Action::Import => true,
            Action::Delete | Action::MoveDelete | Action::Purge | Action::Archive => false,
        }
    }
}

/// A file's revisions as `p4 filelog` lists them, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLog {
    pub depot_path: String,
    pub revisions: Vec<FileLogRevision>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLogRevision {
    pub revision: u32,
    pub change: u64,
    pub action: Action,
    pub file_type: String,
    pub integrations: Vec<Integration>,
}

/// One integration record as p4 stores it. `file` and the revisions are the other side's:
/// the source of a `... from` record, the target of a `... into` one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Integration {
    pub how: IntegrationHow,
    pub file: String,
    /// The revisions after `start_rev` up to `end_rev`; `None` for `#none`, the start.
    pub start_rev: Option<u32>,
    pub end_rev: u32,
}

/// Each kind comes in both directions: the target's record and the source's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrationHow {
    BranchFrom,
    BranchInto,
    CopyFrom,
    CopyInto,
    MovedFrom,
    MovedInto,
    MergeFrom,
    MergeInto,
    EditFrom,
    EditInto,
    IntegrateFrom,
    IntegrateInto,
    Ignored,
    IgnoredBy,
    DeleteFrom,
    DeleteInto,
    AddFrom,
    AddInto,
    Undid,
    UndoneBy,
    /// One added by a newer server.
    Other(String),
}

impl IntegrationHow {
    pub fn from_p4(how: String) -> Self {
        match how.as_str() {
            "branch from" => Self::BranchFrom,
            "branch into" => Self::BranchInto,
            "copy from" => Self::CopyFrom,
            "copy into" => Self::CopyInto,
            "moved from" => Self::MovedFrom,
            "moved into" => Self::MovedInto,
            "merge from" => Self::MergeFrom,
            "merge into" => Self::MergeInto,
            "edit from" => Self::EditFrom,
            "edit into" => Self::EditInto,
            "integrate from" => Self::IntegrateFrom,
            "integrate into" => Self::IntegrateInto,
            "ignored" => Self::Ignored,
            "ignored by" => Self::IgnoredBy,
            "delete from" => Self::DeleteFrom,
            "delete into" => Self::DeleteInto,
            "add from" => Self::AddFrom,
            "add into" => Self::AddInto,
            "undid" => Self::Undid,
            "undone by" => Self::UndoneBy,
            _ => Self::Other(how),
        }
    }
}

/// A stream spec as `p4 streams` lists it, without its description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    /// Its depot path, e.g. `//streams/main`.
    pub stream: String,
    pub name: String,
    pub stream_type: StreamType,
    /// `None` for a mainline (p4's `none`).
    pub parent: Option<String>,
    /// The nearest non-virtual ancestor; `None` for a mainline.
    pub base_parent: Option<String>,
    pub parent_view: ParentView,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamType {
    Mainline,
    Development,
    Release,
    SparseDev,
    SparseRel,
    Task,
    Virtual,
    /// One added by a newer server.
    Other(String),
}

/// Whether the stream's view adds to its parent's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParentView {
    Inherit,
    NoInherit,
    /// One added by a newer server.
    Other(String),
}

/// A user as `p4 users -a` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub user: String,
    pub email: String,
    pub full_name: String,
    pub user_type: UserType,
    /// Seconds since the epoch at which the user's spec was last changed.
    pub update: u64,
    /// Seconds since the epoch at which the user last ran a command.
    pub access: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserType {
    Standard,
    Service,
    Operator,
    /// One added by a newer server.
    Other(String),
}

/// A line of the protections table that applies to the user running p4.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Protection {
    /// Its line number in the table, which counts every user's lines: later lines win.
    pub line: u32,
    pub mode: ProtectionMode,
    pub principal: Principal,
    /// An IP address or range, or `*`.
    pub host: String,
    /// Without the `-` of an exclusion.
    pub path: String,
    /// An exclusion line (`-//depot/...`): removes the access instead of granting it.
    pub exclusion: bool,
}

/// The modes `p4 help protect` lists for files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtectionMode {
    List,
    Read,
    Open,
    Write,
    Admin,
    Owner,
    Super,
    Review,
    /// `=read`, `=branch`, `=open`, `=write`: that one right only.
    OnlyRead,
    OnlyBranch,
    OnlyOpen,
    OnlyWrite,
    /// One added by a newer server.
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    /// `*`: every user.
    Everyone,
    User(String),
    Group(String),
}

/// What `p4 info` reports about the server, enough to tell one server from another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    /// `None` until set with `p4 serverid`.
    pub server_id: Option<String>,
    /// A replica's commit server's `ServerID`; `None` on the commit server or a standalone.
    pub commit_server_id: Option<String>,
    /// `unknown` for an `rsh:` server.
    pub address: String,
    pub version: String,
    pub case_handling: CaseHandling,
    pub unicode: bool,
    /// For example `standard` or `forwarding-standby`.
    pub services: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseHandling {
    Sensitive,
    Insensitive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Depot {
    pub name: String,
    pub depot_type: DepotType,
    pub map: String,
    /// The number of slashes in a stream's name after the depot name. Stream depots only.
    pub stream_depth: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepotType {
    Local,
    Stream,
    Remote,
    Spec,
    Archive,
    Unload,
    Tangent,
    Graph,
    /// Any other type: one added by a newer server, or `extension` and `trait`, which only
    /// the server's hidden depots have.
    Other(String),
}

/// As `p4 depots` spells it.
impl fmt::Display for DepotType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DepotType::Local => "local",
            DepotType::Stream => "stream",
            DepotType::Remote => "remote",
            DepotType::Spec => "spec",
            DepotType::Archive => "archive",
            DepotType::Unload => "unload",
            DepotType::Tangent => "tangent",
            DepotType::Graph => "graph",
            DepotType::Other(name) => name,
        })
    }
}

/// Where the importer reads Perforce from. Only the implementation knows how: the importer
/// sees nothing but these domain types. The importer makes several calls at once, so an
/// implementation must not rely on them coming one at a time.
#[async_trait]
pub trait ChangeSource: Send + Sync {
    async fn server_info(&self) -> Result<ServerInfo>;

    /// The newest submitted changelist on the server.
    async fn latest_change(&self) -> Result<u64>;

    /// The server's depots, without its hidden ones. A depot it cannot read is left out.
    async fn depots(&self) -> Result<Vec<Depot>>;

    /// The first `limit` submitted changelists with files in the depot at `depot_root` (like
    /// `//test1/`) in `(after, upto]`, oldest first; `after: None` starts at the first.
    async fn submitted_changes(
        &self,
        depot_root: &str,
        after: Option<u64>,
        upto: u64,
        limit: usize,
    ) -> Result<Vec<u64>>;

    async fn describe(&self, change: u64) -> Result<Changelist>;

    /// The revision's content. The stream ends only after the content has been checked
    /// against the revision's `size` and `digest`; any failure, a mismatch included, is its
    /// last item.
    async fn print(&self, file: &FileRevision) -> Result<BoxStream<'static, Result<Bytes>>>;

    /// The integration records of each of these revisions, not of the files' other
    /// revisions, keyed by depot path: every file is in the result. One round trip per batch
    /// (the CLI source splits a large one into a few), so ask for a changelist's files at once.
    async fn integrations(
        &self,
        files: &[&FileRevision],
    ) -> Result<HashMap<String, Vec<Integration>>>;
}

#[cfg(test)]
mod tests;
