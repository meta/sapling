/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use fbthrift::compact_protocol;
use sorted_vector_map::SortedVectorMap;
use thrift_convert::ThriftConvert;

use crate::blob::Blob;
use crate::blob::BlobstoreValue;
use crate::blob::FileUnodeBlob;
use crate::blob::ManifestUnodeBlob;
use crate::errors::MononokeTypeError;
use crate::file_change::FileType;
use crate::path::MPathHash;
use crate::path::mpath_element::MPathElement;
use crate::thrift;
use crate::typed_hash::ChangesetId;
use crate::typed_hash::ContentId;
use crate::typed_hash::FileUnodeId;
use crate::typed_hash::FileUnodeIdContext;
use crate::typed_hash::ManifestUnodeId;
use crate::typed_hash::ManifestUnodeIdContext;

/// Unode is a filenode with fixed linknodes. They are designed to find file or directory history
/// quickly which can be used to answer "log" or "blame" requests.
/// The “u” stands for “unique.” There are two sorts of unodes.
/// A file unode is a structure that contains:
/// parent unode hashes
/// file content hash
/// file type - executable, symlink, regular etc
/// hashed form of the file path, which ensures uniqueness within the repository
/// linknode: the changeset that introduced this unode
///
/// NOTE: copy-from is NOT part of the hash. The goal is to make copy information mutable
/// so that we can retroactively change it
///
/// A tree unode (sometimes manifest unode) is a structure that contains:
/// parent unode hashes
/// subentries: a map from base name to (unode hash, type)
/// linknode: the changeset that introduced this unode
///
/// Q&A
/// How is the unode graph computed for a freshly received changeset?
///
/// Computing the unode graph for a changeset is an inductive process. For a changeset where the
/// unode graph has already been computed for all its parents:
/// 1. For each changed file, create a new unode with parents set to their corresponding unodes
///    and content set to the hash.
/// 2. Create new manifest nodes by applying changes and deletes recursively.
/// 3. Finalize the conversion by recording the mapping from changeset ID to root manifest unode hash.
///
/// How would a (commit, file name) pair be mapped to a unode?
///
/// External consumers can ask for data about a file name as it is at a particular commit.
/// Finding the unode would involve walking down the tree until the file is found.
/// (This process will not involve looking at linknodes, since linknodes map unodes to only
/// the commits that introduced that file version, not to all the commits that
/// contain that file version.
///
/// How can unodes contain linknodes? Don't linknodes form a cycle?
///
/// With the bonsai changeset model, the changeset is no longer computed based on unode hashes.
/// Instead, unodes are computed from changesets. This means that linknodes no longer form a cycle.
///
/// Why do file unodes have a path hash? Don't linknodes guarantee uniqueness already?
///
/// Consider a single commit that adds two new empty files at different locations.
/// Without the hash, the unodes for those files are going to be exactly the same.
///
///
/// Why have a path hash? Why not store the full path?
///
/// Unlike with linknodes, there appears to be no situation where one would want to go from a
/// unode to the path that it represents. So storing the path is just going to be a waste of space.
///
/// Why do tree unodes not have a path hash?
///
/// File unodes are repository-wide unique. Each tree contains at least one file unode.
/// This means that manifest unodes will also be unique by construction — no path hash required.
/// If Mononoke ever decides to support empty trees — perhaps representing empty directories? —
/// it would have to add a path hash.

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct FileUnode {
    parents: Vec<FileUnodeId>,
    content_id: ContentId,
    file_type: FileType,
    path_hash: MPathHash,
    linknode: ChangesetId,
}

impl FileUnode {
    pub fn new(
        parents: Vec<FileUnodeId>,
        content_id: ContentId,
        file_type: FileType,
        path_hash: MPathHash,
        linknode: ChangesetId,
    ) -> Self {
        Self {
            parents,
            content_id,
            file_type,
            path_hash,
            linknode,
        }
    }

    pub fn get_unode_id(&self) -> FileUnodeId {
        // FIXME: try avoid clone
        *self.clone().into_blob().id()
    }

    pub fn parents(&self) -> &Vec<FileUnodeId> {
        &self.parents
    }

    pub fn content_id(&self) -> &ContentId {
        &self.content_id
    }

    pub fn file_type(&self) -> &FileType {
        &self.file_type
    }

    pub fn linknode(&self) -> &ChangesetId {
        &self.linknode
    }

    pub(crate) fn from_thrift(t: thrift::unodes::FileUnode) -> Result<FileUnode> {
        let parents: Result<Vec<_>> = t
            .parents
            .into_iter()
            .map(FileUnodeId::from_thrift)
            .collect();
        let parents = parents?;
        let content_id = ContentId::from_thrift(t.content_id)?;
        let file_type = FileType::from_thrift(t.file_type)?;
        let path_hash = MPathHash::from_thrift(t.path_hash)?;
        let linknode = ChangesetId::from_thrift(t.linknode)?;
        Ok(FileUnode {
            parents,
            content_id,
            file_type,
            path_hash,
            linknode,
        })
    }

    pub(crate) fn into_thrift(self) -> thrift::unodes::FileUnode {
        let parents: Vec<_> = self.parents.into_iter().map(|p| p.into_thrift()).collect();

        thrift::unodes::FileUnode {
            parents,
            content_id: self.content_id.into_thrift(),
            file_type: self.file_type.into_thrift(),
            path_hash: self.path_hash.into_thrift(),
            linknode: self.linknode.into_thrift(),
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let thrift_tc = compact_protocol::deserialize(bytes)
            .with_context(|| MononokeTypeError::BlobDeserializeError("FileUnode".into()))?;
        Self::from_thrift(thrift_tc)
    }
}

impl BlobstoreValue for FileUnode {
    type Key = FileUnodeId;

