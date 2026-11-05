/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Write;
use std::num::NonZeroU64;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use blob::Blob;
use configmodel::Config;
use configmodel::ConfigExt;
use dag::Location;
use dag::Set;
use dag::Vertex;
use dag::ops::DagAlgorithm;
use dag::ops::DagExportPullData;
use dag::ops::PrefixLookup;
use dag::protocol::AncestorPath;
use dag::protocol::RemoteIdConvertProtocol;
use edenapi::Response;
use edenapi::ResponseMeta;
use edenapi::SaplingRemoteApi;
use edenapi::SaplingRemoteApiError;
use edenapi::api::UploadLookupPolicy;
use edenapi::configmodel;
use edenapi::types::AnyFileContentId;
use edenapi::types::AnyId;
use edenapi::types::BookmarkEntry;
use edenapi::types::BookmarkKind;
use edenapi::types::CheckManifestPermissionRequest;
use edenapi::types::CheckManifestPermissionResponse;
use edenapi::types::CheckPathPermissionAclEntry;
use edenapi::types::CheckPathPermissionData;
use edenapi::types::CheckPathPermissionRequest;
use edenapi::types::CheckPathPermissionResponse;
use edenapi::types::CommitGraphEntry;
use edenapi::types::CommitGraphSegments;
use edenapi::types::CommitGraphSegmentsEntry;
use edenapi::types::CommitHashLookupResponse;
use edenapi::types::CommitHashToLocationResponse;
use edenapi::types::CommitId;
use edenapi::types::CommitIdScheme;
use edenapi::types::CommitKnownResponse;
use edenapi::types::CommitLocationToHashRequest;
use edenapi::types::CommitLocationToHashResponse;
use edenapi::types::CommitMutationsResponse;
use edenapi::types::CommitRevlogData;
use edenapi::types::CommitTranslateIdResponse;
use edenapi::types::Extra;
use edenapi::types::FileAuxData;
use edenapi::types::FileContent;
use edenapi::types::FileEntry;
use edenapi::types::FileMetadata;
use edenapi::types::FileResponse;
use edenapi::types::FileSpec;
use edenapi::types::HgChangesetContent;
use edenapi::types::HgFilenodeData;
use edenapi::types::HgId;
use edenapi::types::HgMutationEntryContent;
use edenapi::types::HistoryEntry;
use edenapi::types::IndexableId;
use edenapi::types::Key;
use edenapi::types::LandStackData;
use edenapi::types::LandStackResponse;
use edenapi::types::LookupResponse;
use edenapi::types::LookupResult;
use edenapi::types::NodeInfo;
use edenapi::types::Parents;
use edenapi::types::RepoPathBuf;
use edenapi::types::SaplingRemoteApiServerError;
use edenapi::types::SaplingRemoteApiServerErrorKind;
use edenapi::types::ServerError;
use edenapi::types::SetBookmarkResponse;
use edenapi::types::SuffixQueryResponse;
use edenapi::types::TreeAttributes;
use edenapi::types::TreeChildEntry;
use edenapi::types::TreeChildFileEntry;
use edenapi::types::TreeEntry;
use edenapi::types::UploadHgChangeset;
use edenapi::types::UploadToken;
use edenapi::types::UploadTokenData;
use edenapi::types::UploadTokensResponse;
use edenapi::types::UploadTreeEntry;
use edenapi::types::UploadTreeResponse;
use edenapi::types::make_hash_lookup_request;
use edenapi_trait as edenapi;
use edenapi_types::bookmark::Freshness;
use format_util::HgCommitFields;
use format_util::HgCommitLazyFields;
use format_util::git_sha1_deserialize;
use format_util::hg_sha1_deserialize;
use futures::StreamExt;
use futures::stream::BoxStream;
use futures::stream::TryStreamExt;
use http::StatusCode;
use http::Version;
use manifest::DiffType;
use manifest::FsNodeMetadata;
use manifest::List;
use manifest::Manifest;
use manifest::PersistOpts;
use manifest_augmented_tree::AugmentedTreeWithDigest;
use manifest_tree::Flag;
use manifest_tree::TreeManifest;
use minibytes::Bytes;
use minibytes::Text;
use mutationstore::MutationEntry;
use nonblocking::non_blocking_result;
use pathmatcher::AlwaysMatcher;
use repourl::RepoUrl;
use storemodel::KeyStore;
use storemodel::SerializationFormat;
use storemodel::types::FetchContext;
use tracing::debug;
use tracing::error;
use tracing::trace;

use crate::EagerRepo;

impl EagerRepo {
    /// Load file/tree store and bookmark changes from disk.
    ///
    /// This is intended to be used by SaplingRemoteApi impls so content fetched
    /// via SaplingRemoteApi (during testing) is always fresh. It re-opens the
    /// MetaLog to pick up bookmark changes made by other EagerRepo instances
    /// (e.g. after a push operation via an eagerpeer).
    pub(crate) fn refresh_for_api(&self) -> Result<()> {
        let _ = self.store.flush();
        self.refresh_metalog()?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl SaplingRemoteApi for EagerRepo {
    fn url(&self) -> Option<String> {
        Some(format!("eager:{}", self.dir.display()))
    }

    async fn health(&self) -> edenapi::Result<ResponseMeta> {
        Ok(default_response_meta())
    }

    async fn capabilities(&self) -> Result<Vec<String>, SaplingRemoteApiError> {
        let mut caps = vec![
            "segmented-changelog".to_string(),
            "commit-graph-segments".to_string(),
            // Inform client that we only support sha1 content addressing.
            "sha1-only".to_string(),
            // Inform the client that we support most common sapling operations like files, trees, blame etc. but not commit graph segments or commit cloud
            "sapling-common".to_string(),
        ];
        if matches!(self.format(), SerializationFormat::Git) {
            caps.push("git-format".to_string());
        }
        if matches!(self.extension_name(), Some("virtual-repo")) {
            caps.push("invalid-hash".to_string());
        }
        Ok(caps)
    }

    async fn files(
        &self,
        _fctx: FetchContext,
        keys: Vec<Key>,
    ) -> edenapi::Result<Response<FileResponse>> {
        debug!("files {}", debug_key_list(&keys));
        self.refresh_for_api()?;
        let mut values = Vec::with_capacity(keys.len());
        for key in keys {
            let id = key.hgid;
            let data = self.get_sha1_blob_for_api(id, "files")?;

            let (parents, body) = sha1_blob_to_parents_body(&data, self.format())?;

            let entry = FileEntry {
                key: key.clone(),
                parents,
                content: Some(FileContent {
                    hg_file_blob: body,
                    metadata: Default::default(),
                }),
                aux_data: None,
            };
            let response = FileResponse {
                key,
                result: Ok(entry),
            };
            values.push(Ok(response));
        }
        Ok(convert_to_response(values))
    }

    async fn files_attrs(
        &self,
        _fctx: FetchContext,
        reqs: Vec<FileSpec>,
    ) -> edenapi::Result<Response<FileResponse>> {
        ::fail::fail_point!("eagerepo::api::files_attrs", |_| {
            Err(SaplingRemoteApiError::HttpError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: "failpoint".to_string(),
                headers: Box::default(),
                url: self.url("files_attrs"),
            })
        });

        debug!("files_attrs {}", debug_spec_list(&reqs));
        self.refresh_for_api()?;
        let mut values = Vec::with_capacity(reqs.len());
        for spec in reqs {
            let key = spec.key;
            let id = key.hgid;
            let data = self.get_sha1_blob_for_api(id, "files_attrs")?;

            let (parents, body) = sha1_blob_to_parents_body(&data, self.format())?;

            let mut entry = FileEntry {
                key: key.clone(),
                parents,
                content: None,
                aux_data: None,
            };

            if spec.attrs.aux_data {
                let (pure_content, copy_from) =
                    file_body_to_file_content_and_copy_from(&body, self.format());

                let mut aux_data = FileAuxData::from_content(&Blob::Bytes(pure_content));
                aux_data.file_header_metadata = Some(copy_from);

                entry.aux_data = Some(aux_data);
            }

            if spec.attrs.content {
                entry.content = Some(FileContent {
                    hg_file_blob: body,
                    metadata: Default::default(),
                });
            }

            let response = FileResponse {
                key,
                result: Ok(entry),
            };
            values.push(Ok(response));
        }
        Ok(convert_to_response(values))
    }

    async fn history(
        &self,
        keys: Vec<Key>,
        _length: Option<u32>,
    ) -> edenapi::Result<Response<HistoryEntry>> {
        debug!("history {}", debug_key_list(&keys));
        self.refresh_for_api()?;
        let mut values = Vec::new();
        let mut visited: HashSet<Key> = Default::default();
        let mut to_visit: Vec<Key> = keys;
        while let Some(key) = to_visit.pop() {
            if !visited.insert(key.clone()) {
                continue;
            }

            // Don't report missing files as errors. This matches Mononoke's behavior.
            let Some(data) = self.opt_sha1_blob_for_api(key.hgid, "history")? else {
                continue;
            };

            let (parents, body) = sha1_blob_to_parents_body(&data, self.format())?;

            // NOTE: Order of p1, p2 are not preserved, unlike revlog hg.
            // It should be okay correctness-wise.
            let (p1, p2) = parents.into_nodes();
            let mut key1 = Key {
                path: key.path.clone(),
                hgid: p1,
            };
            let mut key2 = Key {
                path: key.path.clone(),
                hgid: p2,
            };
            if let Some(renamed_from) = extract_rename(&body) {
                if p1.is_null() {
                    key1 = renamed_from;
                } else {
                    key2 = renamed_from;
                }
            }
            if !p1.is_null() {
                to_visit.push(key1.clone());
            }
            if !p2.is_null() {
                to_visit.push(key2.clone());
            }
            let entry = HistoryEntry {
                key,
                nodeinfo: NodeInfo {
                    parents: [key1, key2],
                    linknode: *HgId::null_id(),
                },
            };
            values.push(Ok(entry));
        }
        Ok(convert_to_response(values))
    }

    async fn trees(
        &self,
        _fctx: FetchContext,
        keys: Vec<Key>,
        attributes: Option<TreeAttributes>,
    ) -> edenapi::Result<Response<Result<TreeEntry, SaplingRemoteApiServerError>>> {
        debug!("trees {} {:?}", debug_key_list(&keys), attributes);

        ::fail::fail_point!("eagerepo::api::trees", |_| {
            Err(SaplingRemoteApiError::HttpError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: "failpoint".to_string(),
                headers: Box::default(),
                url: self.url("trees"),
            })
        });

        self.refresh_for_api()?;
        let mut values = Vec::new();
        let attributes = attributes.unwrap_or_default();
        if attributes.augmented_trees {
            for key in keys {
                match self
                    .get_augmented_tree_blob_with_digest_for_api(key.hgid, "trees")
                    .await
                {
                    Ok(tree) => {
                        let augmented_tree_with_digest =
                            AugmentedTreeWithDigest::try_deserialize(std::io::Cursor::new(tree))?;

                        if self.enforce_server_acls() && self.tree_has_slacl(key.hgid)? {
                            values.push(Ok(Err(SaplingRemoteApiServerError {
                                key: Some(key.clone()),
                                err: SaplingRemoteApiServerErrorKind::PermissionDenied {
                                    tree_id: key.hgid,
                                    request_acl: crate::eager_repo::EAGER_PLACEHOLDER_ACL
                                        .to_string(),
                                    denial_message: self.server_denial_message(),
                                },
                            })));
                            continue;
                        }

                        let mut converted_entry: TreeEntry =
                            TreeEntry::try_from(augmented_tree_with_digest).map_err(|err| {
                                SaplingRemoteApiServerError::with_key(key.clone(), err)
                            })?;
                        // Match the key in the response and in the request!
                        // TreeEntry produced from the augmented tree format doesn't contain path for itself, only for the children.
                        converted_entry.key = key;
                        // Clean up fields that were not requested, since the augmented trees format contains
                        // everything by default
                        if !attributes.manifest_blob {
                            converted_entry.with_data(None);
                        }
                        if !attributes.parents {
                            converted_entry.with_parents(None);
                        }
                        if !attributes.child_metadata {
                            converted_entry.with_children(None);
                        }
                        values.push(Ok(Ok(converted_entry)));
                    }
                    Err(e) => values.push(Err(e)),
                }
            }
        } else {
            for key in keys {
                let sha1_blob = self.get_sha1_blob_for_api(key.hgid, "trees")?;
                let (parents, body) = sha1_blob_to_parents_body(&sha1_blob, self.format())?;
                let mut entry = TreeEntry {
                    key: key.clone(),
                    ..Default::default()
                };

                if attributes.manifest_blob {
                    entry.data = Some(body.clone());
                }

                if attributes.parents {
                    entry.parents = Some(parents);
                }

                if attributes.child_metadata {
                    let mut children: Vec<Result<TreeChildEntry, SaplingRemoteApiServerError>> =
                        Vec::new();

                    let tree_entry = manifest_tree::TreeEntry(body, self.format());
                    for child in tree_entry.elements() {
                        let child = match child {
                            Ok(child) => child,
                            Err(err) => {
                                children.push(Err(SaplingRemoteApiServerError::with_key(
                                    key.clone(),
                                    err,
                                )));
                                continue;
                            }
                        };

                        match child.flag {
                            Flag::File(_) => {
                                let file_sha1_blob =
                                    self.get_sha1_blob_for_api(child.hgid, "trees (aux)")?;
                                let (_file_parents, file_body) =
                                    sha1_blob_to_parents_body(&file_sha1_blob, self.format())?;

                                let (file_body, copy_from) =
                                    file_body_to_file_content_and_copy_from(
                                        &file_body,
                                        self.format(),
                                    );

                                let mut aux_data =
                                    FileAuxData::from_content(&Blob::Bytes(file_body));
                                aux_data.file_header_metadata = Some(copy_from);

                                children.push(Ok(TreeChildEntry::File(TreeChildFileEntry {
                                    key: Key::new(
                                        RepoPathBuf::from_string(child.component.to_string())
                                            .map_err(anyhow::Error::from)?,
                                        child.hgid,
                                    ),
                                    file_metadata: Some(FileMetadata::from(aux_data)),
                                })));
                            }
                            Flag::Directory => {}
                        }
                    }

                    entry.children = Some(children);
                }

                values.push(Ok(Ok(entry)));
            }
        }
        Ok(convert_to_response(values))
    }

