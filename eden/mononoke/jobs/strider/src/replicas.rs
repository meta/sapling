/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Replica checks: each load reads every store of a multiplex, and a copy a
//! store lacks is reported, never repaired.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use blobstore::BlobstoreMetadata;
use blobstore_factory::ScrubAction;
use blobstore_factory::ScrubHandler;
use blobstore_factory::ScrubOptions;
use blobstore_factory::SrubWriteOnly;
use clap::Args;
use cmdlib_scrubbing::ScrubStatsKey;
use cmdlib_scrubbing::StatsScrubHandler;
use context::CoreContext;
use environment::MononokeEnvironment;
use metaconfig_types::BlobstoreId;
use mononoke_app::AppExtension;
use mononoke_types::ChangesetId;
use mononoke_types::RepositoryId;
use mononoke_types::audit::LoadTag;
use mononoke_types::audit::LoadTags;
use scuba_ext::MononokeScubaSampleBuilder;
use tracing::warn;

/// How long a copy may be missing from a store before it is reported, as in
/// the deployed Walker jobs: a write succeeds once a quorum of stores has the
/// blob, and the healer copies it to the others later.
const SCRUB_GRACE: Duration = Duration::from_secs(120);

/// The names of Strider's counters,
/// `mononoke.strider.scrub.blobstore.<store>.<repo>.repair_required`.
pub const SCRUB_STATS_KEY: ScrubStatsKey = ScrubStatsKey {
    tool: "strider",
    subcommand: "scrub",
};

#[derive(Args, Debug)]
pub struct ReplicaCheckArgs {
    /// Read each key from every store of a multiplexed blobstore, and report
    /// each copy missing for more than 120 seconds. Stores holding different
    /// values make the load unreadable. Nothing is ever repaired.
    #[clap(long)]
    pub check_replicas: bool,
}

/// Turns on the blobstore's scrub reads for `--check-replicas`, in report-only
/// mode: Strider has no option that repairs.
pub struct ReplicaCheckAppExtension;

impl AppExtension for ReplicaCheckAppExtension {
    type Args = ReplicaCheckArgs;

    fn environment_hook(
        &self,
        args: &ReplicaCheckArgs,
        env: &mut MononokeEnvironment,
    ) -> Result<()> {
        if args.check_replicas {
            env.blobstore_options.set_scrub_options(scrub_options());
        }
        Ok(())
    }
}

fn scrub_options() -> ScrubOptions {
    ScrubOptions {
        scrub_action: ScrubAction::ReportOnly,
        scrub_grace: Some(SCRUB_GRACE),
        // As in Walker: a store still being filled is not reported.
        scrub_action_on_missing_write_only: SrubWriteOnly::SkipMissing,
        ..ScrubOptions::default()
    }
}

/// Receives each missing copy from the blobstore, counts it and logs it to
/// Scuba as Walker does, and logs it with the commit and object whose load
/// read it.
#[derive(Debug)]
pub struct ReplicaChecker {
    stats: StatsScrubHandler,
    tags: Arc<LoadTags>,
    repo_name: String,
    /// Missing copies read by a commit's load.
    attributed: AtomicU64,
    /// Missing copies read outside any commit's load: opening the
    /// repository, or a load whose tag is gone. They are counted and logged
    /// to Scuba, but belong to no commit.
    unattributed: AtomicU64,
}

impl ReplicaChecker {
    pub fn new(
        tags: Arc<LoadTags>,
        scuba: MononokeScubaSampleBuilder,
        repo_id: RepositoryId,
        repo_name: String,
    ) -> Self {
        Self {
            // Quiet: each gap is logged below, with its commit when known.
            stats: StatsScrubHandler::new(
                true,
                scuba,
                SCRUB_STATS_KEY,
                HashMap::from([(repo_id, repo_name.clone())]),
            ),
            tags,
            repo_name,
            attributed: AtomicU64::new(0),
            unattributed: AtomicU64::new(0),
        }
    }

    pub fn attributed(&self) -> u64 {
        self.attributed.load(Ordering::Relaxed)
    }

    pub fn unattributed(&self) -> u64 {
        self.unattributed.load(Ordering::Relaxed)
    }

