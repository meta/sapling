/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Tests for bookmark service scope parameter usage.
//!
//! These tests verify that the bookmark service correctly passes the scope
//! parameter from Thrift requests to the underlying scoped bookmarks cache.

use std::sync::Arc;

use anyhow::Result;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmark_service_if::DerivedDataSet;
use bookmark_service_if::GetRequest;
use bookmarks::BookmarkUpdateLog;
use bookmarks::BookmarkUpdateLogArc;
use bookmarks::Bookmarks;
use bookmarks::BookmarksArc;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use fbinit::FacebookInit;
use filestore::FilestoreConfig;
use fixtures::Linear;
use fixtures::TestRepoFixture;
use mercurial_derivation::MappedHgChangesetId;
use mononoke_macros::mononoke;
use mononoke_repos::MononokeRepos;
use repo_blobstore::RepoBlobstore;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataArc;
use repo_derived_data::RepoDerivedDataRef;
use repo_event_publisher::RepoEventPublisher;
use repo_event_publisher::RepoEventPublisherArc;
use repo_identity::RepoIdentity;
use repo_identity::RepoIdentityArc;
use repo_identity::RepoIdentityRef;
use tests_utils::resolve_cs_id;
use warm_bookmarks_cache::InitMode;
use warm_bookmarks_cache::WarmBookmarksCache;
use warm_bookmarks_cache::WarmerTag;
use warm_bookmarks_cache::create_derived_data_warmer;

use super::BookmarkServiceImpl;
use super::BookmarkServiceRepo;

#[facet::container]
#[derive(Clone)]
struct TestRepo(
    RepoIdentity,
    RepoBlobstore,
    RepoDerivedData,
    CommitGraph,
    dyn CommitGraphWriter,
    dyn BonsaiHgMapping,
    dyn BookmarkUpdateLog,
    dyn Bookmarks,
    dyn RepoEventPublisher,
    FilestoreConfig,
);

/// Test that the bookmark service correctly passes the scope parameter to the cache.
///
/// This test creates a service with a cache that has only Hg warmers, then verifies that:
/// - Querying with HG_ONLY scope succeeds (Hg warmers are configured)
/// - Querying with GIT_ONLY scope fails with "not being tracked" error
#[mononoke::fbinit_test]
async fn test_service_uses_scope_parameter(fb: FacebookInit) -> Result<()> {
    let repo: TestRepo = Linear::get_repo(fb).await;
    let ctx = CoreContext::test_mock(fb);

    // Get the master bookmark's changeset ID
    let master_cs_id = resolve_cs_id(&ctx, &repo, "master").await?;

    // Derive Hg data for the master changeset
    repo.repo_derived_data()
        .derive::<MappedHgChangesetId>(&ctx, master_cs_id, DerivationPriority::LOW)
        .await?;

    // Create a warm bookmarks cache with ONLY Hg warmers (no Git warmers)
    let warmers = vec![create_derived_data_warmer::<MappedHgChangesetId>(
        &ctx,
        repo.repo_derived_data_arc(),
        vec![WarmerTag::Hg],
    )];

    let cache = WarmBookmarksCache::new(
        &ctx,
        &repo.bookmarks_arc(),
        &repo.bookmark_update_log_arc(),
        &repo.repo_identity_arc(),
        &repo.repo_event_publisher_arc(),
        false,
        warmers,
        InitMode::Warm, // Wait for warming to complete
        bookmarks_cache::WarmerRequirement::AllKinds,
    )
    .await?;

    // Create a BookmarkServiceRepo with our cache
    let service_repo = BookmarkServiceRepo {
        repo_identity: repo.repo_identity_arc(),
        scoped_bookmarks_cache: Arc::new(cache),
    };

    // Create a MononokeRepos and add our test repo
    let repos = MononokeRepos::new();
    repos.add(
        repo.repo_identity().name(),
        repo.repo_identity().id().id(),
        service_repo,
    );

    let repo_name = repo.repo_identity().name().to_string();

    // Test 1: HG_ONLY scope should succeed
    let hg_request = GetRequest {
        repo_name: repo_name.clone(),
        bookmark_name: "master".to_string(),
        derived_data_types: Some(DerivedDataSet::HG_ONLY),
    };
    let hg_result = BookmarkServiceImpl::get_with_repos(&repos, ctx.clone(), hg_request).await;
    assert!(
        hg_result.is_ok(),
        "HgOnly scope should succeed when Hg warmers are configured"
    );
    assert!(
        hg_result.unwrap().changeset_id.is_some(),
        "HgOnly scope should return the bookmark"
    );

    // Test 2: GIT_ONLY scope should fail because no Git warmers are configured
    let git_request = GetRequest {
        repo_name: repo_name.clone(),
        bookmark_name: "master".to_string(),
        derived_data_types: Some(DerivedDataSet::GIT_ONLY),
    };
    let git_result = BookmarkServiceImpl::get_with_repos(&repos, ctx.clone(), git_request).await;
    assert!(
        git_result.is_err(),
        "GitOnly scope should fail when no Git warmers are configured"
    );
    let error = git_result.unwrap_err();
    let error_msg = format!("{error:?}");
    assert_eq!(
        error_msg,
        "InternalError(The requirement requested is not being tracked: GitOnly)"
    );

    // Test 3: ALL_KINDS scope behavior when only Hg warmers are available
    // Since Git is not being tracked, All kinds is equivalent to Hg.
    let all_request = GetRequest {
        repo_name,
        bookmark_name: "master".to_string(),
        derived_data_types: Some(DerivedDataSet::ALL_KINDS),
    };
    let all_result = BookmarkServiceImpl::get_with_repos(&repos, ctx, all_request).await;
    assert!(
        all_result.is_ok(),
        "AllKinds scope should still succeed when just Hg warmers are configured"
    );
    assert!(
        all_result.unwrap().changeset_id.is_some(),
        "AllKinds scope should return the bookmark"
    );

    Ok(())
}
