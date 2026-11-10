/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use cas_client::CasFetchManager;
use configmodel::Config;
use configmodel::ConfigExt;
use context;
use revisionstore::SaplingRemoteApiFileStore;
use revisionstore::SaplingRemoteApiTreeStore;
use revisionstore::scmstore;
use revisionstore::scmstore::FileStoreBuilder;
use revisionstore::scmstore::MaxFetchCount;
use revisionstore::scmstore::TreeStoreBuilder;
use storemodel::SerializationFormat;
use storemodel::StoreInfo;

/// Default file-fetch limit when running as an AI coding agent.
/// Sized to be comfortably above any realistic single-command working set
/// while still catching accidental whole-repo scans.
const DEFAULT_MAX_FILE_FETCH_COUNT: u64 = 20_000;

/// Default tree-fetch limit when running as an AI coding agent.
/// Tracked separately from files because trees scale roughly with the number
/// of touched directories (typically smaller than the file count).
const DEFAULT_MAX_TREE_FETCH_COUNT: u64 = 10_000;

/// Build a `MaxFetchCount` from `[agent]` `<config_key>` (default `default`)
/// when the current process is detected as an AI coding agent and not running
/// in plain mode. `item_kind` (e.g. `"files"`, `"trees"`) appears in the
/// user-facing abort message. A configured limit of `0` returns the default
/// (disabled) counter.
fn agent_max_fetch_count(
    config: &dyn Config,
    config_key: &str,
    default: u64,
    item_kind: &str,
) -> Result<MaxFetchCount> {
    if !config.get_or("agent", "enable-fetch-guard", || true)? {
        return Ok(MaxFetchCount::default());
    }
    if hgplain::is_plain(None) || !agentdetect::is_agent() {
        return Ok(MaxFetchCount::default());
    }
    let limit: u64 = config.get_or("agent", config_key, || default)?;
    if limit == 0 {
        return Ok(MaxFetchCount::default());
    }
    let cli = identity::default().cli_name();
    let err_msg = format!(
        "command accessed over {limit} {item_kind} from store\n\
         (narrow it to the paths you need, and find files by name with code search; \
         see '{cli} help agent performance')"
    );
    Ok(MaxFetchCount::new(limit, err_msg))
}

pub fn build_scm_file_store(
    info: &dyn StoreInfo,
    cas_manager: Option<Arc<CasFetchManager>>,
) -> Result<Arc<scmstore::FileStore>> {
    tracing::trace!(target: "repo::file_store", "building filestore");
    let mut file_builder = FileStoreBuilder::new(info.config()).cas_manager(cas_manager);

    if let Some(store_path) = info.store_path() {
        file_builder = file_builder.local_path(store_path);
    }

    if let Some(eden_api) = info.remote_peer()? {
        tracing::trace!(target: "repo::file_store", "enabling edenapi");
        file_builder = file_builder.edenapi(SaplingRemoteApiFileStore::new(eden_api));
    } else {
        tracing::trace!(target: "repo::file_store", "disabling edenapi");
        file_builder = file_builder.override_edenapi(false);
    }

    if info.has_requirement("git") {
        tracing::trace!(target: "repo::file_store", "enabling git serialization");
        file_builder = file_builder.format(SerializationFormat::Git);
    }

    file_builder = file_builder.max_fetch_count(agent_max_fetch_count(
        info.config(),
        "max-file-fetch-count",
        DEFAULT_MAX_FILE_FETCH_COUNT,
        "files",
    )?);

    tracing::trace!(target: "repo::file_store", "building file store");
    let file_store = file_builder.build().context("when building FileStore")?;

    Ok(Arc::new(file_store))
}

pub fn build_scm_tree_store(
    info: &dyn StoreInfo,
    file_store: Option<Arc<scmstore::FileStore>>,
    permission_denied_paths: Option<context::PermissionDeniedPaths>,
    cas_manager: Option<Arc<CasFetchManager>>,
) -> Result<Arc<scmstore::TreeStore>> {
    tracing::trace!(target: "repo::tree_store", "building treestore");
    let mut tree_builder = TreeStoreBuilder::new(info.config())
        .suffix("manifests")
        .cas_manager(cas_manager);

    if let Some(paths) = permission_denied_paths {
        tree_builder = tree_builder.permission_denied_paths(paths);
    }

    if let Some(store_path) = info.store_path() {
        tree_builder = tree_builder.local_path(store_path);
    }

    if let Some(eden_api) = info.remote_peer()? {
        tracing::trace!(target: "repo::tree_store", "enabling edenapi");
        tree_builder = tree_builder.edenapi(SaplingRemoteApiTreeStore::new(eden_api));
    } else {
        tracing::trace!(target: "repo::tree_store", "disabling edenapi");
        tree_builder = tree_builder.override_edenapi(false);
    }

    // The presence of the file store on the tree store causes the tree store to
    // request tree metadata (and write it back to file store aux cache).
    if let Some(file_store) = file_store {
        tracing::trace!(target: "repo::tree_store", "configuring filestore for aux fetching");
        tree_builder = tree_builder.filestore(file_store);
    } else {
        tracing::trace!(target: "repo::tree_store", "no filestore for aux fetching");
    }

    if info.has_requirement("git") {
        tracing::trace!(target: "repo::tree_store", "enabling git serialization");
        tree_builder = tree_builder.format(SerializationFormat::Git);
    }

    tree_builder = tree_builder.max_fetch_count(agent_max_fetch_count(
        info.config(),
        "max-tree-fetch-count",
        DEFAULT_MAX_TREE_FETCH_COUNT,
        "trees",
    )?);

    tracing::trace!(target: "repo::tree_store", "building tree store");
    let tree_store = tree_builder.build().context("when building TreeStore")?;

    Ok(Arc::new(tree_store))
}