    fn into_blob(self) -> FileUnodeBlob {
        let thrift = self.into_thrift();
        let data = compact_protocol::serialize(&thrift);
        let mut context = FileUnodeIdContext::new();
        context.update(&data);
        let id = context.finish();
        Blob::new(id, data)
    }

    fn from_blob(blob: Blob<Self::Key>) -> Result<Self> {
        let thrift_tc = compact_protocol::deserialize(blob.data().as_ref())
            .with_context(|| MononokeTypeError::BlobDeserializeError("FileUnode".into()))?;
        Self::from_thrift(thrift_tc)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct ManifestUnode {
    parents: Vec<ManifestUnodeId>,
    subentries: SortedVectorMap<MPathElement, UnodeEntry>,
    linknode: ChangesetId,
}

impl ManifestUnode {
    pub fn new(
        parents: Vec<ManifestUnodeId>,
        subentries: SortedVectorMap<MPathElement, UnodeEntry>,
        linknode: ChangesetId,
    ) -> Self {
        Self {
            parents,
            subentries,
            linknode,
        }
    }

    pub fn lookup(&self, basename: &MPathElement) -> Option<&UnodeEntry> {
        self.subentries.get(basename)
    }

    pub fn list(&self) -> impl Iterator<Item = (&MPathElement, &UnodeEntry)> {
        self.subentries.iter()
    }

    pub fn subentries(&self) -> &SortedVectorMap<MPathElement, UnodeEntry> {
        &self.subentries
    }

    pub fn into_subentries(self) -> SortedVectorMap<MPathElement, UnodeEntry> {
        self.subentries
    }

    pub fn parents(&self) -> &Vec<ManifestUnodeId> {
        &self.parents
    }

    pub fn linknode(&self) -> &ChangesetId {
        &self.linknode
    }

    pub fn get_unode_id(&self) -> ManifestUnodeId {
        // FIXME: try avoid clone
        *self.clone().into_blob().id()
    }

    pub(crate) fn from_thrift(t: thrift::unodes::ManifestUnode) -> Result<ManifestUnode> {
        let parents: Result<Vec<_>> = t
            .parents
            .into_iter()
            .map(ManifestUnodeId::from_thrift)
            .collect();
        let parents = parents?;

        let subentries = t
            .subentries
            .into_iter()
            .map(|(basename, unode_entry)| {
                let basename = MPathElement::from_thrift(basename)?;
                let unode_entry = UnodeEntry::from_thrift(unode_entry)?;

                Ok((basename, unode_entry))
            })
            .collect::<Result<_>>()?;

        let linknode = ChangesetId::from_thrift(t.linknode)?;
        Ok(ManifestUnode {
            parents,
            subentries,
            linknode,
        })
    }

    pub(crate) fn into_thrift(self) -> thrift::unodes::ManifestUnode {
        let parents: Vec<_> = self.parents.into_iter().map(|p| p.into_thrift()).collect();

        let subentries: SortedVectorMap<_, _> = self
            .subentries
            .into_iter()
            .map(|(basename, unode_entry)| (basename.into_thrift(), unode_entry.into_thrift()))
            .collect();

        thrift::unodes::ManifestUnode {
            parents,
            subentries,
            linknode: self.linknode.into_thrift(),
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let thrift_tc = compact_protocol::deserialize(bytes)
            .with_context(|| MononokeTypeError::BlobDeserializeError("ManifestUnode".into()))?;
        Self::from_thrift(thrift_tc)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum UnodeEntry {
    File(FileUnodeId),
    Directory(ManifestUnodeId),
}

impl UnodeEntry {
    pub(crate) fn from_thrift(t: thrift::unodes::UnodeEntry) -> Result<UnodeEntry> {
        match t {
            thrift::unodes::UnodeEntry::File(file_unode_id) => {
                let file_unode_id = FileUnodeId::from_thrift(file_unode_id)?;
                Ok(UnodeEntry::File(file_unode_id))
            }
            thrift::unodes::UnodeEntry::Directory(manifest_unode_id) => {
                let manifest_unode_id = ManifestUnodeId::from_thrift(manifest_unode_id)?;
                Ok(UnodeEntry::Directory(manifest_unode_id))
            }
            thrift::unodes::UnodeEntry::UnknownField(unknown) => bail!(
                "Unknown field encountered when parsing thrift::unodes::UnodeEntry: {unknown}",
            ),
        }
    }

    pub(crate) fn into_thrift(self) -> thrift::unodes::UnodeEntry {
        match self {
            UnodeEntry::File(file_unode_id) => {
                thrift::unodes::UnodeEntry::File(file_unode_id.into_thrift())
            }
            UnodeEntry::Directory(manifest_unode_id) => {
                thrift::unodes::UnodeEntry::Directory(manifest_unode_id.into_thrift())
            }
        }
    }

    pub fn is_directory(&self) -> bool {
        match self {
            UnodeEntry::File(_) => false,
            UnodeEntry::Directory(_) => true,
        }
    }
}

impl BlobstoreValue for ManifestUnode {
    type Key = ManifestUnodeId;

    fn into_blob(self) -> ManifestUnodeBlob {
        let thrift = self.into_thrift();
        let data = compact_protocol::serialize(&thrift);
        let mut context = ManifestUnodeIdContext::new();
        context.update(&data);
        let id = context.finish();
        Blob::new(id, data)
    }

    fn from_blob(blob: Blob<Self::Key>) -> Result<Self> {
        Self::from_bytes(blob.data().as_ref())
    }
}
