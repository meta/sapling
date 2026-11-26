/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use blobstore::BlobCopier;
use blobstore::Blobstore;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::GenericBlobstoreCopier;
use blobstore::KeyedBlobstore;
use context::CoreContext;
use mononoke_types::BlobstoreBytes;
use mononoke_types::RepositoryId;
use prefixblob::PrefixBlobstore;
use redactedblobstore::RedactedBlobs;
use redactedblobstore::RedactedBlobstore;
use redactedblobstore::RedactedBlobstoreConfig;
use scuba_ext::MononokeScubaSampleBuilder;

use crate::hg_manifest_read_check::HgManifestReadCheck;

mod hg_manifest_read_check;

/// RedactedBlobstore should be part of every blobstore since it is a layer
/// which adds security by preventing users to access sensitive content.
///
/// Making PrefixBlobstore part of every blobstore does two things:
/// 1. It ensures that the prefix applies first, which is important for shared caches like
///    memcache.
/// 2. It ensures that all possible blobrepos use a prefix.
type RepoBlobstoreStack<T> = RedactedBlobstore<PrefixBlobstore<T>>;

// NOTE: We parametize AbstractRepoBlobstore over T instead of explicitly using Arc<dyn Blobstore>
// so that even if we were to add a blobstore to the RepoBlobstoreStack that actually is a Arc<dyn
// Blobstore>, then we cannot accidentally forget to unwrap it below (since we wouldn't get a T
// back).
#[derive(Clone, Debug)]
pub struct AbstractRepoBlobstore<T>(RepoBlobstoreStack<T>);

impl<T: Blobstore + Clone> AbstractRepoBlobstore<T> {
    pub fn as_parts(&self) -> (T, RedactedBlobstoreConfig, String) {
        let (blobstore, redacted_blobstore_config) = self.0.as_parts();
        let prefix = blobstore.prefix();

        (blobstore.into_inner(), redacted_blobstore_config, prefix)
    }
}

#[facet::facet]
#[derive(Clone, Debug)]
pub struct RepoBlobstore {
    blobstore: AbstractRepoBlobstore<Arc<dyn Blobstore>>,
    hg_manifest_read_check: Option<Arc<HgManifestReadCheck>>,
}

impl RepoBlobstore {
    pub fn boxed(&self) -> Arc<dyn KeyedBlobstore> {
        Arc::new(self.clone())
    }

    /// In a repo whose `scm/mononoke:hgmanifest_read_from_augmented` is on,
    /// count reads of Hg manifests by access label (see
    /// `CoreContext::with_access_label`), and log a sample of unlabelled ones.
    /// Count every write of an Hg manifest, and log a sample of them.
    /// Copies made by `new_with_wrapped_inner_blobstore`, such as a bubble's
    /// view of the repo, and by `with_log_only_redaction` keep the check.
    pub fn with_hg_manifest_read_check(self, repo_name: &str) -> Self {
        Self {
            hg_manifest_read_check: Some(Arc::new(HgManifestReadCheck::new(repo_name.to_string()))),
            ..self
        }
    }

    pub fn new(
        blobstore: Arc<dyn Blobstore>,
        redacted_blobs: Option<Arc<RedactedBlobs>>,
        repoid: RepositoryId,
        scuba_builder: MononokeScubaSampleBuilder,
    ) -> Self {
        let redacted_blobstore_config = RedactedBlobstoreConfig::new(redacted_blobs, scuba_builder);
        Self::build(blobstore, repoid.prefix(), redacted_blobstore_config)
    }

    pub fn new_with_wrapped_inner_blobstore<F>(blobstore: RepoBlobstore, wrapper: F) -> Self
    where
        F: FnOnce(Arc<dyn Blobstore>) -> Arc<dyn Blobstore>,
    {
        let (inner, redacted_blobstore_config, prefix) = blobstore.blobstore.as_parts();
        Self {
            hg_manifest_read_check: blobstore.hg_manifest_read_check,
            ..Self::build(wrapper(inner), prefix, redacted_blobstore_config)
        }
    }

