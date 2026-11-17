/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Test helper functions for warm bookmarks cache tests.
//!
//! This module provides reusable helper functions to reduce code duplication
//! across bookmark tests, particularly for scoped bookmark tracking tests.

use std::sync::Arc;

use anyhow::Error;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLogArc;
use bookmarks::BookmarksArc;
use context::CoreContext;
use git_types::MappedGitCommitId;
use mercurial_derivation::MappedHgChangesetId;
use mononoke_types::ChangesetId;
use repo_derivation_queues::DerivationPriority;
use repo_derived_data::RepoDerivedDataArc;
use repo_derived_data::RepoDerivedDataRef;
use repo_event_publisher::RepoEventPublisherArc;
use repo_identity::RepoIdentityArc;
use tests_utils::CreateCommitContext;
use unodes::RootUnodeManifestId;

use crate::BookmarkState;
use crate::InitMode;
use crate::WarmBookmarksCache;
use crate::Warmer;
use crate::WarmerRequirement;
use crate::WarmerTag;
use crate::tests::Repo;
use crate::warmers::create_derived_data_warmer;

// ===== Configuration Helpers =====

/// Configuration for warmer setup
#[derive(Clone, Debug)]
pub struct WarmerConfig {
    pub include_hg: bool,
    pub include_git: bool,
    pub include_unodes: bool,
}

impl WarmerConfig {
    #[allow(dead_code)]
    pub fn git_only() -> Self {
        Self {
            include_hg: false,
            include_git: true,
            include_unodes: true,
        }
    }

    pub fn all_warmers() -> Self {
        Self {
            include_hg: true,
            include_git: true,
            include_unodes: true,
        }
    }

    #[allow(dead_code)]
    pub fn hg_only() -> Self {
        Self {
            include_hg: true,
            include_git: false,
            include_unodes: true,
        }
    }
}

// ===== Setup Helpers =====

/// Creates warmers based on configuration to reduce duplication
pub fn setup_warmers(ctx: &CoreContext, repo: &Repo, config: WarmerConfig) -> Arc<Vec<Warmer>> {
    let mut warmers = Vec::new();

    if config.include_hg {
        warmers.push(create_derived_data_warmer::<MappedHgChangesetId>(
            ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg],
        ));
    }

    if config.include_git {
        warmers.push(create_derived_data_warmer::<MappedGitCommitId>(
            ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Git],
        ));
    }

    if config.include_unodes {
        // Unodes requires both Hg and Git tags
        warmers.push(create_derived_data_warmer::<RootUnodeManifestId>(
            ctx,
            repo.repo_derived_data_arc(),
            vec![WarmerTag::Hg, WarmerTag::Git],
        ));
    }

    Arc::new(warmers)
}

/// Initializes bookmarks cache with standard configuration
pub async fn init_cache_with_warmers(
    ctx: &CoreContext,
    repo: &Repo,
    warmers: Vec<Warmer>,
    mode: InitMode,
) -> Result<WarmBookmarksCache, Error> {
    WarmBookmarksCache::new(
        ctx,
        &repo.bookmarks_arc(),
        &repo.bookmark_update_log_arc(),
        &repo.repo_identity_arc(),
        &repo.repo_event_publisher_arc(),
        warmers,
        mode,
        WarmerRequirement::AllKinds,
    )
    .await
}

// ===== Derivation Helpers =====

/// Helper to derive specific data types for a changeset
pub async fn derive_data(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
    derive_hg: bool,
    derive_git: bool,
    derive_unodes: bool,
) -> Result<(), Error> {
    if derive_hg {
        repo.repo_derived_data()
            .derive::<MappedHgChangesetId>(ctx, cs_id, DerivationPriority::LOW)
            .await?;
    }

    if derive_git {
        repo.repo_derived_data()
            .derive::<MappedGitCommitId>(ctx, cs_id, DerivationPriority::LOW)
            .await?;
    }

    if derive_unodes {
        repo.repo_derived_data()
            .derive::<RootUnodeManifestId>(ctx, cs_id, DerivationPriority::LOW)
            .await?;
    }

    Ok(())
}

/// Creates a commit chain with specific derivation patterns
pub struct CommitChainBuilder<'a> {
    ctx: &'a CoreContext,
    repo: &'a Repo,
    parent: ChangesetId,
}

impl<'a> CommitChainBuilder<'a> {
    pub fn new(ctx: &'a CoreContext, repo: &'a Repo, parent: ChangesetId) -> Self {
        Self { ctx, repo, parent }
    }

    /// Creates a commit and derives specified data types
    pub async fn add_commit(
        &mut self,
        file_name: &str,
        derive_hg: bool,
        derive_git: bool,
        derive_unodes: bool,
    ) -> Result<ChangesetId, Error> {
        let commit = CreateCommitContext::new(self.ctx, self.repo, vec![self.parent])
            .add_file(file_name, "content")
            .commit()
            .await?;

        derive_data(
            self.ctx,
            self.repo,
            commit,
            derive_hg,
            derive_git,
            derive_unodes,
        )
        .await?;

        self.parent = commit;
        Ok(commit)
    }
}

// ===== Assertion Helpers =====

/// Verifies that a bookmark has the expected pointers for each scope
pub fn assert_bookmark_pointers(
    state: &BookmarkState,
    expected_hg: Option<ChangesetId>,
    expected_git: Option<ChangesetId>,
    expected_all: Option<ChangesetId>,
    description: &str,
) {
    assert_eq!(
        state.get(WarmerRequirement::HgOnly).unwrap(),
        expected_hg,
        "{description}: HgOnly pointer mismatch"
    );

    assert_eq!(
        state.get(WarmerRequirement::GitOnly).unwrap(),
        expected_git,
        "{description}: GitOnly pointer mismatch"
    );

    assert_eq!(
        state.get(WarmerRequirement::AllKinds).unwrap(),
        expected_all,
        "{description}: AllKinds pointer mismatch"
    );
}

/// Verifies ScopedBookmarksCache::get returns expected values for all scopes
pub async fn assert_scoped_get(
    cache: &WarmBookmarksCache,
    ctx: &CoreContext,
    bookmark_key: &BookmarkKey,
    expected_hg: Option<ChangesetId>,
    expected_git: Option<ChangesetId>,
    expected_all: Option<ChangesetId>,
    description: &str,
) -> Result<(), Error> {
    use crate::ScopedBookmarksCache;

    let hg_result =
        ScopedBookmarksCache::get(cache, ctx, bookmark_key, WarmerRequirement::HgOnly).await?;
    assert_eq!(
        hg_result, expected_hg,
        "{description}: ScopedBookmarksCache HgOnly mismatch"
    );

    let git_result =
        ScopedBookmarksCache::get(cache, ctx, bookmark_key, WarmerRequirement::GitOnly).await?;
    assert_eq!(
        git_result, expected_git,
        "{description}: ScopedBookmarksCache GitOnly mismatch"
    );

    let all_result =
        ScopedBookmarksCache::get(cache, ctx, bookmark_key, WarmerRequirement::AllKinds).await?;
    assert_eq!(
        all_result, expected_all,
        "{description}: ScopedBookmarksCache AllKinds mismatch"
    );

    Ok(())
}