    async fn commit_revlog_data(
        &self,
        hgids: Vec<HgId>,
    ) -> edenapi::Result<Response<CommitRevlogData>> {
        debug!("revlog_data {}", debug_hgid_list(&hgids));
        self.refresh_for_api()?;
        let mut values = Vec::new();
        for id in hgids {
            let data = self.get_sha1_blob_for_api(id, "commit_revlog_data")?;
            let data = CommitRevlogData {
                hgid: id,
                revlog_data: match self.format() {
                    SerializationFormat::Hg => data,
                    SerializationFormat::Git => {
                        // For Git, just return the commit data without hesders.
                        let git_commit_data = git_sha1_deserialize(&data)?.0;
                        data.slice_to_bytes(git_commit_data)
                    }
                },
            };
            values.push(Ok(data));
        }
        Ok(convert_to_response(values))
    }

    async fn commit_location_to_hash(
        &self,
        requests: Vec<CommitLocationToHashRequest>,
    ) -> edenapi::Result<Vec<CommitLocationToHashResponse>> {
        self.refresh_for_api()?;
        let path_names: Vec<(AncestorPath, Vec<Vertex>)> = {
            let paths: Vec<AncestorPath> = requests
                .into_iter()
                .map(|r| AncestorPath {
                    x: Vertex::copy_from(r.location.descendant.as_ref()),
                    n: r.location.distance,
                    batch_size: r.count,
                })
                .collect();
            self.dag()
                .await
                .resolve_relative_paths_to_names(paths)
                .await
                .map_err(map_dag_err)?
        };

        check_convert_to_hgid(path_names.iter().flat_map(|i| i.1.iter()))?;
        check_convert_to_hgid(path_names.iter().map(|i| &i.0.x))?;

        let values: edenapi::Result<Vec<CommitLocationToHashResponse>> = path_names
            .into_iter()
            .map(|(p, ns)| {
                let count = ns.len();
                let response = CommitLocationToHashResponse {
                    location: Location {
                        descendant: HgId::from_slice(p.x.as_ref()).unwrap(), // unwrap: checked above
                        distance: p.n,
                    },
                    hgids: ns
                        .into_iter()
                        .map(|n| HgId::from_slice(n.as_ref()).unwrap()) // unwrap: checked above
                        .collect(),
                    count: count as _,
                };
                Ok(response)
            })
            .collect();

        values
    }