    /// The commit and tag of the load that `ctx` belongs to, while it runs.
    /// The multiplex reports a missing copy from inside that load's read.
    fn commit_load(&self, ctx: &CoreContext) -> Option<(ChangesetId, LoadTag)> {
        let tag = self.tags.get(ctx.sampling_key()?)?;
        Some((tag.changeset_id?, tag))
    }
}

impl ScrubHandler for ReplicaChecker {
    fn on_repair(
        &self,
        ctx: &CoreContext,
        blobstore_id: BlobstoreId,
        key: &str,
        is_repaired: bool,
        meta: &BlobstoreMetadata,
    ) {
        self.stats
            .on_repair(ctx, blobstore_id, key, is_repaired, meta);

        let Some((changeset_id, tag)) = self.commit_load(ctx) else {
            self.unattributed.fetch_add(1, Ordering::Relaxed);
            warn!(
                repo = self.repo_name,
                store = %blobstore_id,
                key,
                "Strider replica gap outside any commit's load"
            );
            return;
        };
        self.attributed.fetch_add(1, Ordering::Relaxed);
        warn!(
            repo = self.repo_name,
            changeset_id = %changeset_id,
            object = %tag.object,
            store = %blobstore_id,
            key,
            "Strider replica gap"
        );
    }
}

#[cfg(test)]
mod tests {
    use fbinit::FacebookInit;
    use memblob::KeyedMemblob;
    use mononoke_macros::mononoke;
    use mononoke_types::audit::LoadContext;
    use mononoke_types::audit::ObjectId;
    use mononoke_types::hash::Blake2;
    use tests_utils::BasicTestRepo;

    use super::*;

    fn changeset(byte: u8) -> ChangesetId {
        ChangesetId::new(Blake2::from_byte_array([byte; 32]))
    }

    fn object(byte: u8) -> ObjectId {
        ObjectId::of::<BasicTestRepo, _>(&changeset(byte))
    }

    fn checker(tags: &Arc<LoadTags>) -> ReplicaChecker {
        ReplicaChecker::new(
            tags.clone(),
            MononokeScubaSampleBuilder::with_discard(),
            RepositoryId::new(0),
            "repo".to_string(),
        )
    }

    fn load_ctx(ctx: &CoreContext, tags: &Arc<LoadTags>) -> LoadContext {
        LoadContext::new(ctx.clone(), Arc::new(KeyedMemblob::default()), tags.clone())
    }

    #[mononoke::test]
    fn scrub_options_report_only_after_the_grace_period() {
        let options = scrub_options();
        assert_eq!(options.scrub_action, ScrubAction::ReportOnly);
        assert_eq!(options.scrub_grace, Some(Duration::from_secs(120)));
    }

    #[mononoke::fbinit_test]
    async fn gaps_belong_to_the_commit_and_object_whose_load_read_them(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given a load of an object while auditing commit 1
        let ctx = CoreContext::test_mock(fb);
        let tags = Arc::new(LoadTags::default());
        let checker = checker(&tags);
        let load = load_ctx(&ctx, &tags)
            .with_changeset_id(changeset(1))
            .tag(object(2));

        // When one store lacks a key the load read
        checker.on_repair(
            &load,
            BlobstoreId::new(0),
            "a",
            false,
            &BlobstoreMetadata::default(),
        );

        // Then the gap is commit 1's, for that object
        let (changeset_id, tag) = checker.commit_load(&load).expect("the load is live");
        assert_eq!(changeset_id, changeset(1));
        assert_eq!(tag.object, object(2));
        assert_eq!((checker.attributed(), checker.unattributed()), (1, 0));
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn gaps_outside_a_commits_load_are_unattributed(fb: FacebookInit) -> Result<()> {
        // Given a context with no sampling key, as when opening the
        // repository, and a load tagged with no commit
        let ctx = CoreContext::test_mock(fb);
        let tags = Arc::new(LoadTags::default());
        let checker = checker(&tags);
        let untracked = load_ctx(&ctx, &tags).tag(object(2));

        // When one store lacks a key read by each
        let meta = BlobstoreMetadata::default();
        checker.on_repair(&ctx, BlobstoreId::new(0), "a", false, &meta);
        checker.on_repair(&untracked, BlobstoreId::new(0), "b", false, &meta);

        // Then both are counted, and neither belongs to a commit
        assert_eq!((checker.attributed(), checker.unattributed()), (0, 2));
        Ok(())
    }
}
