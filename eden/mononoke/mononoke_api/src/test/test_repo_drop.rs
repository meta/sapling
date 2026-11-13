/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use blobstore::Blobstore;
use blobstore::BlobstoreGetData;
use blobstore::OverwriteStatus;
use blobstore::PutBehaviour;
use context::CoreContext;
use fbinit::FacebookInit;
use memblob::Memblob;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreBytes;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;
use tests_utils::bookmark;
use tokio::sync::Notify;

use crate::repo::Repo;

/// In-memory blobstore whose reads never complete once `hang_reads` is set.
#[derive(Debug, Default)]
struct HangingBlobstore {
    inner: Memblob,
    hang_reads: AtomicBool,
    read_hung: Notify,
}

impl fmt::Display for HangingBlobstore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HangingBlobstore")
    }
}

#[async_trait]
impl Blobstore for HangingBlobstore {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        if self.hang_reads.load(Ordering::SeqCst) {
            self.read_hung.notify_one();
            std::future::pending::<()>().await;
        }
        self.inner.get(ctx, key).await
    }

    async fn put_explicit<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
        put_behaviour: PutBehaviour,
    ) -> Result<OverwriteStatus> {
        self.inner
            .put_explicit(ctx, key, value, put_behaviour)
            .await
    }

    async fn put_with_status<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<OverwriteStatus> {
        self.inner.put_with_status(ctx, key, value).await
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        Blobstore::unlink(&self.inner, ctx, key).await
    }
}

/// A `Weak` reference to each named facet, paired with the facet's name.
macro_rules! weak_facets {
    ($repo:ident: $($facet:ident)*) => {
        vec![$({
            let weak = Arc::downgrade(&$repo.$facet);
            (stringify!($facet), Box::new(move || weak.strong_count() > 0) as Box<dyn Fn() -> bool>)
        }),*]
    };
}

#[mononoke::fbinit_test]
async fn dropping_repo_frees_its_facets(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let blobstore = Arc::new(HangingBlobstore::default());
    let repo: Repo = TestRepoFactory::new(fb)?
        .with_blobstore(blobstore.clone())
        .build()
        .await?;

    // Leave a warm bookmarks cache updater stuck on a blobstore read when
    // the repo is dropped.
    let cs_id = CreateCommitContext::new_root(&ctx, &repo)
        .add_file("file", "content")
        .commit()
        .await?;
    blobstore.hang_reads.store(true, Ordering::SeqCst);
    bookmark(&ctx, &repo, "master").set_to(cs_id).await?;
    tokio::time::timeout(Duration::from_secs(30), blobstore.read_hung.notified()).await?;

    let facets = weak_facets!(repo:
        repo_blobstore mutable_repo_blobstore repo_bookmark_attrs repo_derived_data repo_identity
        bonsai_tag_mapping bonsai_git_mapping git_ref_content_mapping git_bundle_uri
        bonsai_globalrev_mapping repo_event_publisher bonsai_svnrev_mapping bonsai_p4_mapping
        bonsai_hg_mapping bookmark_update_log bookmarks phases pushrebase_mutation_mapping
        hg_mutation_store mutable_counters repo_permission_checker repo_lock repo_config
        repo_config_source repo_ephemeral_store mutable_renames repo_cross_repo acl_regions
        repo_sparse_profiles streaming_clone commit_graph commit_graph_writer git_symbolic_refs
        git_source_of_truth_config enabled_derived_data_types filenodes commit_cloud
        sql_query_config warm_bookmarks_cache hook_manager repo_handler_base filestore_config
        repo_stats_logger commit_graph_bulk_fetcher commit_rate_limit cgdm_changeset_divider
    );
    drop(repo);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let alive: Vec<&str> = facets
        .iter()
        .filter(|(_, is_alive)| is_alive())
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(alive, Vec::<&str>::new());
    Ok(())
}