    pub fn build(
        blobstore: Arc<dyn Blobstore>,
        prefix: String,
        redacted_blobstore_config: RedactedBlobstoreConfig,
    ) -> Self {
        let blobstore = PrefixBlobstore::new(blobstore, prefix);
        let blobstore = RedactedBlobstore::new(blobstore, redacted_blobstore_config);
        RepoBlobstore {
            blobstore: AbstractRepoBlobstore(blobstore),
            hg_manifest_read_check: None,
        }
    }

    /// Return a view that logs redacted accesses without enforcing them.
    pub fn with_log_only_redaction(&self) -> Self {
        let (blobstore, redacted_blobstore_config) = self.blobstore.0.as_parts();
        let blobstore = RedactedBlobstore::new(
            blobstore,
            redacted_blobstore_config.with_log_only_redaction(),
        );
        Self {
            blobstore: AbstractRepoBlobstore(blobstore),
            hg_manifest_read_check: self.hg_manifest_read_check.clone(),
        }
    }

    pub fn copier_to<'a>(&'a self, other: &'a RepoBlobstore) -> RepoBlobstoreCopier<'a> {
        RepoBlobstoreCopier::new(self, other)
    }
}

impl std::fmt::Display for RepoBlobstore {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "RepoBlobstore<{}>", self.blobstore.0)
    }
}

#[async_trait]
impl KeyedBlobstore for RepoBlobstore {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        if let Some(check) = &self.hg_manifest_read_check {
            check.check_read(ctx, key, "get");
        }
        Blobstore::get(&self.blobstore.0, ctx, key).await
    }
    async fn put<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<()> {
        if let Some(check) = &self.hg_manifest_read_check {
            check.check_write(ctx, &key, "put");
        }
        Blobstore::put(&self.blobstore.0, ctx, key, value).await
    }
    async fn is_present<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent> {
        if let Some(check) = &self.hg_manifest_read_check {
            check.check_read(ctx, key, "is_present");
        }
        Blobstore::is_present(&self.blobstore.0, ctx, key).await
    }
    async fn copy<'a>(
        &'a self,
        ctx: &'a CoreContext,
        old_key: &'a str,
        new_key: String,
    ) -> Result<()> {
        if let Some(check) = &self.hg_manifest_read_check {
            check.check_write(ctx, &new_key, "copy");
        }
        Blobstore::copy(&self.blobstore.0, ctx, old_key, new_key).await
    }
    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        Blobstore::unlink(&self.blobstore.0, ctx, key).await
    }
}

pub enum RepoBlobstoreCopier<'a> {
    Unoptimized(GenericBlobstoreCopier<'a, RepoBlobstore, RepoBlobstore>),
    /// We checked both repo blobstores have the same inner storage, but differ
    /// in prefixes
    Optimized {
        source: &'a PrefixBlobstore<Arc<dyn Blobstore>>,
        target: &'a PrefixBlobstore<Arc<dyn Blobstore>>,
    },
}

impl<'a> RepoBlobstoreCopier<'a> {
    fn new(source: &'a RepoBlobstore, target: &'a RepoBlobstore) -> Self {
        let inner_source = source.blobstore.0.as_inner_unredacted();
        let inner_target = target.blobstore.0.as_inner_unredacted();
        #[allow(ambiguous_wide_pointer_comparisons)]
        if Arc::ptr_eq(inner_source.as_inner(), inner_target.as_inner()) {
            Self::Optimized {
                source: inner_source,
                target: inner_target,
            }
        } else {
            Self::Unoptimized(GenericBlobstoreCopier { source, target })
        }
    }

    pub fn is_optimized(&self) -> bool {
        matches!(self, RepoBlobstoreCopier::Optimized { .. })
    }
}

#[async_trait]
impl<'a> BlobCopier for RepoBlobstoreCopier<'a> {
    async fn copy(&self, ctx: &CoreContext, key: String) -> Result<()> {
        match self {
            Self::Unoptimized(generic) => generic.copy(ctx, key).await,
            Self::Optimized { source, target } => {
                // same as target.as_inner()
                let inner: &Arc<dyn Blobstore> = source.as_inner();
                let source_key = source.prepend(&key);
                let target_key = target.prepend(&key);
                inner.copy(ctx, &source_key, target_key).await
            }
        }
    }
}
