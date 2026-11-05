/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::time::Duration;

use async_trait::async_trait;
use edenapi_types::AlterSnapshotRequest;
use edenapi_types::AlterSnapshotResponse;
use edenapi_types::AnyFileContentId;
use edenapi_types::AnyId;
use edenapi_types::BlameResult;
use edenapi_types::BonsaiChangesetContent;
use edenapi_types::BookmarkEntry;
use edenapi_types::BookmarkKind;
use edenapi_types::CheckManifestPermissionRequest;
use edenapi_types::CheckManifestPermissionResponse;
use edenapi_types::CheckPathPermissionRequest;
use edenapi_types::CheckPathPermissionResponse;
use edenapi_types::CloudShareWorkspaceRequest;
use edenapi_types::CloudShareWorkspaceResponse;
use edenapi_types::CommitGraphEntry;
use edenapi_types::CommitGraphSegmentsEntry;
use edenapi_types::CommitHashLookupResponse;
use edenapi_types::CommitHashToLocationResponse;
use edenapi_types::CommitId;
use edenapi_types::CommitIdScheme;
use edenapi_types::CommitKnownResponse;
use edenapi_types::CommitLocationToHashRequest;
use edenapi_types::CommitLocationToHashResponse;
use edenapi_types::CommitMutationsResponse;
use edenapi_types::CommitRevlogData;
use edenapi_types::CommitTranslateIdResponse;
use edenapi_types::EphemeralExtendRequest;
use edenapi_types::EphemeralExtendResponse;
use edenapi_types::EphemeralPrepareResponse;
use edenapi_types::FetchSnapshotRequest;
use edenapi_types::FetchSnapshotResponse;
use edenapi_types::FileResponse;
use edenapi_types::FileSpec;
use edenapi_types::GetReferencesParams;
use edenapi_types::GetSmartlogByVersionParams;
use edenapi_types::GetSmartlogParams;
use edenapi_types::HgFilenodeData;
use edenapi_types::HgMutationEntryContent;
use edenapi_types::HistoricalVersionsParams;
use edenapi_types::HistoricalVersionsResponse;
use edenapi_types::HistoryEntry;
use edenapi_types::IdenticalChangesetContent;
use edenapi_types::LandStackResponse;
use edenapi_types::LookupResponse;
use edenapi_types::MirrorBookmarkMove;
use edenapi_types::PathHistoryRequestPaginationCursor;
use edenapi_types::PathHistoryResponse;
use edenapi_types::ReferencesDataResponse;
use edenapi_types::RenameWorkspaceRequest;
use edenapi_types::RenameWorkspaceResponse;
use edenapi_types::ReplayIdenticalMovesResponse;
use edenapi_types::RollbackWorkspaceRequest;
use edenapi_types::RollbackWorkspaceResponse;
use edenapi_types::SaplingRemoteApiServerError;
use edenapi_types::SetBookmarkResponse;
use edenapi_types::StreamingChangelogResponse;
use edenapi_types::SuffixQueryResponse;
use edenapi_types::TreeAttributes;
use edenapi_types::TreeEntry;
use edenapi_types::UpdateArchiveParams;
use edenapi_types::UpdateArchiveResponse;
use edenapi_types::UpdateReferencesParams;
use edenapi_types::UploadHgChangeset;
use edenapi_types::UploadToken;
use edenapi_types::UploadTokensResponse;
use edenapi_types::UploadTreeEntry;
use edenapi_types::UploadTreeResponse;
use edenapi_types::WorkspaceDataResponse;
use edenapi_types::WorkspacesDataResponse;
use edenapi_types::bookmark::Freshness;
use edenapi_types::cloud::SmartlogDataResponse;
use minibytes::Bytes;
use types::FetchContext;
use types::HgId;
use types::Key;
use types::RepoPathBuf;

use crate::errors::SaplingRemoteApiError;
use crate::response::Response;
use crate::response::ResponseMeta;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UploadLookupPolicy {
    PerformLookup,
    SkipLookup,
}