    async fn commit_hash_to_location(
        &self,
        master_heads: Vec<HgId>,
        hgids: Vec<HgId>,
    ) -> edenapi::Result<Vec<CommitHashToLocationResponse>> {
        self.refresh_for_api()?;
        let path_names: Vec<(AncestorPath, Vec<Vertex>)> = {
            let heads: Vec<Vertex> = to_vec_vertex(&master_heads);
            let names: Vec<Vertex> = to_vec_vertex(&hgids);
            self.dag()
                .await
                .resolve_names_to_relative_paths(heads, names)
                .await
                .map_err(map_dag_err)?
        };

        check_convert_to_hgid(path_names.iter().flat_map(|i| i.1.iter()))?;
        check_convert_to_hgid(path_names.iter().map(|i| &i.0.x))?;

        let values: edenapi::Result<Vec<CommitHashToLocationResponse>> = path_names
            .into_iter()
            .flat_map(|(p, ns)| {
                ns.into_iter()
                    .enumerate()
                    .map(|(i, n)| {
                        CommitHashToLocationResponse {
                            hgid: HgId::from_slice(n.as_ref()).unwrap(), // unwrap: checked above
                            result: Ok(Some(Location {
                                descendant: HgId::from_slice(p.x.as_ref()).unwrap(), // unwrap: checked above
                                distance: p.n + (i as u64),
                            })),
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .map(Ok)
            .collect();

        // For hgids outside the master group, just ignore them.
        // It's okay to return them with result "None" too.

        values
    }

    async fn commit_known(&self, hgids: Vec<HgId>) -> edenapi::Result<Vec<CommitKnownResponse>> {
        debug!("commit_known {}", debug_hgid_list(&hgids));
        self.refresh_for_api()?;
        let mut values = Vec::new();
        for id in hgids {
            let known = self.get_sha1_blob(id).map_err(map_crate_err)?.is_some();
            let response = CommitKnownResponse {
                hgid: id,
                known: Ok(known),
            };
            values.push(response);
        }
        Ok(values)
    }

    async fn commit_graph(
        &self,
        heads: Vec<HgId>,
        common: Vec<HgId>,
    ) -> Result<Vec<CommitGraphEntry>, SaplingRemoteApiError> {
        debug!(
            "commit_graph {} {}",
            debug_hgid_list(&heads),
            debug_hgid_list(&common),
        );
        self.refresh_for_api()?;
        let heads = to_set(&heads);
        let common = to_set(&common);
        let graph = self
            .dag()
            .await
            .only(heads, common)
            .await
            .map_err(map_dag_err)?;
        let stream = graph.iter_rev().await.map_err(map_dag_err)?;
        let stream: BoxStream<edenapi::Result<CommitGraphEntry>> = stream
            .then(|s| async move {
                let s = s?;
                let hgid = HgId::from_slice(s.as_ref()).unwrap();
                let parents = self.dag().await.parent_names(s).await?;
                let parents: Vec<HgId> = parents
                    .into_iter()
                    .map(|v| HgId::from_slice(v.as_ref()).unwrap())
                    .collect();
                let entry = CommitGraphEntry {
                    hgid,
                    parents,
                    is_draft: None,
                };
                Ok(entry)
            })
            .map_err(map_dag_err)
            .boxed();
        let values: edenapi::Result<Vec<CommitGraphEntry>> = stream.try_collect().await;
        values
    }

    async fn commit_graph_segments(
        &self,
        heads: Vec<HgId>,
        common: Vec<HgId>,
    ) -> Result<Vec<CommitGraphSegmentsEntry>, SaplingRemoteApiError> {
        ::fail::fail_point!("eagerepo::api::commitgraphsegments", |_| {
            Err(SaplingRemoteApiError::NotSupported)
        });

        debug!(
            "commit_graph_segments {} {}",
            debug_hgid_list(&heads),
            debug_hgid_list(&common),
        );
        self.refresh_for_api()?;
        let heads = to_set(&heads);
        let common = to_set(&common);
        let graph = self
            .dag()
            .await
            .only(heads, common)
            .await
            .map_err(map_dag_err)?;

        let graph_segments: CommitGraphSegments = self
            .dag()
            .await
            .export_pull_data(&graph)
            .await
            .map_err(map_dag_err)?
            .try_into()?;

        Ok(graph_segments.segments)
    }

    async fn bookmarks(
        &self,
        bookmarks: Vec<String>,
        _freshness: Option<Freshness>,
    ) -> edenapi::Result<Vec<BookmarkEntry>> {
        debug!("bookmarks {}", debug_string_list(&bookmarks));
        self.refresh_for_api()?;
        let mut values = Vec::new();
        let map = self.get_bookmarks_map().map_err(map_crate_err)?;
        for name in bookmarks {
            let opt_id = map.get(&name).cloned();
            let entry = BookmarkEntry {
                bookmark: name,
                hgid: opt_id,
            };
            values.push(entry);
        }
        Ok(values)
    }

    async fn list_bookmark_patterns(
        &self,
        patterns: Vec<String>,
        _kinds: Vec<BookmarkKind>,
    ) -> edenapi::Result<Vec<BookmarkEntry>> {
        debug!("list_bookmark_patterns {}", debug_string_list(&patterns));
        self.refresh_for_api()?;
        let map = self.get_bookmarks_map().map_err(map_crate_err)?;
        let mut values = Vec::new();
        for pattern in &patterns {
            if let Some(prefix) = pattern.strip_suffix('*') {
                // Glob pattern: match all bookmarks with this prefix
                for (name, id) in &map {
                    if name.starts_with(prefix) {
                        values.push(BookmarkEntry {
                            bookmark: name.clone(),
                            hgid: Some(*id),
                        });
                    }
                }
            } else {
                // Exact match
                let opt_id = map.get(pattern).cloned();
                values.push(BookmarkEntry {
                    bookmark: pattern.clone(),
                    hgid: opt_id,
                });
            }
        }
        Ok(values)
    }

    async fn set_bookmark(
        &self,
        bookmark: String,
        to: Option<HgId>,
        from: Option<HgId>,
        _pushvars: HashMap<String, String>,
    ) -> Result<SetBookmarkResponse, SaplingRemoteApiError> {
        debug!("bookmarks {:?} -> {:?}", from, to);
        self.refresh_for_api()?;

        let mut bms = self.get_bookmarks_map().map_err(map_crate_err)?;

        if to.is_none() && from.is_none() {
            return Err(SaplingRemoteApiError::HttpError {
                status: StatusCode::BAD_REQUEST,
                message: "must specify one of 'to' or 'from'".to_string(),
                headers: Box::default(),
                url: self.url("set_bookmark"),
            });
        }

        if let Some(from) = from {
            match bms.get(&bookmark) {
                None => {
                    return Err(SaplingRemoteApiError::HttpError {
                        status: StatusCode::NOT_FOUND,
                        message: format!("bookmark {bookmark} doesn't exist"),
                        headers: Box::default(),
                        url: self.url("set_bookmark"),
                    });
                }
                Some(node) => {
                    if *node != from {
                        return Err(SaplingRemoteApiError::HttpError {
                            status: StatusCode::BAD_REQUEST,
                            message: format!(
                                "bookmark {bookmark}'s current value is {node}, not {from}"
                            ),
                            headers: Box::default(),
                            url: self.url("set_bookmark"),
                        });
                    }
                }
            }
        } else if bms.contains_key(&bookmark) {
            return Err(SaplingRemoteApiError::HttpError {
                status: StatusCode::BAD_REQUEST,
                message: format!("bookmark {bookmark} already exists"),
                headers: Box::default(),
                url: self.url("set_bookmark"),
            });
        }

        match to {
            None => bms.remove(&bookmark),
            Some(to) => bms.insert(bookmark, to),
        };

        // This validates that the bookmark value is a valid commit.
        self.set_bookmarks_map(bms).map_err(map_crate_err)?;

        self.flush_for_api("set_bookmark").await?;

        Ok(SetBookmarkResponse { data: Ok(()) })
    }

    async fn hash_prefixes_lookup(
        &self,
        prefixes: Vec<String>,
    ) -> Result<Vec<CommitHashLookupResponse>, SaplingRemoteApiError> {
        self.refresh_for_api()?;
        let dag = self.dag().await;
        prefixes
            .into_iter()
            .map(
                move |prefix| -> Result<CommitHashLookupResponse, SaplingRemoteApiError> {
                    let req = make_hash_lookup_request(prefix.clone())?;
                    let resp =
                        non_blocking_result(dag.vertexes_by_hex_prefix(prefix.as_bytes(), 100))
                            .map_err(|e| SaplingRemoteApiError::Other(e.into()));
                    resp.and_then(|vertexes| {
                        Ok(CommitHashLookupResponse {
                            request: req,
                            hgids: vertexes
                                .into_iter()
                                .map(|vertex| {
                                    HgId::from_hex(vertex.to_hex().as_bytes())
                                        .map_err(anyhow::Error::from)
                                })
                                .collect::<Result<Vec<_>, _>>()?,
                        })
                    })
                },
            )
            .collect()
    }

    async fn commit_mutations(
        &self,
        mut commits: Vec<HgId>,
    ) -> Result<Vec<CommitMutationsResponse>, SaplingRemoteApiError> {
        commits.sort();
        debug!("commit_mutations {}", debug_hgid_list(&commits));
        self.refresh_for_api()?;

        let mut seen_commits = HashSet::new();
        let mut mutations = Vec::new();
        let mut_store = self.mut_store.lock().await;

        // Max of 100 mutation depth.
        for _ in 0..100 {
            commits.retain(|c| seen_commits.insert(*c));

            let new_mutations: Vec<_> = mut_store
                .get_entries(&commits, &commits)
                .unwrap()
                .into_iter()
                .map(|e| CommitMutationsResponse {
                    mutation: local_mutation_to_edenapi(e),
                })
                .collect();
            if new_mutations.is_empty() {
                break;
            }

            for m in new_mutations.iter() {
                commits.push(m.mutation.successor);
                commits.extend_from_slice(&m.mutation.predecessors);
                commits.extend_from_slice(&m.mutation.split);
            }

            mutations.extend(new_mutations);
        }

        Ok(mutations)
    }

    async fn process_files_upload(
        &self,
        data: Vec<(AnyFileContentId, Bytes)>,
        bubble_id: Option<NonZeroU64>,
        copy_from_bubble_id: Option<NonZeroU64>,
        _lookup_policy: UploadLookupPolicy,
    ) -> Result<Response<UploadToken>, SaplingRemoteApiError> {
        debug!(?data, "process_files_upload");

        self.refresh_for_api()?;

        if bubble_id.is_some() || copy_from_bubble_id.is_some() {
            return Err(self.not_implemented_error(
                "EagerRepo does not support bubble_id".to_string(),
                "process_files_upload",
            ));
        }

        let mut res = Vec::with_capacity(data.len());
        for (id, data) in data {
            match self.add_sha1_blob_for_api(
                self.sha1_from_anyid(AnyId::AnyFileContentId(id), "process_files_upload")?,
                data,
                "process_files_upload",
            ) {
                Err(err) => res.push(Err(err)),
                Ok(()) => res.push(Ok(UploadToken {
                    data: UploadTokenData {
                        id: AnyId::AnyFileContentId(id),
                        bubble_id: None,
                        metadata: None,
                    },
                    signature: Default::default(),
                })),
            }
        }

        self.flush_for_api("process_files_upload").await?;

        Ok(convert_to_response(res))
    }

    async fn upload_filenodes_batch(
        &self,
        items: Vec<HgFilenodeData>,
    ) -> Result<Response<UploadTokensResponse>, SaplingRemoteApiError> {
        debug!(?items, "upload_filenodes_batch");

        self.refresh_for_api()?;

        let mut res = Vec::with_capacity(items.len());
        for data in items {
            let content_sha1 = self.sha1_from_anyid(
                data.file_content_upload_token.data.id,
                "upload_filesnodes_batch",
            )?;
            // NOTE: "raw_text" is pure content without hg/git SHA1 frames!
            let raw_text = self.get_sha1_blob_for_api(content_sha1, "upload_filenodes_batch")?;

            let mut content_with_parents =
                Vec::<u8>::with_capacity(raw_text.len() + 40 + 4 + data.metadata.len());
            let (mut p1, mut p2) = data.parents.into_nodes();
            if p2 < p1 {
                std::mem::swap(&mut p1, &mut p2);
            }
            content_with_parents.extend_from_slice(p1.as_ref());
            content_with_parents.extend_from_slice(p2.as_ref());

            // see sapling.filelog.filelog.add
            if raw_text.starts_with(b"\x01\n") || !data.metadata.is_empty() {
                content_with_parents.extend_from_slice(b"\x01\n");
                content_with_parents.extend(data.metadata);
                content_with_parents.extend_from_slice(b"\x01\n");
            }
            content_with_parents.extend_from_slice(raw_text.as_ref());

            self.add_sha1_blob_for_api(
                data.node_id,
                content_with_parents.into(),
                "upload_filenodes_batch",
            )?;

            res.push(Ok(UploadTokensResponse {
                token: UploadToken {
                    data: UploadTokenData {
                        id: AnyId::HgFilenodeId(data.node_id),
                        bubble_id: None,
                        metadata: None,
                    },
                    signature: Default::default(),
                },
            }));
        }

        self.flush_for_api("upload_filenodes_batch").await?;

        Ok(convert_to_response(res))
    }

    async fn upload_trees_batch(
        &self,
        items: Vec<UploadTreeEntry>,
    ) -> Result<Response<UploadTreeResponse>, SaplingRemoteApiError> {
        debug!(?items, "upload_trees_batch");

        self.refresh_for_api()?;

        let mut res = Vec::with_capacity(items.len());
        for tree in items {
            let mut content_with_parents = Vec::<u8>::with_capacity(tree.data.len() + 40);
            let (mut p1, mut p2) = tree.parents.into_nodes();
            if p2 < p1 {
                std::mem::swap(&mut p1, &mut p2);
            }
            content_with_parents.extend_from_slice(p1.as_ref());
            content_with_parents.extend_from_slice(p2.as_ref());
            content_with_parents.extend(tree.data);

            self.add_sha1_blob_for_api(
                tree.node_id,
                content_with_parents.into(),
                "upload_trees_batch",
            )?;

            res.push(Ok(UploadTreeResponse {
                token: UploadToken {
                    data: UploadTokenData {
                        id: AnyId::HgTreeId(tree.node_id),
                        bubble_id: None,
                        metadata: None,
                    },
                    signature: Default::default(),
                },
            }));
        }

        self.flush_for_api("upload_trees_batch").await?;

        Ok(convert_to_response(res))
    }

    async fn upload_changesets(
        &self,
        changesets: Vec<UploadHgChangeset>,
        mutations: Vec<HgMutationEntryContent>,
    ) -> Result<Response<UploadTokensResponse>, SaplingRemoteApiError> {
        debug!(?changesets, ?mutations, "upload_changesets");
        self.refresh_for_api()?;

        ::fail::fail_point!("eagerepo::api::uploadchangesets", |mode| {
            match mode.as_deref() {
                Some("error") => Err(SaplingRemoteApiError::HttpError {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: "failpoint".to_string(),
                    headers: Box::default(),
                    url: self.url("upload_changesets"),
                }),
                Some("empty") => Ok(convert_to_response(Vec::new())),
                _ => panic!("invalid failpoint return mode - specify 'error' or 'empty'"),
            }
        });

        let mut res = Vec::with_capacity(changesets.len());
        for UploadHgChangeset {
            node_id: node,
            changeset_content: cs,
        } in changesets
        {
            let mut parents = Vec::with_capacity(2);
            let (p1, p2) = cs.parents.into_nodes();
            if !p1.is_null() {
                parents.push(p1);
                if !p2.is_null() {
                    parents.push(p2);
                }
            }

            // Eagerly compute augmented manifest data.
            if let Err(err) = self.derive_augmented_tree_recursively(cs.manifestid) {
                error!(?err, "error pre-deriving augmented tree data");
            }

            let text = match changeset_to_text(cs) {
                Ok(text) => text,
                Err(err) => {
                    res.push(Err(err.context("creating changeset text").into()));
                    continue;
                }
            };

            match self.add_commit(&parents, &text).await {
                Ok(actual_id) => {
                    if actual_id != node {
                        res.push(Err(anyhow!("commit id mismatch").into()));
                    } else {
                        res.push(Ok(UploadTokensResponse {
                            token: UploadToken {
                                data: UploadTokenData {
                                    id: AnyId::HgChangesetId(node),
                                    bubble_id: None,
                                    metadata: None,
                                },
                                signature: Default::default(),
                            },
                        }));
                    }
                }
                Err(err) => {
                    // edenapi_upload.py has the expectation that errors are not
                    // propagated by the server. "failed" commits are simply not returned.
                    // I don't think that is good, but let's go with the flow for now.
                    error!(?err, "error inserting commit");
                    continue;
                }
            }
        }

        {
            let mut mut_store = self.mut_store.lock().await;
            for m in mutations {
                if let Err(err) = mut_store.add(&edenapi_mutation_to_local(m)) {
                    return Err(SaplingRemoteApiError::HttpError {
                        status: StatusCode::INTERNAL_SERVER_ERROR,
                        message: format!("error inserting mutation entry: {err:?}"),
                        headers: Box::default(),
                        url: self.url("upload_changesets"),
                    });
                }
            }
        }

        self.flush_for_api("upload_changesets").await?;

        Ok(convert_to_response(res))
    }

    async fn lookup_batch(
        &self,
        items: Vec<AnyId>,
        bubble_id: Option<NonZeroU64>,
        copy_from_bubble_id: Option<NonZeroU64>,
    ) -> Result<Vec<LookupResponse>, SaplingRemoteApiError> {
        debug!(?items, "lookup_batch");

        self.refresh_for_api()?;

        if bubble_id.is_some() || copy_from_bubble_id.is_some() {
            return Err(self.not_implemented_error(
                "EagerRepo does not support bubble_id".to_string(),
                "lookup_batch",
            ));
        }

        let mut res = Vec::with_capacity(items.len());
        for item in items {
            let sha1 = self.sha1_from_anyid(item, "lookup_batch")?;

            match self.get_sha1_blob(sha1) {
                Ok(None) => {
                    res.push(LookupResponse {
                        result: LookupResult::NotPresent(IndexableId {
                            id: item,
                            bubble_id: None,
                        }),
                    });
                }
                Ok(Some(_)) => {
                    res.push(LookupResponse {
                        result: LookupResult::Present(UploadToken {
                            data: UploadTokenData {
                                id: item,
                                bubble_id: None,
                                metadata: None,
                            },
                            signature: Default::default(),
                        }),
                    });
                }
                Err(e) => {
                    return Err(SaplingRemoteApiError::HttpError {
                        status: StatusCode::INTERNAL_SERVER_ERROR,
                        message: format!("{e:?}"),
                        headers: Box::default(),
                        url: self.url("lookup_batch"),
                    });
                }
            }
        }

        Ok(res)
    }

    async fn commit_translate_id(
        &self,
        commits: Vec<CommitId>,
        scheme: CommitIdScheme,
        from_repo: Option<String>,
        to_repo: Option<String>,
        _lookup_behavior: Option<String>,
    ) -> Result<Response<CommitTranslateIdResponse>, SaplingRemoteApiError> {
        debug!("files {commits:?} -> {scheme:?}");

        if std::env::var_os("TESTTMP").is_none() {
            return Err(SaplingRemoteApiError::NotSupported);
        }

        // Implement a dummy "Bonsai" translation for testing.

        if from_repo.is_some() || to_repo.is_some() {
            return Err(SaplingRemoteApiError::HttpError {
                status: StatusCode::BAD_REQUEST,
                message: "from_repo and to_repo not supported".to_string(),
                headers: Box::default(),
                url: self.url("commit_translate_id"),
            });
        }

        if !matches!(scheme, CommitIdScheme::Hg | CommitIdScheme::Bonsai) {
            return Err(SaplingRemoteApiError::HttpError {
                status: StatusCode::BAD_REQUEST,
                message: "only hg and bonsai supported".to_string(),
                headers: Box::default(),
                url: self.url("commit_translate_id"),
            });
        }

        let mut res = Vec::new();
        for commit in commits {
            let translated = match &commit {
                CommitId::Hg(hg) => {
                    if scheme == CommitIdScheme::Hg {
                        commit.clone()
                    } else {
                        let mut fake_bonsai = [0u8; 32];
                        fake_bonsai[..20].copy_from_slice(hg.as_ref());
                        CommitId::Bonsai(fake_bonsai.into())
                    }
                }
                CommitId::Bonsai(bz) => {
                    if scheme == CommitIdScheme::Hg {
                        let mut hg = [0u8; 20];
                        hg[..].copy_from_slice(&bz.as_ref()[..20]);
                        CommitId::Hg(hg.into())
                    } else {
                        commit.clone()
                    }
                }
                _ => {
                    return Err(SaplingRemoteApiError::HttpError {
                        status: StatusCode::BAD_REQUEST,
                        message: "only hg and bonsai supported".to_string(),
                        headers: Box::default(),
                        url: self.url("commit_translate_id"),
                    });
                }
            };

            res.push(Ok(CommitTranslateIdResponse { commit, translated }));
        }

        Ok(convert_to_response(res))
    }

    async fn suffix_query(
        &self,
        commit: CommitId,
        suffixes: Vec<String>,
        prefix: Option<Vec<String>>,
    ) -> Result<Response<SuffixQueryResponse>, SaplingRemoteApiError> {
        debug!("suffix_query");
        // TODO(T189729875) Make this react to committed files
        //let files = self.files();
        let _ = (commit, prefix);
        let mut res = vec![];
        for suffix in suffixes {
            match suffix.clone().as_str() {
                ".cpp" => {
                    let from_string = RepoPathBuf::from_string("ranier.cpp".to_string());
                    let file_path = from_string.unwrap();
                    res.push(Ok(SuffixQueryResponse { file_path }));
                    let from_string = RepoPathBuf::from_string("fuji/peak.cpp".to_string());
                    let file_path = from_string.unwrap();
                    res.push(Ok(SuffixQueryResponse { file_path }));
                }
                ".txt" => {
                    let from_string = RepoPathBuf::from_string("foo.txt".to_string());
                    let file_path = from_string.unwrap();
                    res.push(Ok(SuffixQueryResponse { file_path }));
                    let from_string = RepoPathBuf::from_string("baz.txt".to_string());
                    let file_path = from_string.unwrap();
                    res.push(Ok(SuffixQueryResponse { file_path }));
                }
                ".rs" => {
                    let from_string = RepoPathBuf::from_string("bar.rs".to_string());
                    let file_path = from_string.unwrap();
                    res.push(Ok(SuffixQueryResponse { file_path }));
                }
                _ => {}
            }
        }
        Ok(convert_to_response(res))
    }

    async fn check_path_permission(
        &self,
        request: CheckPathPermissionRequest,
    ) -> edenapi::Result<Response<CheckPathPermissionResponse>> {
        debug!("check_path_permission {:?}", &request.paths);
        self.refresh_for_api()?;

        let manifest = self.commit_to_manifest(request.hg_cs_id).await?;
        let values = request
            .paths
            .into_iter()
            .map(|path| {
                let mut restriction_entries = Vec::new();
                let ancestors = path.ancestors().collect::<Vec<_>>();
                for ancestor in ancestors.into_iter().rev() {
                    if ancestor.is_empty() {
                        continue;
                    }
                    let has_slacl = match manifest.list(ancestor)? {
                        List::Directory(children) => children.iter().any(|(name, metadata)| {
                            name.as_str() == ".slacl" && matches!(metadata, FsNodeMetadata::File(_))
                        }),
                        List::File | List::NotFound => false,
                    };
                    if has_slacl {
                        restriction_entries.push(CheckPathPermissionAclEntry {
                            restriction_root: ancestor.to_owned(),
                            repo_region_acl: crate::eager_repo::EAGER_PLACEHOLDER_ACL.to_string(),
                            permission_request_group: crate::eager_repo::EAGER_PLACEHOLDER_ACL
                                .to_string(),
                        });
                    }
                }
                let has_access = restriction_entries.is_empty();
                Ok(CheckPathPermissionResponse::from_result(
                    path,
                    Ok(CheckPathPermissionData {
                        has_access,
                        restriction_entries,
                    }),
                ))
            })
            .collect::<Vec<_>>();

        Ok(convert_to_response(values))
    }

    async fn check_manifest_permission(
        &self,
        request: CheckManifestPermissionRequest,
    ) -> edenapi::Result<Response<CheckManifestPermissionResponse>> {
        debug!(
            "check_manifest_permission {}",
            debug_hgid_list(&request.manifest_ids)
        );
        self.refresh_for_api()?;

        let mut values = Vec::new();
        for manifest_id in request.manifest_ids {
            let has_slacl = self.tree_has_slacl(manifest_id)?;
            let (request_acl, denial_message) = if has_slacl {
                (
                    Some(crate::eager_repo::EAGER_PLACEHOLDER_ACL.to_string()),
                    self.server_denial_message(),
                )
            } else {
                (None, None)
            };

            values.push(Ok(CheckManifestPermissionResponse {
                manifest_id,
                has_access: !has_slacl,
                request_acl,
                denial_message,
            }));
        }

        Ok(convert_to_response(values))
    }

    async fn land_stack(
        &self,
        bookmark: String,
        head: HgId,
        base: HgId,
        pushvars: HashMap<String, String>,
    ) -> Result<LandStackResponse, SaplingRemoteApiError> {
        let mainline_parent_first = pushvars
            .get("MAINLINE_PARENT_FIRST")
            .is_some_and(|v| matches!(v.as_str(), "true" | "1"));
        let latest_bookmark_id = match self.get_bookmark(&bookmark) {
            Ok(Some(id)) => id,
            _ => {
                return Err(SaplingRemoteApiError::HttpError {
                    status: StatusCode::NOT_FOUND,
                    message: format!("bookmark {bookmark} was not found"),
                    headers: Box::default(),
                    url: self.url("land_stack"),
                });
            }
        };

        let commits = match commits_to_rebase(
            self,
            &bookmark,
            head,
            base,
            latest_bookmark_id,
            mainline_parent_first,
        )
        .await?
        {
            Ok(commits) => commits,
            Err(e) => return Ok(LandStackResponse { data: Err(e) }),
        };

        if latest_bookmark_id == base && !mainline_parent_first {
            // no changes on server side, just move the bookmark
            EagerRepo::set_bookmark(self, &bookmark, Some(head)).unwrap();
            let old_to_new_hgids = commits.into_iter().map(|c| (c, c)).collect();
            let data = LandStackData {
                new_head: head,
                old_to_new_hgids,
            };
            self.flush_for_api("land_stack").await?;
            return Ok(LandStackResponse { data: Ok(data) });
        }

        let head_manifest = self.commit_to_manifest(head).await.map_err(map_crate_err)?;
        let base_manifest = self.commit_to_manifest(base).await.map_err(map_crate_err)?;
        let bookmark_manifest = self
            .commit_to_manifest(latest_bookmark_id)
            .await
            .map_err(map_crate_err)?;

        let conflicts = pushrebase_conflicts(&base_manifest, &bookmark_manifest, &head_manifest)?;
        if !conflicts.is_empty() {
            let e = ServerError::generic(format!("Conflicts while pushrebasing: {conflicts:?}"));

            return Ok(LandStackResponse { data: Err(e) });
        }

        // Each commit is rebased onto the new version of its first parent
        // that is `base` or in `commits`. Its other parents are kept: they
        // are merged-in commits that land unchanged. With
        // `mainline_parent_first`, that parent, the mainline parent, becomes
        // the first parent, as in Mononoke.
        let mut new_ids = HashMap::from([(base, latest_bookmark_id)]);
        let mut old_to_new_hgids = HashMap::new();
        for commit in commits {
            let parents = parent_hgids(self, commit).await?;
            let base_index = parents
                .iter()
                .position(|p| new_ids.contains_key(p))
                .ok_or_else(|| {
                    SaplingRemoteApiError::Other(anyhow!(
                        "commit {commit} does not descend from {base}"
                    ))
                })?;
            let base_commit = parents[base_index];
            let mut new_parents: Vec<HgId> = parents
                .iter()
                .map(|p| new_ids.get(p).copied().unwrap_or(*p))
                .collect();
            let swapped = mainline_parent_first && base_index > 0;
            if swapped {
                new_parents[..=base_index].rotate_right(1);
            }
            let new_commit = pushrebase_one(
                self,
                base_commit,
                commit,
                new_ids[&base_commit],
                &new_parents,
                swapped,
            )
            .await?;
            new_ids.insert(commit, new_commit);
            old_to_new_hgids.insert(commit, new_commit);
        }

        let new_head = old_to_new_hgids[&head];
        EagerRepo::set_bookmark(self, &bookmark, Some(new_head)).map_err(map_crate_err)?;
        let data = LandStackData {
            new_head,
            old_to_new_hgids,
        };
        self.flush_for_api("land_stack").await?;
        return Ok(LandStackResponse { data: Ok(data) });

        /// Returns the commits that land_stack rebases from `base` onto the
        /// bookmark at `onto`, parents first, after the checks Mononoke makes:
        /// `base` must be the only merge base of `onto` and `head`, and none
        /// of the commits may be public (an ancestor of a bookmark). Unless
        /// `mainline_parent_first`, `base` must not be a second parent.
        async fn commits_to_rebase(
            repo: &EagerRepo,
            bookmark: &str,
            head: HgId,
            base: HgId,
            onto: HgId,
            mainline_parent_first: bool,
        ) -> Result<Result<Vec<HgId>, ServerError>, SaplingRemoteApiError> {
            let dag = repo.dag().await;
            let merge_bases = dag
                .gca_all(to_set(&[onto, head]))
                .await
                .map_err(map_dag_err)?;
            let merge_bases = to_hgids(merge_bases).await?;
            if merge_bases.is_empty() {
                return Ok(Err(ServerError::generic(format!(
                    "cannot land {head} onto '{bookmark}': they have no common ancestor"
                ))));
            }
            if merge_bases != [base] {
                let reason = match merge_bases.as_slice() {
                    [merge_base] => format!(
                        "the merge base of '{bookmark}' and {head} is {merge_base}, but the push is based on {base}"
                    ),
                    _ => format!("'{bookmark}' and {head} have more than one merge base"),
                };
                return Ok(Err(ServerError::generic(format!(
                    "bookmark '{bookmark}' has moved incompatibly with this push: {reason}. Pull and retry"
                ))));
            }

            let commits = dag
                .range(to_set(&[base]), to_set(&[head]))
                .await
                .map_err(map_dag_err)?
                - to_set(&[base]);
            let bookmarked: Vec<HgId> = repo
                .get_bookmarks_map()
                .map_err(map_crate_err)?
                .into_values()
                .collect();
            let public = commits.clone()
                & dag
                    .ancestors(to_set(&bookmarked))
                    .await
                    .map_err(map_dag_err)?;
            let public = to_hgids(public).await?;
            if !public.is_empty() {
                let public: Vec<String> = public.iter().map(|id| id.to_hex()).collect();
                return Ok(Err(ServerError::generic(format!(
                    "cannot rebase public commits: {}",
                    public.join(", ")
                ))));
            }

            let commits = to_hgids(commits).await?;
            if commits.is_empty() {
                return Ok(Err(ServerError::generic(format!(
                    "nothing to land: {head} is already an ancestor of '{bookmark}'"
                ))));
            }
            // As with Mononoke's default `forbid_p2_root_rebases`.
            let base_vertex = Vertex::copy_from(base.as_ref());
            for commit in &commits {
                let parents = dag
                    .parent_names(Vertex::copy_from(commit.as_ref()))
                    .await
                    .map_err(map_dag_err)?;
                if !mainline_parent_first && parents[1..].contains(&base_vertex) {
                    return Ok(Err(ServerError::generic(format!(
                        "Forbid pushrebase because root ({base}) is not a p1 of {bookmark} bookmark"
                    ))));
                }
            }
            Ok(Ok(commits))
        }

        async fn parent_hgids(
            repo: &EagerRepo,
            commit: HgId,
        ) -> Result<Vec<HgId>, SaplingRemoteApiError> {
            let parents = repo
                .dag()
                .await
                .parent_names(Vertex::copy_from(commit.as_ref()))
                .await
                .map_err(map_dag_err)?;
            Ok(parents
                .into_iter()
                .map(|v| HgId::from_slice(v.as_ref()).unwrap())
                .collect())
        }

        /// Creates a commit with `new_parents` that applies the changes from
        /// `base_commit` to `source_commit` on top of `dest_commit`. In the Hg
        /// format, `parents_swapped` marks it with the `parents_swapped`
        /// extra, as Mononoke's pushrebase does.
        async fn pushrebase_one(
            repo: &EagerRepo,
            base_commit: HgId,
            source_commit: HgId,
            dest_commit: HgId,
            new_parents: &[HgId],
            parents_swapped: bool,
        ) -> anyhow::Result<HgId> {
            let base_manifest = repo.commit_to_manifest(base_commit).await?;
            let source_manifest = repo.commit_to_manifest(source_commit).await?;
            let mut new_manifest = repo.commit_to_manifest(dest_commit).await?;

            let matcher = AlwaysMatcher::new();

            // generate new manifest
            for e in base_manifest.diff(&source_manifest, matcher)?.into_iter() {
                let e = e?;
                match e.diff_type {
                    DiffType::LeftOnly(_) => {
                        new_manifest.remove(&e.path)?;
                    }
                    DiffType::Changed(_, right) => {
                        new_manifest.insert(e.path, right)?;
                    }
                    DiffType::RightOnly(right) => {
                        new_manifest.insert(e.path.clone(), right)?;
                    }
                }
            }

            let new_tree_id = match repo.store.format() {
                SerializationFormat::Hg => {
                    let mut parent_manifests = Vec::with_capacity(new_parents.len());
                    for parent in new_parents {
                        parent_manifests.push(repo.commit_to_manifest(*parent).await?);
                    }
                    let parent_manifests: Vec<&TreeManifest> = parent_manifests.iter().collect();
                    new_manifest.persist(&parent_manifests)?
                }
                SerializationFormat::Git => {
                    Manifest::persist(&mut new_manifest, PersistOpts { parents: &[] })?
                }
            };

            // generate new commit
            let old_raw_text = match repo.store.get_content(source_commit)? {
                None => {
                    return Err(anyhow!(
                        "commit content cannot be found: {}",
                        source_commit.to_hex()
                    ));
                }
                Some(raw_text) => raw_text,
            };
            let new_raw_text: Vec<u8> = match repo.store.format() {
                SerializationFormat::Hg if parents_swapped => {
                    let old_fields = HgCommitLazyFields::new(old_raw_text.into_text_lossy());
                    let old_fields = old_fields.fields()?;
                    let mut extras = old_fields.extras.clone();
                    if extras
                        .insert(Text::from_static("parents_swapped"), Text::from_static("1"))
                        .is_some()
                    {
                        return Err(anyhow!(
                            "commit {source_commit} already has the parents_swapped extra"
                        ));
                    }
                    HgCommitFields {
                        tree: new_tree_id,
                        author: old_fields.author.clone(),
                        date: old_fields.date,
                        extras,
                        files: old_fields.files.clone(),
                        message: old_fields.message.clone(),
                    }
                    .to_text()?
                    .into_bytes()
                }
                _ => {
                    let mut new_raw_text = Vec::new();
                    write!(new_raw_text, "{new_tree_id}")?;
                    new_raw_text.extend_from_slice(&old_raw_text[HgId::hex_len()..]);
                    new_raw_text
                }
            };

            let new_commit = repo.add_commit(new_parents, &new_raw_text).await?;

            Ok(new_commit)
        }

        /// `left` and `right` are considerered to be conflict free, if none of the element
        /// from `left` is prefix of element from `right`, and vice versa.
        fn pushrebase_conflicts(
            mbase: &TreeManifest,
            mleft: &TreeManifest,
            mright: &TreeManifest,
        ) -> anyhow::Result<Vec<(RepoPathBuf, RepoPathBuf)>> {
            let matcher = AlwaysMatcher::new();
            let mut left = mbase
                .diff(mleft, matcher.clone())?
                .into_iter()
                .map(|e| e.map(|e| e.path))
                .collect::<anyhow::Result<Vec<_>>>()?;
            left.sort_unstable();
            let mut left_iter = left.into_iter();

            let mut right = mbase
                .diff(mright, matcher)?
                .into_iter()
                .map(|e| e.map(|e| e.path))
                .collect::<anyhow::Result<Vec<_>>>()?;
            right.sort_unstable();
            let mut right_iter = right.into_iter();

            let mut conflicts = Vec::new();
            let mut state = (left_iter.next(), right_iter.next());
            let is_case_sensitive = true;
            loop {
                state = match state {
                    (Some(l), Some(r)) => match l.cmp(&r) {
                        Ordering::Equal => {
                            conflicts.push((l.clone(), r.clone()));
                            (left_iter.next(), right_iter.next())
                        }
                        Ordering::Less => {
                            if r.starts_with(&l, is_case_sensitive) {
                                conflicts.push((l.clone(), r.clone()));
                            }
                            (left_iter.next(), Some(r))
                        }
                        Ordering::Greater => {
                            if l.starts_with(&r, is_case_sensitive) {
                                conflicts.push((l.clone(), r.clone()));
                            }
                            (Some(l), right_iter.next())
                        }
                    },
                    _ => break,
                }
            }

            Ok(conflicts)
        }
    }
}

fn sha1_blob_to_parents_body(
    data: &Bytes,
    format: SerializationFormat,
) -> anyhow::Result<(Parents, Bytes)> {
    let (parents, body) = match format {
        SerializationFormat::Hg => {
            let (body, p2, p1) = hg_sha1_deserialize(data)?;
            (Parents::new(p1, p2), data.slice_to_bytes(body))
        }
        SerializationFormat::Git => {
            let body = git_sha1_deserialize(data)?.0;
            (Parents::default(), data.slice_to_bytes(body))
        }
    };
    Ok((parents, body))
}

fn file_body_to_file_content_and_copy_from(
    body: &Bytes,
    format: SerializationFormat,
) -> (Bytes, Bytes) {
    match format {
        SerializationFormat::Hg => format_util::split_hg_file_metadata(body),
        SerializationFormat::Git => (body.clone(), Bytes::new()),
    }
}

fn edenapi_mutation_to_local(m: HgMutationEntryContent) -> MutationEntry {
    MutationEntry {
        succ: m.successor,
        preds: m.predecessors,
        split: m.split,
        op: m.op,
        user: String::from_utf8_lossy(&m.user).to_string(),
        time: m.time,
        tz: m.tz,
        extra: m
            .extras
            .into_iter()
            .map(|e| (e.key.into_boxed_slice(), e.value.into_boxed_slice()))
            .collect(),
    }
}

fn local_mutation_to_edenapi(m: MutationEntry) -> HgMutationEntryContent {
    HgMutationEntryContent {
        successor: m.succ,
        predecessors: m.preds,
        split: m.split,
        op: m.op,
        user: m.user.into_bytes(),
        time: m.time,
        tz: m.tz,
        extras: m
            .extra
            .into_iter()
            .map(|(k, v)| Extra {
                key: k.to_vec(),
                value: v.to_vec(),
            })
            .collect(),
    }
}

fn changeset_to_text(mut cs: HgChangesetContent) -> anyhow::Result<Vec<u8>> {
    // TODO: validate stuff better
    let mut text = Vec::<u8>::new();

    writeln!(text, "{}", cs.manifestid)?;

    writeln!(text, "{}", String::from_utf8(cs.user)?)?;

    write!(text, "{} {}", cs.time, cs.tz)?;

    if !cs.extras.is_empty() {
        write!(text, " ")?;

        let mut extras: Vec<(String, String)> = Vec::with_capacity(cs.extras.len());
        for extra in cs.extras {
            extras.push((
                String::from_utf8(extra.key)?,
                String::from_utf8(extra.value)?,
            ));
        }
        extras.sort_by(|a, b| a.0.cmp(&b.0));
        for (idx, (k, v)) in extras.into_iter().enumerate() {
            let extra = format!("{k}:{v}")
                .replace('\\', r"\\")
                .replace('\n', r"\n")
                .replace('\r', r"\r")
                .replace('\0', r"\0");
            if idx > 0 {
                text.push(0);
            }
            write!(text, "{extra}")?;
        }
    }

    text.push(b'\n');

    cs.files.sort();
    for file in cs.files {
        writeln!(text, "{file}")?;
    }

    text.push(b'\n');

    text.extend_from_slice(&cs.message);

    Ok(text)
}

impl EagerRepo {
    fn get_sha1_blob_for_api(&self, id: HgId, handler: &str) -> edenapi::Result<minibytes::Bytes> {
        // Emulate the HTTP errors.
        match self.opt_sha1_blob_for_api(id, handler)? {
            None => Err(SaplingRemoteApiError::HttpError {
                status: StatusCode::NOT_FOUND,
                message: format!("{} cannot be found", id.to_hex()),
                headers: Box::default(),
                url: self.url(handler),
            }),
            Some(data) => Ok(data),
        }
    }

    fn opt_sha1_blob_for_api(
        &self,
        id: HgId,
        handler: &str,
    ) -> edenapi::Result<Option<minibytes::Bytes>> {
        // Emulate the HTTP errors.
        match self.get_sha1_blob(id) {
            Ok(None) => {
                trace!(" not found: {}", id.to_hex());
                Ok(None)
            }
            Ok(Some(data)) => {
                trace!(" found: {}, {} bytes", id.to_hex(), data.len());
                Ok(Some(data))
            }
            Err(e) => Err(SaplingRemoteApiError::HttpError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("{e:?}"),
                headers: Box::default(),
                url: self.url(handler),
            }),
        }
    }

    fn add_sha1_blob_for_api(
        &self,
        id: HgId,
        blob: minibytes::Bytes,
        handler: &str,
    ) -> edenapi::Result<()> {
        let actual_id = match self.add_sha1_blob(blob.as_ref()) {
            Ok(actual_id) => actual_id,
            Err(e) => {
                return Err(SaplingRemoteApiError::HttpError {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: format!("{e:?}"),
                    headers: Box::default(),
                    url: self.url(handler),
                });
            }
        };
        if id != actual_id {
            return Err(SaplingRemoteApiError::HttpError {
                status: StatusCode::BAD_REQUEST,
                message: "content hash mismatch".to_string(),
                headers: Box::default(),
                url: self.url(handler),
            });
        }

        Ok(())
    }

    fn opt_augmented_tree_blob_with_digest_for_api(
        &self,
        id: HgId,
        handler: &str,
    ) -> edenapi::Result<Option<minibytes::Bytes>> {
        // Emulate the HTTP errors.
        match self.derive_augmented_tree_recursively(id) {
            Ok(None) => {
                trace!(" not found: {}", id.to_hex());
                Ok(None)
            }
            Ok(Some(data)) => {
                trace!(" found: {}, {} bytes", id.to_hex(), data.len());
                Ok(Some(data))
            }
            Err(e) => Err(SaplingRemoteApiError::HttpError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("{e:?}"),
                headers: Box::default(),
                url: self.url(handler),
            }),
        }
    }

    async fn get_augmented_tree_blob_with_digest_for_api(
        &self,
        id: HgId,
        handler: &str,
    ) -> edenapi::Result<minibytes::Bytes> {
        // Emulate the HTTP errors.
        match self.opt_augmented_tree_blob_with_digest_for_api(id, handler)? {
            None => Err(SaplingRemoteApiError::HttpError {
                status: StatusCode::NOT_FOUND,
                message: format!("{} cannot be found", id.to_hex()),
                headers: Box::default(),
                url: self.url(handler),
            }),
            Some(data) => Ok(data),
        }
    }

    async fn flush_for_api(&self, handler: &str) -> edenapi::Result<()> {
        self.flush()
            .await
            .map_err(|err| SaplingRemoteApiError::HttpError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("error flushing dag/store: {err:?}"),
                headers: Box::default(),
                url: self.url(handler),
            })
    }

    /// Not implement error.
    fn not_implemented_error(&self, message: String, handler: &str) -> SaplingRemoteApiError {
        SaplingRemoteApiError::HttpError {
            status: StatusCode::NOT_IMPLEMENTED,
            message,
            headers: Box::default(),
            url: self.url(handler),
        }
    }

    /// Provide the URL for HTTP error reporting.
    fn url(&self, handler: &str) -> String {
        format!("eager://{}/{}", self.dir.display(), handler)
    }

    fn sha1_from_anyid(&self, id: AnyId, handler: &str) -> edenapi::Result<HgId> {
        match id {
            AnyId::HgFilenodeId(hgid) => Ok(hgid),
            AnyId::HgTreeId(hgid) => Ok(hgid),
            AnyId::HgChangesetId(hgid) => Ok(hgid),
            AnyId::AnyFileContentId(AnyFileContentId::Sha1(id)) => {
                Ok(HgId::from_byte_array(id.into_byte_array()))
            }
            _ => Err(self.not_implemented_error(
                format!("id type {id:?} not supported by EagerRepo"),
                handler,
            )),
        }
    }
}

/// Optionally build `SaplingRemoteApi` from config.
///
/// If the config does not specify eagerepo-based `SaplingRemoteApi`, return `Ok(None)`.
pub fn edenapi_from_config(
    config: &dyn Config,
) -> edenapi::Result<Option<Arc<dyn SaplingRemoteApi>>> {
    for (section, name) in [("paths", "default"), ("edenapi", "url")] {
        if let Ok(url) = config.must_get::<RepoUrl>(section, name) {
            trace!(
                target: "eagerepo::edenapi_from_config",
                "attempt to create EagerRepo as SaplingRemoteApi from config {section}.{name}={url}",
            );
            if let Some(path) = EagerRepo::url_to_dir(&url) {
                let repo = EagerRepo::open(&path).map_err(edenapi::SaplingRemoteApiError::Other)?;
                let enforce_server_acls = config
                    .get_or_default::<bool>("slacl", "server-acl-enforcement")
                    .map_err(|err| edenapi::SaplingRemoteApiError::Other(err.into()))?;
                repo.set_enforce_server_acls(enforce_server_acls);
                let server_denial_message = config
                    .get_nonempty_opt::<String>("slacl", "server-denial-message")
                    .map_err(|err| edenapi::SaplingRemoteApiError::Other(err.into()))?;
                repo.set_server_denial_message(server_denial_message);
                return Ok(Some(Arc::new(repo)));
            }
        }
    }
    Ok(None)
}

fn default_response_meta() -> ResponseMeta {
    ResponseMeta {
        version: Version::HTTP_11,
        status: StatusCode::OK,
        server: Some("EagerRepo".to_string()),
        ..Default::default()
    }
}

/// Extract rename metadata from filelog header (if rename exists).
/// data is not prefixed by hashes.
///
/// See `filelog.py:parsemeta`.
fn extract_rename(data: &[u8]) -> Option<Key> {
    if data.starts_with(b"\x01\n") {
        let data = &data[2..];
        if let Some(pos) = data.windows(2).position(|needle| needle == b"\x01\n") {
            let header = String::from_utf8_lossy(&data[..pos]);
            let mut path = None;
            let mut rev = None;
            for line in header.lines() {
                let kv: Vec<&str> = line.split(": ").collect();
                if let [k, v] = &kv[..] {
                    if *k == "copy" {
                        path = RepoPathBuf::from_string(v.to_string()).ok();
                    } else if *k == "copyrev" {
                        rev = HgId::from_hex(v.as_bytes()).ok();
                    }
                }
            }
            if let (Some(path), Some(rev)) = (path, rev) {
                return Some(Key { path, hgid: rev });
            }
        }
    }
    None
}

/// Convert `Vec<T>` to `Response<T>`.
fn convert_to_response<T: Send + Sync + 'static>(values: Vec<edenapi::Result<T>>) -> Response<T> {
    Response {
        stats: Box::pin(async { Ok(Default::default()) }),
        entries: Box::pin(futures::stream::iter(values)),
    }
}

fn check_convert_to_hgid<'a>(vertexes: impl Iterator<Item = &'a Vertex>) -> edenapi::Result<()> {
    for v in vertexes {
        let _ = HgId::from_slice(v.as_ref()).map_err(|e| SaplingRemoteApiError::Other(e.into()))?;
    }
    Ok(())
}

