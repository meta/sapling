/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

/*

*
* This software may be used and distributed according to the terms of the
* GNU General Public License version 2.
*/

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use anyhow::bail;
use async_trait::async_trait;
use context::CoreContext;
use fbinit::FacebookInit;
use git_env::GitHost;
use metaconfig_types::GitBundleURIConfig;
#[cfg(fbcode_build)]
use metaconfig_types::UriGeneratorType;
use mononoke_types::RepositoryId;

#[cfg(fbcode_build)]
mod facebook;
#[cfg(fbcode_build)]
pub use facebook::cdn;
#[cfg(fbcode_build)]
pub use facebook::manifold;

mod sql;

#[cfg(fbcode_build)]
pub use cdn::CdnManifoldBundleUrlGenerator;
#[cfg(fbcode_build)]
pub use manifold::ManifoldBundleUrlGenerator;

pub use crate::sql::SqlGitBundleMetadataStorage;
pub use crate::sql::SqlGitBundleMetadataStorageBuilder;

#[async_trait]
pub trait GitBundleMetadataStorage {
    async fn get_newest_bundle_list_for_repo(
        &self,
        repo_id: RepositoryId,
    ) -> Result<Option<BundleList>>;
    async fn get_newest_bundle_lists(&self) -> Result<HashMap<RepositoryId, BundleList>>;
}

#[async_trait]
pub trait GitBundleUrlGenerator {
    async fn get_url_for_bundle_handle(
        &self,
        ctx: &CoreContext,
        git_host: &GitHost,
        ttl: i64,
        handle: &str,
    ) -> Result<String>;
}

#[async_trait]
impl GitBundleUrlGenerator for LocalFSBUndleUriGenerator {
    async fn get_url_for_bundle_handle(
        &self,
        _ctx: &CoreContext,
        _git_host: &GitHost,
        _ttl: i64,
        handle: &str,
    ) -> Result<String> {
        Ok(format!("file://{handle}"))
    }
}

#[derive(Clone)]
pub struct LocalFSBUndleUriGenerator {}

/// The URL generator of a repo with no bundle-uri config: its bundles have no
/// URLs, so none are offered to clients.
#[derive(Clone)]
pub struct NoBundleUrlGenerator {}

#[async_trait]
impl GitBundleUrlGenerator for NoBundleUrlGenerator {
    async fn get_url_for_bundle_handle(
        &self,
        _ctx: &CoreContext,
        _git_host: &GitHost,
        _ttl: i64,
        _handle: &str,
    ) -> Result<String> {
        bail!("the repo has no bundle-uri config, so its bundles have no URLs")
    }
}

#[facet::facet]
#[async_trait]
/// Facet trait powering git's bundle-uri feature
pub trait GitBundleUri: Send + Sync {
    /// Gets the latest list of git bundles which together comprise the whole repo.
    /// There might be None.
    async fn get_latest_bundle_list(&self, ctx: &CoreContext) -> Result<Option<BundleList>>;

    /// Gets the latest list of git bundles which together comprise the whole repo.
    /// There might be None. Provides read-after-write consistency.
    async fn get_latest_bundle_list_from_primary(
        &self,
        ctx: &CoreContext,
    ) -> Result<Option<BundleList>>;

    /// Get all available bundle lists for a repo.
    async fn get_bundle_lists(&self, ctx: &CoreContext) -> Result<Vec<BundleList>>;

    /// Remove all bundles in a given bundle list from the metadata db.
    async fn remove_bundle_list_from_metadata_db(
        &self,
        ctx: &CoreContext,
        bundle_list_num: u64,
    ) -> Result<()>;

    async fn get_url_for_bundle_handle(
        &self,
        ctx: &CoreContext,
        git_host: &GitHost,
        ttl: i64,
        handle: &str,
    ) -> Result<String>;

    /// Adds bundles as a new bundle list to the metadata db. Return bundle list num.
    async fn add_new_bundles(&self, ctx: &CoreContext, bundles: &[Bundle]) -> Result<u64>;

    /// The repository for which the bundles are being tracked
    fn repo_id(&self) -> RepositoryId;
}

#[derive(Clone, Debug)]
pub struct Bundle {
    pub handle: String,
    pub fingerprint: String,
    pub in_bundle_list_order: u64,
    pub generation_start_timestamp: u64,
}

#[derive(Clone, Debug)]
pub struct BundleList {
    pub bundle_list_num: u64,
    pub bundles: Vec<Bundle>,
}

pub struct BundleUri<U> {
    pub bundle_metadata_storage: SqlGitBundleMetadataStorage,
    pub bundle_url_generator: U,
    pub repo_id: RepositoryId,
}