#[async_trait]
pub trait SaplingRemoteApi: Send + Sync + 'static {
    /// Returns the URL to describe the SaplingRemoteApi. The URL is intended
    /// to match configs like `paths.default`.
    fn url(&self) -> Option<String> {
        None
    }

    async fn health(&self) -> Result<ResponseMeta, SaplingRemoteApiError> {
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn capabilities(&self) -> Result<Vec<String>, SaplingRemoteApiError> {
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn files(
        &self,
        fctx: FetchContext,
        keys: Vec<Key>,
    ) -> Result<Response<FileResponse>, SaplingRemoteApiError> {
        let _ = fctx;
        let _ = keys;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn files_attrs(
        &self,
        fctx: FetchContext,
        reqs: Vec<FileSpec>,
    ) -> Result<Response<FileResponse>, SaplingRemoteApiError> {
        let _ = fctx;
        let _ = reqs;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn history(
        &self,
        keys: Vec<Key>,
        length: Option<u32>,
    ) -> Result<Response<HistoryEntry>, SaplingRemoteApiError> {
        let _ = (keys, length);
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn path_history(
        &self,
        commit: HgId,
        paths: Vec<RepoPathBuf>,
        limit: Option<u32>,
        cursor: Vec<PathHistoryRequestPaginationCursor>,
    ) -> Result<Response<PathHistoryResponse>, SaplingRemoteApiError> {
        let _ = (commit, paths, limit, cursor);
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn trees(
        &self,
        fctx: FetchContext,
        keys: Vec<Key>,
        attributes: Option<TreeAttributes>,
    ) -> Result<Response<Result<TreeEntry, SaplingRemoteApiServerError>>, SaplingRemoteApiError>
    {
        let _ = (fctx, keys, attributes);
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn commit_revlog_data(
        &self,
        hgids: Vec<HgId>,
    ) -> Result<Response<CommitRevlogData>, SaplingRemoteApiError> {
        let _ = hgids;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn commit_location_to_hash(
        &self,
        requests: Vec<CommitLocationToHashRequest>,
    ) -> Result<Vec<CommitLocationToHashResponse>, SaplingRemoteApiError> {
        let _ = requests;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn commit_hash_to_location(
        &self,
        master_heads: Vec<HgId>,
        hgids: Vec<HgId>,
    ) -> Result<Vec<CommitHashToLocationResponse>, SaplingRemoteApiError> {
        let _ = (master_heads, hgids);
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Return a subset of commits that are known by the server.
    /// This is similar to the "known" command in HG wireproto.
    async fn commit_known(
        &self,
        hgids: Vec<HgId>,
    ) -> Result<Vec<CommitKnownResponse>, SaplingRemoteApiError> {
        let _ = hgids;
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Return part of the commit graph that are ancestors of `heads`,
    /// excluding ancestors of `common`.
    ///
    /// This is `heads % common` (or `only(heads, common)`) expressed using HG
    /// revset, or `changelog.findmissing` in HG Python code base.
    ///
    /// If any of the nodes in `heads` and `common` are unknown by the server,
    /// it should be an error.
    async fn commit_graph(
        &self,
        heads: Vec<HgId>,
        common: Vec<HgId>,
    ) -> Result<Vec<CommitGraphEntry>, SaplingRemoteApiError> {
        let _ = (heads, common);
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Returns a segmented representation of the part of the commit graph
    /// consisting of ancestors of `heads`, excluding ancestors of `common`.
    ///
    /// If any of the nodes in `heads` and `common` are unknown by the server,
    /// it should be an error.
    async fn commit_graph_segments(
        &self,
        heads: Vec<HgId>,
        common: Vec<HgId>,
    ) -> Result<Vec<CommitGraphSegmentsEntry>, SaplingRemoteApiError> {
        let _ = (heads, common);
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Return matching full hashes of hex hash prefix
    async fn hash_prefixes_lookup(
        &self,
        prefixes: Vec<String>,
    ) -> Result<Vec<CommitHashLookupResponse>, SaplingRemoteApiError> {
        let _ = prefixes;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn bookmarks(
        &self,
        bookmarks: Vec<String>,
        freshness: Option<Freshness>,
    ) -> Result<Vec<BookmarkEntry>, SaplingRemoteApiError> {
        let _ = (bookmarks, freshness);
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// List bookmarks matching patterns (replacement for wireproto listkeyspatterns).
    /// Patterns can be exact bookmark names or prefix patterns ending with '*'.
    /// A prefix pattern matches every non-scratch bookmark with that prefix, and
    /// scratch bookmarks too if `kinds` contains `Scratch`; `kinds` does not tell
    /// the other kinds apart. An exact name matches a bookmark of any kind.
    async fn list_bookmark_patterns(
        &self,
        patterns: Vec<String>,
        kinds: Vec<BookmarkKind>,
    ) -> Result<Vec<BookmarkEntry>, SaplingRemoteApiError> {
        let _ = (patterns, kinds);
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Create, delete, or move a bookmark
    ///
    /// Both `from` and `to` can be None, but not both:
    ///   * if `from` is None, the bookmark will be created at `to`.
    ///   * if `to` is None, the bookmark will be deleted.
    async fn set_bookmark(
        &self,
        bookmark: String,
        to: Option<HgId>,
        from: Option<HgId>,
        pushvars: HashMap<String, String>,
    ) -> Result<SetBookmarkResponse, SaplingRemoteApiError> {
        let _ = (bookmark, to, from, pushvars);
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Mirror a contiguous chain of source bookmark moves to a `*_shadow`
    /// replica. modern_sync uses this to keep the replica's bookmark and
    /// bookmarks_update_log identical to the source, row for row: the replica
    /// reuses each source move's log id, changesets, and reason.
    ///
    /// `moves` must be non-empty, ordered by strictly increasing `log_id`, and
    /// contiguous (each move's `from` equals the previous move's `to`). If the
    /// first move's `from` is `None`, the server creates the bookmark.
    async fn replay_identical_moves(
        &self,
        bookmark: String,
        moves: Vec<MirrorBookmarkMove>,
        pushvars: HashMap<String, String>,
    ) -> Result<ReplayIdenticalMovesResponse, SaplingRemoteApiError> {
        let _ = (bookmark, moves, pushvars);
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Land a stack of commits, rebasing them onto the specified bookmark
    /// and updating the bookmark to the top of the rebased stack.
    ///
    /// * bookmark: the name of the bookmark to land to.
    /// * head: the head commit of the stack that is to be landed.
    /// * base: the merge base of the head commit and the bookmark. A caller
    ///   whose view of the bookmark is behind the server's passes the same
    ///   commit; the server rejects any other. The commits that descend from
    ///   it and are ancestors of the head are rebased onto the bookmark, and
    ///   must not be public. Other ancestors of the head that are not
    ///   ancestors of the bookmark, such as the other side of a merge, land
    ///   unchanged.
    /// * pushvars: the pushvars to use when landing the stack.
    async fn land_stack(
        &self,
        bookmark: String,
        head: HgId,
        base: HgId,
        pushvars: HashMap<String, String>,
    ) -> Result<LandStackResponse, SaplingRemoteApiError> {
        let _ = (bookmark, head, base, pushvars);
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Lookup items and return signed upload tokens if an item has been uploaded
    /// Supports: file content, hg filenode, hg tree, hg changeset
    async fn lookup_batch(
        &self,
        items: Vec<AnyId>,
        bubble_id: Option<NonZeroU64>,
        copy_from_bubble_id: Option<NonZeroU64>,
    ) -> Result<Vec<LookupResponse>, SaplingRemoteApiError> {
        let _ = (items, bubble_id, copy_from_bubble_id);
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Upload files content
    async fn process_files_upload(
        &self,
        data: Vec<(AnyFileContentId, Bytes)>,
        bubble_id: Option<NonZeroU64>,
        copy_from_bubble_id: Option<NonZeroU64>,
        lookup_policy: UploadLookupPolicy,
    ) -> Result<Response<UploadToken>, SaplingRemoteApiError> {
        let _ = (data, bubble_id, copy_from_bubble_id, lookup_policy);
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Upload list of hg filenodes
    async fn upload_filenodes_batch(
        &self,
        items: Vec<HgFilenodeData>,
    ) -> Result<Response<UploadTokensResponse>, SaplingRemoteApiError> {
        let _ = items;
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Upload list of trees
    ///
    /// `items` must list children before their parents: they may be split
    /// across several requests, and the server orders trees only within one.
    async fn upload_trees_batch(
        &self,
        items: Vec<UploadTreeEntry>,
    ) -> Result<Response<UploadTreeResponse>, SaplingRemoteApiError> {
        let _ = items;
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Upload list of changesets
    async fn upload_changesets(
        &self,
        changesets: Vec<UploadHgChangeset>,
        mutations: Vec<HgMutationEntryContent>,
    ) -> Result<Response<UploadTokensResponse>, SaplingRemoteApiError> {
        let _ = (changesets, mutations);
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Upload list of changesets with bonsai and hg info
    async fn upload_identical_changesets(
        &self,
        changesets: Vec<IdenticalChangesetContent>,
    ) -> Result<Response<UploadTokensResponse>, SaplingRemoteApiError> {
        let _ = changesets;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn upload_bonsai_changeset(
        &self,
        changeset: BonsaiChangesetContent,
        bubble_id: Option<NonZeroU64>,
    ) -> Result<UploadTokensResponse, SaplingRemoteApiError> {
        let _ = (changeset, bubble_id);
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn ephemeral_prepare(
        &self,
        custom_duration: Option<Duration>,
        labels: Option<Vec<String>>,
    ) -> Result<EphemeralPrepareResponse, SaplingRemoteApiError> {
        let _ = (custom_duration, labels);
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn ephemeral_extend(
        &self,
        request: EphemeralExtendRequest,
    ) -> Result<EphemeralExtendResponse, SaplingRemoteApiError> {
        let _ = request;
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Fetch information about the requested snapshot
    async fn fetch_snapshot(
        &self,
        request: FetchSnapshotRequest,
    ) -> Result<FetchSnapshotResponse, SaplingRemoteApiError> {
        let _ = request;
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Alter the properties of an existing snapshot
    async fn alter_snapshot(
        &self,
        request: AlterSnapshotRequest,
    ) -> Result<AlterSnapshotResponse, SaplingRemoteApiError> {
        let _ = request;
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Download single file from upload token
    async fn download_file(&self, token: UploadToken) -> Result<Bytes, SaplingRemoteApiError> {
        let _ = token;
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Download mutation info related to given commits
    async fn commit_mutations(
        &self,
        commits: Vec<HgId>,
    ) -> Result<Vec<CommitMutationsResponse>, SaplingRemoteApiError> {
        let _ = commits;
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Translate commit IDs to a different commit ID scheme
    async fn commit_translate_id(
        &self,
        commits: Vec<CommitId>,
        scheme: CommitIdScheme,
        from_repo: Option<String>,
        to_repo: Option<String>,
        lookup_behavior: Option<String>,
    ) -> Result<Response<CommitTranslateIdResponse>, SaplingRemoteApiError> {
        let _ = (commits, scheme, from_repo, to_repo, lookup_behavior);
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Fetch metadata describing the last commit to modify each line in given file(s)
    async fn blame(&self, files: Vec<Key>) -> Result<Response<BlameResult>, SaplingRemoteApiError> {
        let _ = files;
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Retrieves users workspace from commit cloud
    async fn cloud_workspace(
        &self,
        workspace: String,
        reponame: String,
    ) -> Result<WorkspaceDataResponse, SaplingRemoteApiError> {
        let _ = (workspace, reponame);
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Retrieves workspaces that match a prefix in from commit cloud
    async fn cloud_workspaces(
        &self,
        prefix: String,
        reponame: String,
    ) -> Result<WorkspacesDataResponse, SaplingRemoteApiError> {
        let _ = (prefix, reponame);
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn cloud_references(
        &self,
        data: GetReferencesParams,
    ) -> Result<ReferencesDataResponse, SaplingRemoteApiError> {
        let _ = data;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn cloud_update_references(
        &self,
        data: UpdateReferencesParams,
    ) -> Result<ReferencesDataResponse, SaplingRemoteApiError> {
        let _ = data;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn cloud_smartlog(
        &self,
        data: GetSmartlogParams,
    ) -> Result<SmartlogDataResponse, SaplingRemoteApiError> {
        let _ = data;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn cloud_share_workspace(
        &self,
        data: CloudShareWorkspaceRequest,
    ) -> Result<CloudShareWorkspaceResponse, SaplingRemoteApiError> {
        let _ = data;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn cloud_update_archive(
        &self,
        data: UpdateArchiveParams,
    ) -> Result<UpdateArchiveResponse, SaplingRemoteApiError> {
        let _ = data;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn cloud_rename_workspace(
        &self,
        data: RenameWorkspaceRequest,
    ) -> Result<RenameWorkspaceResponse, SaplingRemoteApiError> {
        let _ = data;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn cloud_smartlog_by_version(
        &self,
        data: GetSmartlogByVersionParams,
    ) -> Result<SmartlogDataResponse, SaplingRemoteApiError> {
        let _ = data;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn cloud_historical_versions(
        &self,
        data: HistoricalVersionsParams,
    ) -> Result<HistoricalVersionsResponse, SaplingRemoteApiError> {
        let _ = data;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn cloud_rollback_workspace(
        &self,
        data: RollbackWorkspaceRequest,
    ) -> Result<RollbackWorkspaceResponse, SaplingRemoteApiError> {
        let _ = data;
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn cloud_other_repo_workspaces(
        &self,
        workspace: String,
    ) -> Result<WorkspacesDataResponse, SaplingRemoteApiError> {
        let _ = workspace;
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Fetch files matching the given suffixes on the given commit
    async fn suffix_query(
        &self,
        commit: CommitId,
        suffixes: Vec<String>,
        prefixes: Option<Vec<String>>,
    ) -> Result<Response<SuffixQueryResponse>, SaplingRemoteApiError> {
        let _ = (commit, suffixes, prefixes);
        Err(SaplingRemoteApiError::NotSupported)
    }

    async fn streaming_clone(
        &self,
        tag: Option<String>,
    ) -> Result<Response<StreamingChangelogResponse>, SaplingRemoteApiError> {
        let _ = tag;
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Check whether the caller has access to the given paths.
    async fn check_path_permission(
        &self,
        request: CheckPathPermissionRequest,
    ) -> Result<Response<CheckPathPermissionResponse>, SaplingRemoteApiError> {
        let _ = request;
        Err(SaplingRemoteApiError::NotSupported)
    }

    /// Check whether the caller has access to the given manifest IDs.
    async fn check_manifest_permission(
        &self,
        request: CheckManifestPermissionRequest,
    ) -> Result<Response<CheckManifestPermissionResponse>, SaplingRemoteApiError> {
        let _ = request;
        Err(SaplingRemoteApiError::NotSupported)
    }
}