fn to_vec_vertex(ids: &[HgId]) -> Vec<Vertex> {
    ids.iter().map(|i| Vertex::copy_from(i.as_ref())).collect()
}

fn to_set(ids: &[HgId]) -> Set {
    let vertexes = to_vec_vertex(ids);
    Set::from_static_names(vertexes)
}

async fn to_hgids(set: Set) -> edenapi::Result<Vec<HgId>> {
    set.iter_rev()
        .await
        .map_err(map_dag_err)?
        .map_ok(|v| HgId::from_slice(v.as_ref()).unwrap())
        .try_collect()
        .await
        .map_err(map_dag_err)
}

fn map_dag_err(e: dag::Error) -> SaplingRemoteApiError {
    SaplingRemoteApiError::Other(e.into())
}

fn map_crate_err(e: crate::Error) -> SaplingRemoteApiError {
    SaplingRemoteApiError::Other(e)
}

fn debug_key_list(keys: &[Key]) -> String {
    debug_list(keys, |k| k.hgid.to_hex())
}

fn debug_spec_list(reqs: &[FileSpec]) -> String {
    debug_list(reqs, |s| format!("{s:?}"))
}

fn debug_hgid_list(ids: &[HgId]) -> String {
    debug_list(ids, |i| i.to_hex())
}

