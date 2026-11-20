/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Backfills augmented manifests, and the ACL manifests they depend on, for
//! commits that lack them.

mod repo_backfill;

pub use repo_backfill::CommitOutcome;
pub use repo_backfill::RepoBackfill;