impl<U: Send + Sync + Clone + GitBundleUrlGenerator + 'static> BundleUri<U> {
    pub async fn new(
        storage: SqlGitBundleMetadataStorage,
        bundle_url_generator: U,
        repo_id: RepositoryId,
    ) -> Result<Self>
    where
        U: GitBundleUrlGenerator + Clone + Send + Sync,
    {
        Ok(Self {
            bundle_metadata_storage: storage,
            bundle_url_generator,
            repo_id,
        })
    }
}

/// The bundle-uri facet of a repo. Without a `config`, bundles are still
/// recorded and listed, but have no URLs.
#[cfg(fbcode_build)]
pub fn bundle_uri_arc(
    fb: FacebookInit,
    storage: SqlGitBundleMetadataStorage,
    repo_id: RepositoryId,
    config: Option<&GitBundleURIConfig>,
) -> Arc<dyn GitBundleUri + Send + Sync + 'static> {
    let Some(config) = config else {
        return Arc::new(BundleUri {
            bundle_metadata_storage: storage,
            bundle_url_generator: NoBundleUrlGenerator {},
            repo_id,
        });
    };
    match &config.uri_generator_type {
        UriGeneratorType::Cdn { bucket, api_key } => Arc::new(BundleUri {
            bundle_metadata_storage: storage,
            bundle_url_generator: CdnManifoldBundleUrlGenerator::new(
                fb,
                bucket.clone(),
                api_key.clone(),
            ),
            repo_id,
        }),
        UriGeneratorType::Manifold { bucket, api_key } => Arc::new(BundleUri {
            bundle_metadata_storage: storage,
            bundle_url_generator: ManifoldBundleUrlGenerator::new(
                fb,
                bucket.clone(),
                api_key.clone(),
            ),
            repo_id,
        }),
        UriGeneratorType::LocalFS => Arc::new(BundleUri {
            bundle_metadata_storage: storage,
            bundle_url_generator: LocalFSBUndleUriGenerator {},
            repo_id,
        }),
    }
}

/// The bundle-uri facet of a repo. Without a `config`, bundles are still
/// recorded and listed, but have no URLs.
#[cfg(not(fbcode_build))]
pub fn bundle_uri_arc(
    _fb: FacebookInit,
    storage: SqlGitBundleMetadataStorage,
    repo_id: RepositoryId,
    config: Option<&GitBundleURIConfig>,
) -> Arc<dyn GitBundleUri + Send + Sync + 'static> {
    match config {
        Some(_) => Arc::new(BundleUri {
            bundle_metadata_storage: storage,
            bundle_url_generator: LocalFSBUndleUriGenerator {},
            repo_id,
        }),
        None => Arc::new(BundleUri {
            bundle_metadata_storage: storage,
            bundle_url_generator: NoBundleUrlGenerator {},
            repo_id,
        }),
    }
}

#[async_trait]
impl<U: Clone + Send + GitBundleUrlGenerator + Sync> GitBundleUri for BundleUri<U> {
    fn repo_id(&self) -> RepositoryId {
        self.repo_id
    }

    async fn get_latest_bundle_list(&self, ctx: &CoreContext) -> Result<Option<BundleList>> {
        self.bundle_metadata_storage
            .get_latest_bundle_list(ctx)
            .await
    }

    async fn get_latest_bundle_list_from_primary(
        &self,
        ctx: &CoreContext,
    ) -> Result<Option<BundleList>> {
        self.bundle_metadata_storage
            .get_latest_bundle_list_from_primary(ctx)
            .await
    }

    async fn get_bundle_lists(&self, ctx: &CoreContext) -> Result<Vec<BundleList>> {
        self.bundle_metadata_storage.get_bundle_lists(ctx).await
    }

    async fn remove_bundle_list_from_metadata_db(
        &self,
        ctx: &CoreContext,
        bundle_list_num: u64,
    ) -> Result<()> {
        self.bundle_metadata_storage
            .remove_bundle_list(ctx, bundle_list_num)
            .await
    }

    async fn add_new_bundles(&self, ctx: &CoreContext, bundles: &[Bundle]) -> Result<u64> {
        self.bundle_metadata_storage
            .add_new_bundles(ctx, bundles)
            .await
    }

    async fn get_url_for_bundle_handle(
        &self,
        ctx: &CoreContext,
        git_host: &GitHost,
        ttl: i64,
        handle: &str,
    ) -> Result<String> {
        self.bundle_url_generator
            .get_url_for_bundle_handle(ctx, git_host, ttl, handle)
            .await
    }
}