fn debug_string_list(s: &[String]) -> String {
    debug_list(s, |s| s.clone())
}

fn debug_list<T>(keys: &[T], func: impl Fn(&T) -> String) -> String {
    let limit = 5;
    let msg = keys
        .iter()
        .take(limit)
        .map(func)
        .collect::<Vec<_>>()
        .join(", ");
    if keys.len() > limit {
        format!("{} and {} more", msg, keys.len() - limit)
    } else {
        msg
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use edenapi::SaplingRemoteApi;
    use edenapi::types::AnyFileContentId;
    use edenapi::types::AnyId;
    use edenapi::types::Extra;
    use edenapi::types::HgChangesetContent;
    use edenapi::types::HgFilenodeData;
    use edenapi::types::HgId;
    use edenapi::types::Parents;
    use edenapi::types::RepoPathBuf;
    use edenapi::types::UploadToken;
    use edenapi::types::UploadTokenData;
    use minibytes::Bytes;
    use sha1::Digest;
    use storemodel::SerializationFormat;

    use super::*;

    fn compute_sha1(data: &[u8]) -> HgId {
        let mut hasher = sha1::Sha1::new();
        hasher.update(data);
        let hash = hasher.finalize();
        HgId::from_slice(&hash).unwrap()
    }

    fn hgid_to_content_sha1(id: HgId) -> AnyFileContentId {
        AnyFileContentId::Sha1(edenapi_types::Sha1::from_byte_array(id.into_byte_array()))
    }

    // Invariant: data without the '\x01\n' prefix is never a rename
    #[test]
    fn test_extract_rename_no_prefix() {
        assert_eq!(extract_rename(b"plain file content"), None);
        assert_eq!(extract_rename(b""), None);
        assert_eq!(extract_rename(b"\x01"), None);
    }

    // Invariant: '\x01\n' present but no closing '\x01\n' means malformed header, returns None
    #[test]
    fn test_extract_rename_no_closing_marker() {
        let data = b"\x01\ncopy: foo\ncopyrev: aabbccdd";
        assert_eq!(extract_rename(data), None);
    }

    // Invariant: header with 'copy:' but missing 'copyrev:' cannot construct a full Key
    #[test]
    fn test_extract_rename_copy_without_copyrev() {
        let data = b"\x01\ncopy: foo/bar.txt\n\x01\nfile content";
        assert_eq!(extract_rename(data), None);
    }

    // Invariant: both copy and copyrev present produce a valid Key with correct path and hgid
    #[test]
    fn test_extract_rename_valid() {
        let hex = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let data = format!("\x01\ncopy: foo/bar.txt\ncopyrev: {hex}\n\x01\nfile content");
        let result = extract_rename(data.as_bytes()).unwrap();
        assert_eq!(result.path.as_str(), "foo/bar.txt");
        assert_eq!(result.hgid, HgId::from_hex(hex.as_bytes()).unwrap());
    }

    // Invariant: parser handles multiple header lines and only extracts copy/copyrev
    #[test]
    fn test_extract_rename_multiple_header_lines() {
        let hex = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let data = format!(
            "\x01\nother: ignored\ncopy: a/b.txt\nextra: stuff\ncopyrev: {hex}\n\x01\nbody"
        );
        let result = extract_rename(data.as_bytes()).unwrap();
        assert_eq!(result.path.as_str(), "a/b.txt");
        assert_eq!(result.hgid, HgId::from_hex(hex.as_bytes()).unwrap());
    }

    // Invariant: extras are sorted by key alphabetically before encoding
    #[test]
    fn test_changeset_to_text_extras_sorted() {
        let cs = HgChangesetContent {
            parents: Parents::default(),
            manifestid: *HgId::null_id(),
            user: b"user".to_vec(),
            time: 1000,
            tz: 0,
            extras: vec![
                Extra {
                    key: b"zebra".to_vec(),
                    value: b"z".to_vec(),
                },
                Extra {
                    key: b"alpha".to_vec(),
                    value: b"a".to_vec(),
                },
                Extra {
                    key: b"middle".to_vec(),
                    value: b"m".to_vec(),
                },
            ],
            files: Vec::new(),
            message: b"msg".to_vec(),
        };
        let text = changeset_to_text(cs).unwrap();
        let text_str = String::from_utf8_lossy(&text);
        let time_line = text_str.lines().nth(2).unwrap();
        assert!(time_line.contains("alpha:a"));
        assert!(time_line.contains("middle:m"));
        assert!(time_line.contains("zebra:z"));
        let alpha_pos = time_line.find("alpha:a").unwrap();
        let middle_pos = time_line.find("middle:m").unwrap();
        let zebra_pos = time_line.find("zebra:z").unwrap();
        assert!(alpha_pos < middle_pos);
        assert!(middle_pos < zebra_pos);
    }

    // Invariant: special characters in extras are escaped per Mercurial convention
    #[test]
    fn test_changeset_to_text_extras_escaped() {
        let cs = HgChangesetContent {
            parents: Parents::default(),
            manifestid: *HgId::null_id(),
            user: b"user".to_vec(),
            time: 0,
            tz: 0,
            extras: vec![Extra {
                key: b"key".to_vec(),
                value: b"back\\slash\nnewline\rcarriage\0nul".to_vec(),
            }],
            files: Vec::new(),
            message: b"msg".to_vec(),
        };
        let text = changeset_to_text(cs).unwrap();
        let text_str = String::from_utf8_lossy(&text);
        let time_line = text_str.lines().nth(2).unwrap();
        assert!(time_line.contains(r"key:back\\slash\nnewline\rcarriage\0nul"));
    }

    // Invariant: extras entries are separated by NUL byte in the output
    #[test]
    fn test_changeset_to_text_extras_nul_separated() {
        let cs = HgChangesetContent {
            parents: Parents::default(),
            manifestid: *HgId::null_id(),
            user: b"user".to_vec(),
            time: 0,
            tz: 0,
            extras: vec![
                Extra {
                    key: b"a".to_vec(),
                    value: b"1".to_vec(),
                },
                Extra {
                    key: b"b".to_vec(),
                    value: b"2".to_vec(),
                },
            ],
            files: Vec::new(),
            message: b"msg".to_vec(),
        };
        let text = changeset_to_text(cs).unwrap();
        let time_line_start = text.windows(3).position(|w| w == b"0 0").unwrap();
        let newline_after = text[time_line_start..]
            .iter()
            .position(|&b| b == b'\n')
            .unwrap();
        let extras_region = &text[time_line_start..time_line_start + newline_after];
        assert!(extras_region.contains(&0u8));
    }

    // Invariant: files list is sorted alphabetically in the output
    #[test]
    fn test_changeset_to_text_files_sorted() {
        let cs = HgChangesetContent {
            parents: Parents::default(),
            manifestid: *HgId::null_id(),
            user: b"user".to_vec(),
            time: 0,
            tz: 0,
            extras: Vec::new(),
            files: vec![
                RepoPathBuf::from_string("z_file".to_string()).unwrap(),
                RepoPathBuf::from_string("a_file".to_string()).unwrap(),
                RepoPathBuf::from_string("m_file".to_string()).unwrap(),
            ],
            message: b"msg".to_vec(),
        };
        let text = changeset_to_text(cs).unwrap();
        let text_str = String::from_utf8(text).unwrap();
        let a_pos = text_str.find("a_file").unwrap();
        let m_pos = text_str.find("m_file").unwrap();
        let z_pos = text_str.find("z_file").unwrap();
        assert!(a_pos < m_pos);
        assert!(m_pos < z_pos);
    }

    // Invariant: empty extras produces no extras segment on the time line
    #[test]
    fn test_changeset_to_text_no_extras() {
        let cs = HgChangesetContent {
            parents: Parents::default(),
            manifestid: *HgId::null_id(),
            user: b"user".to_vec(),
            time: 42,
            tz: -3600,
            extras: Vec::new(),
            files: Vec::new(),
            message: b"commit message".to_vec(),
        };
        let text = changeset_to_text(cs).unwrap();
        let text_str = String::from_utf8(text).unwrap();
        let time_line = text_str.lines().nth(2).unwrap();
        assert_eq!(time_line, "42 -3600");
    }

    // Invariant: Hg format extracts p1/p2 from the first 40 bytes of serialized data
    #[test]
    fn test_sha1_blob_to_parents_body_hg() {
        let p1 = HgId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        let p2 = HgId::from_hex(b"2222222222222222222222222222222222222222").unwrap();
        let body_content = b"file body here";

        let mut blob = Vec::new();
        blob.extend_from_slice(p1.as_ref());
        blob.extend_from_slice(p2.as_ref());
        blob.extend_from_slice(body_content);
        let data = Bytes::from(blob);

        let (parents, body) = sha1_blob_to_parents_body(&data, SerializationFormat::Hg).unwrap();
        let (actual_p1, actual_p2) = parents.into_nodes();
        assert_eq!(actual_p2, p1);
        assert_eq!(actual_p1, p2);
        assert_eq!(body.as_ref(), body_content);
    }

    // Invariant: Git format returns default (null) parents and extracts body after header
    #[test]
    fn test_sha1_blob_to_parents_body_git() {
        let body_content = b"tree abc123\nauthor foo";
        let header = format!("commit {}", body_content.len());
        let mut blob = Vec::new();
        blob.extend_from_slice(header.as_bytes());
        blob.push(0);
        blob.extend_from_slice(body_content);
        let data = Bytes::from(blob);

        let (parents, body) = sha1_blob_to_parents_body(&data, SerializationFormat::Git).unwrap();
        assert_eq!(parents, Parents::default());
        assert_eq!(body.as_ref(), body_content);
    }

    // Invariant: Hg format splits metadata from file content
    #[test]
    fn test_file_body_to_file_content_and_copy_from_hg() {
        let meta = b"\x01\ncopy: orig.txt\ncopyrev: aaaa\n\x01\n";
        let content = b"actual file content";
        let mut full = Vec::new();
        full.extend_from_slice(meta);
        full.extend_from_slice(content);
        let body = Bytes::from(full);

        let (file_content, copy_from) =
            file_body_to_file_content_and_copy_from(&body, SerializationFormat::Hg);
        assert_eq!(file_content.as_ref(), content);
        assert!(!copy_from.is_empty());
    }

    // Invariant: Git format returns body unchanged with empty metadata
    #[test]
    fn test_file_body_to_file_content_and_copy_from_git() {
        let body = Bytes::from(&b"git blob content"[..]);
        let (file_content, copy_from) =
            file_body_to_file_content_and_copy_from(&body, SerializationFormat::Git);
        assert_eq!(file_content.as_ref(), b"git blob content");
        assert!(copy_from.is_empty());
    }

    // Invariant: set_bookmark rejects when both to and from are None
    #[tokio::test]
    async fn test_set_bookmark_both_none_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let repo = EagerRepo::open(dir.path()).unwrap();

        let result =
            SaplingRemoteApi::set_bookmark(&repo, "test".to_string(), None, None, HashMap::new())
                .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(format!("{err:?}").contains("BAD_REQUEST") || format!("{err:?}").contains("400"));
    }

    // Invariant: set_bookmark returns NOT_FOUND when from is specified but bookmark doesn't exist
    #[tokio::test]
    async fn test_set_bookmark_from_nonexistent_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let repo = EagerRepo::open(dir.path()).unwrap();
        let id = repo.add_commit(&[], b"A").await.unwrap();
        repo.flush().await.unwrap();

        let result = SaplingRemoteApi::set_bookmark(
            &repo,
            "missing".to_string(),
            None,
            Some(id),
            HashMap::new(),
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(format!("{err:?}").contains("NOT_FOUND") || format!("{err:?}").contains("404"));
    }

    // Invariant: set_bookmark rejects when from differs from current bookmark value
    #[tokio::test]
    async fn test_set_bookmark_from_mismatch_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let repo = EagerRepo::open(dir.path()).unwrap();
        let id1 = repo.add_commit(&[], b"A").await.unwrap();
        let id2 = repo.add_commit(&[], b"B").await.unwrap();
        repo.set_bookmark("bm", Some(id1)).unwrap();
        repo.flush().await.unwrap();

        let result = SaplingRemoteApi::set_bookmark(
            &repo,
            "bm".to_string(),
            Some(id1),
            Some(id2),
            HashMap::new(),
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(format!("{err:?}").contains("BAD_REQUEST") || format!("{err:?}").contains("400"));
    }

    // Invariant: set_bookmark rejects creating a bookmark that already exists (no from specified)
    #[tokio::test]
    async fn test_set_bookmark_create_existing_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let repo = EagerRepo::open(dir.path()).unwrap();
        let id1 = repo.add_commit(&[], b"A").await.unwrap();
        let id2 = repo.add_commit(&[], b"B").await.unwrap();
        repo.set_bookmark("bm", Some(id1)).unwrap();
        repo.flush().await.unwrap();

        let result = SaplingRemoteApi::set_bookmark(
            &repo,
            "bm".to_string(),
            Some(id2),
            None,
            HashMap::new(),
        )
        .await;
        assert!(result.is_err());
    }

    // Invariant: set_bookmark with to=None and valid from deletes the bookmark
    #[tokio::test]
    async fn test_set_bookmark_delete() {
        let dir = tempfile::tempdir().unwrap();
        let repo = EagerRepo::open(dir.path()).unwrap();
        let id = repo.add_commit(&[], b"A").await.unwrap();
        repo.set_bookmark("bm", Some(id)).unwrap();
        repo.flush().await.unwrap();

        let result =
            SaplingRemoteApi::set_bookmark(&repo, "bm".to_string(), None, Some(id), HashMap::new())
                .await;
        assert!(result.is_ok());
        let map = repo.get_bookmarks_map().unwrap();
        assert!(!map.contains_key("bm"));
    }

    // Invariant: set_bookmark with to=Some(id) and no from creates a new bookmark
    #[tokio::test]
    async fn test_set_bookmark_create() {
        let dir = tempfile::tempdir().unwrap();
        let repo = EagerRepo::open(dir.path()).unwrap();
        let id = repo.add_commit(&[], b"A").await.unwrap();
        repo.flush().await.unwrap();

        let result = SaplingRemoteApi::set_bookmark(
            &repo,
            "new_bm".to_string(),
            Some(id),
            None,
            HashMap::new(),
        )
        .await;
        assert!(result.is_ok());
        let map = repo.get_bookmarks_map().unwrap();
        assert_eq!(map.get("new_bm"), Some(&id));
    }

    // Invariant: pattern ending with '*' matches all bookmarks by prefix
    #[tokio::test]
    async fn test_list_bookmark_patterns_glob() {
        let dir = tempfile::tempdir().unwrap();
        let repo = EagerRepo::open(dir.path()).unwrap();
        let id1 = repo.add_commit(&[], b"A").await.unwrap();
        let id2 = repo.add_commit(&[], b"B").await.unwrap();
        repo.set_bookmark("release/1.0", Some(id1)).unwrap();
        repo.set_bookmark("release/2.0", Some(id2)).unwrap();
        repo.set_bookmark("main", Some(id1)).unwrap();
        repo.flush().await.unwrap();

        let entries =
            SaplingRemoteApi::list_bookmark_patterns(&repo, vec!["release/*".to_string()], vec![])
                .await
                .unwrap();
        assert_eq!(entries.len(), 2);
        let names: Vec<&str> = entries.iter().map(|e| e.bookmark.as_str()).collect();
        assert!(names.contains(&"release/1.0"));
        assert!(names.contains(&"release/2.0"));
    }

    // Invariant: exact pattern returns only the exact match or None if missing
    #[tokio::test]
    async fn test_list_bookmark_patterns_exact() {
        let dir = tempfile::tempdir().unwrap();
        let repo = EagerRepo::open(dir.path()).unwrap();
        let id = repo.add_commit(&[], b"A").await.unwrap();
        repo.set_bookmark("main", Some(id)).unwrap();
        repo.flush().await.unwrap();

        let entries = SaplingRemoteApi::list_bookmark_patterns(
            &repo,
            vec!["main".to_string(), "nonexistent".to_string()],
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(entries.len(), 2);
        let main_entry = entries.iter().find(|e| e.bookmark == "main").unwrap();
        assert_eq!(main_entry.hgid, Some(id));
        let missing_entry = entries
            .iter()
            .find(|e| e.bookmark == "nonexistent")
            .unwrap();
        assert_eq!(missing_entry.hgid, None);
    }

    // Invariant: default Hg repo capabilities do not include 'git-format' or 'invalid-hash'
    #[tokio::test]
    async fn test_capabilities_hg_default() {
        let dir = tempfile::tempdir().unwrap();
        let repo = EagerRepo::open(dir.path()).unwrap();

        let caps = SaplingRemoteApi::capabilities(&repo).await.unwrap();
        assert!(caps.contains(&"segmented-changelog".to_string()));
        assert!(caps.contains(&"sha1-only".to_string()));
        assert!(!caps.contains(&"git-format".to_string()));
        assert!(!caps.contains(&"invalid-hash".to_string()));
    }

    // Invariant: Git format repo adds 'git-format' capability
    #[tokio::test]
    async fn test_capabilities_git_format() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join("repo.git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let repo = EagerRepo::open(&git_dir).unwrap();

        let caps = SaplingRemoteApi::capabilities(&repo).await.unwrap();
        assert!(caps.contains(&"git-format".to_string()));
    }

    // Invariant: when p2 < p1, upload_filenodes_batch swaps them before assembly
    #[tokio::test]
    async fn test_upload_filenodes_batch_parent_swap() {
        let dir = tempfile::tempdir().unwrap();
        let repo = EagerRepo::open(dir.path()).unwrap();

        let content = b"file content";
        let content_id = repo.add_sha1_blob(content).unwrap();
        repo.flush().await.unwrap();

        let p1 = HgId::from_hex(b"ffffffffffffffffffffffffffffffffffffffff").unwrap();
        let p2 = HgId::from_hex(b"0000000000000000000000000000000000000001").unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(p2.as_ref());
        expected.extend_from_slice(p1.as_ref());
        expected.extend_from_slice(content);

        let expected_id = compute_sha1(&expected);

        let token = UploadToken {
            data: UploadTokenData {
                id: AnyId::AnyFileContentId(hgid_to_content_sha1(content_id)),
                bubble_id: None,
                metadata: None,
            },
            signature: Default::default(),
        };

        let data = HgFilenodeData {
            node_id: expected_id,
            parents: Parents::new(p1, p2),
            file_content_upload_token: token,
            metadata: Vec::new(),
        };

        let resp = SaplingRemoteApi::upload_filenodes_batch(&repo, vec![data])
            .await
            .unwrap();
        let entries: Vec<_> = resp.entries.collect::<Vec<_>>().await;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_ok());
    }

    // Invariant: file body starting with '\x01\n' gets metadata header wrapper
    #[tokio::test]
    async fn test_upload_filenodes_batch_metadata_header_for_special_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let repo = EagerRepo::open(dir.path()).unwrap();

        let content = b"\x01\nsome content that starts with marker";
        let content_id = repo.add_sha1_blob(content).unwrap();
        repo.flush().await.unwrap();

        let p1 = *HgId::null_id();
        let p2 = *HgId::null_id();

        let mut expected = Vec::new();
        expected.extend_from_slice(p1.as_ref());
        expected.extend_from_slice(p2.as_ref());
        expected.extend_from_slice(b"\x01\n");
        expected.extend_from_slice(b"\x01\n");
        expected.extend_from_slice(content);

        let expected_id = compute_sha1(&expected);

        let token = UploadToken {
            data: UploadTokenData {
                id: AnyId::AnyFileContentId(hgid_to_content_sha1(content_id)),
                bubble_id: None,
                metadata: None,
            },
            signature: Default::default(),
        };

        let data = HgFilenodeData {
            node_id: expected_id,
            parents: Parents::new(p1, p2),
            file_content_upload_token: token,
            metadata: Vec::new(),
        };

        let resp = SaplingRemoteApi::upload_filenodes_batch(&repo, vec![data])
            .await
            .unwrap();
        let entries: Vec<_> = resp.entries.collect::<Vec<_>>().await;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_ok());
    }

    // Invariant: non-empty metadata triggers header insertion regardless of body content
    #[tokio::test]
    async fn test_upload_filenodes_batch_nonempty_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let repo = EagerRepo::open(dir.path()).unwrap();

        let content = b"normal content";
        let content_id = repo.add_sha1_blob(content).unwrap();
        repo.flush().await.unwrap();

        let p1 = *HgId::null_id();
        let p2 = *HgId::null_id();
        let metadata = b"copy: orig.txt\ncopyrev: aa\n";

        let mut expected = Vec::new();
        expected.extend_from_slice(p1.as_ref());
        expected.extend_from_slice(p2.as_ref());
        expected.extend_from_slice(b"\x01\n");
        expected.extend_from_slice(metadata);
        expected.extend_from_slice(b"\x01\n");
        expected.extend_from_slice(content);

        let expected_id = compute_sha1(&expected);

        let token = UploadToken {
            data: UploadTokenData {
                id: AnyId::AnyFileContentId(hgid_to_content_sha1(content_id)),
                bubble_id: None,
                metadata: None,
            },
            signature: Default::default(),
        };

        let data = HgFilenodeData {
            node_id: expected_id,
            parents: Parents::new(p1, p2),
            file_content_upload_token: token,
            metadata: metadata.to_vec(),
        };

        let resp = SaplingRemoteApi::upload_filenodes_batch(&repo, vec![data])
            .await
            .unwrap();
        let entries: Vec<_> = resp.entries.collect::<Vec<_>>().await;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_ok());
    }

    // Invariant: empty metadata + body not starting with '\x01\n' produces no header wrapper
    #[tokio::test]
    async fn test_upload_filenodes_batch_no_header() {
        let dir = tempfile::tempdir().unwrap();
        let repo = EagerRepo::open(dir.path()).unwrap();

        let content = b"plain content no marker";
        let content_id = repo.add_sha1_blob(content).unwrap();
        repo.flush().await.unwrap();

        let p1 = *HgId::null_id();
        let p2 = *HgId::null_id();

        let mut expected = Vec::new();
        expected.extend_from_slice(p1.as_ref());
        expected.extend_from_slice(p2.as_ref());
        expected.extend_from_slice(content);

        let expected_id = compute_sha1(&expected);

        let token = UploadToken {
            data: UploadTokenData {
                id: AnyId::AnyFileContentId(hgid_to_content_sha1(content_id)),
                bubble_id: None,
                metadata: None,
            },
            signature: Default::default(),
        };

        let data = HgFilenodeData {
            node_id: expected_id,
            parents: Parents::new(p1, p2),
            file_content_upload_token: token,
            metadata: Vec::new(),
        };

        let resp = SaplingRemoteApi::upload_filenodes_batch(&repo, vec![data])
            .await
            .unwrap();
        let entries: Vec<_> = resp.entries.collect::<Vec<_>>().await;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_ok());
    }
}
