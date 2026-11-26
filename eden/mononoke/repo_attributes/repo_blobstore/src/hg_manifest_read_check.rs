/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Finds code that still reads Hg manifests in a repo that reads augmented
//! manifests instead.
//!
//! With `scm/mononoke:hgmanifest_read_from_augmented` on for a repo, only Hg
//! changeset derivation and its pipeline, the backfill of augmented manifests
//! from Hg, and augmented manifest derivation from a commit's mapped Hg
//! manifest may read Hg manifests, and they read through a context carrying an
//! access label (`CoreContext::with_access_label`). Any other read comes from
//! a reader that was missed, and it would break once Hg manifests stop being
//! written. This check counts labelled reads by label, and counts and logs
//! unlabelled ones, so those readers can be found. It also counts every write
//! of an Hg manifest by label and operation, labelled or not, and logs a
//! sample of them, so that it can show when Hg manifest writes have stopped
//! and name any writer left.

use std::num::NonZeroU64;

use context::CoreContext;
use stats::prelude::*;

/// The key prefix of Hg manifest blobs, as `HgManifestId::blobstore_key`
/// builds them.
const HG_MANIFEST_KEY_PREFIX: &str = "hgmanifest.sha1.";

/// One unlabelled read in this many is logged to Scuba. A low rate, so that a
/// reader making only a few reads is still likely to be logged.
const UNLABELLED_READ_SAMPLE_RATE: NonZeroU64 = NonZeroU64::new(10).expect("10 is not zero");

/// One Hg manifest write in this many is logged to Scuba. Until Hg manifest
/// writes stop, every new commit and every uploaded tree writes them, so the
/// rate is much lower than for unlabelled reads.
const WRITE_SAMPLE_RATE: NonZeroU64 = NonZeroU64::new(1000).expect("1000 is not zero");

/// The label value of writes made without an access label.
const UNLABELLED: &str = "unlabelled";

define_stats! {
    prefix = "mononoke.hg_manifest_read_check";
    unlabelled_reads: dynamic_timeseries("{}.unlabelled_reads", (repo: String); Rate, Sum),
    labelled_reads: dynamic_timeseries(
        "{}.labelled_reads.{}",
        (repo: String, label: &'static str);
        Rate, Sum
    ),
    writes: dynamic_timeseries(
        "{}.writes.{}.{}",
        (repo: String, label: &'static str, operation: &'static str);
        Rate, Sum
    ),
}

#[derive(Debug)]
pub(crate) struct HgManifestReadCheck {
    repo_name: String,
    sample_rate: NonZeroU64,
    write_sample_rate: NonZeroU64,
}

impl HgManifestReadCheck {
    pub(crate) fn new(repo_name: String) -> Self {
        Self {
            repo_name,
            sample_rate: UNLABELLED_READ_SAMPLE_RATE,
            write_sample_rate: WRITE_SAMPLE_RATE,
        }
    }

    /// Check a write of `key`, a key without the repo prefix. Every write is
    /// counted and a sample is logged whatever its label, because Hg
    /// manifest writes are meant to stop altogether.
    pub(crate) fn check_write(&self, ctx: &CoreContext, key: &str, operation: &'static str) {
        if !self.is_checked(key) {
            return;
        }

        let label = ctx.access_label().unwrap_or(UNLABELLED);
        STATS::writes.add_value(1, (self.repo_name.clone(), label, operation));
        let mut scuba = ctx.scuba().clone();
        scuba
            .unsampled()
            .sampled(self.write_sample_rate)
            .add_metadata(ctx.metadata())
            .add("repo", self.repo_name.as_str())
            .add("blobstore_key", key)
            .add("blobstore_operation", operation)
            .add_opt("access_label", ctx.access_label());
        scuba.log_with_msg("Hg manifest write", None);
    }

    fn is_checked(&self, key: &str) -> bool {
        key.starts_with(HG_MANIFEST_KEY_PREFIX)
            && justknobs::eval(
                "scm/mononoke:hgmanifest_read_from_augmented",
                None,
                Some(&self.repo_name),
            )
    }

    /// Check a read of `key`, a key without the repo prefix.
    pub(crate) fn check_read(&self, ctx: &CoreContext, key: &str, operation: &str) {
        if !self.is_checked(key) {
            return;
        }

        if let Some(label) = ctx.access_label() {
            STATS::labelled_reads.add_value(1, (self.repo_name.clone(), label));
            return;
        }

        STATS::unlabelled_reads.add_value(1, (self.repo_name.clone(),));
        let mut scuba = ctx.scuba().clone();
        // Replace whatever sampling the request's own logging uses.
        scuba
            .unsampled()
            .sampled(self.sample_rate)
            .add_metadata(ctx.metadata())
            .add("repo", self.repo_name.as_str())
            .add("blobstore_key", key)
            .add("blobstore_operation", operation);
        scuba.log_with_msg("Unlabelled Hg manifest read", None);
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Arc;

    use anyhow::Result;
    use blobstore::Blobstore;
    use blobstore::BlobstoreBytes;
    use blobstore::KeyedBlobstore;
    use fbinit::FacebookInit;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::override_just_knobs;
    use memblob::Memblob;
    use mononoke_macros::mononoke;
    use mononoke_types::RepositoryId;
    use scuba_ext::MononokeScubaSampleBuilder;
    use serde_json::Value;
    use tempfile::NamedTempFile;

    use super::*;
    use crate::RepoBlobstore;

    const HG_MANIFEST_KEY: &str = "hgmanifest.sha1.0123";
    const OTHER_KEY: &str = "hgfilenode.sha1.0123";
    const NEW_HG_MANIFEST_KEY: &str = "hgmanifest.sha1.4567";
    const COPIED_HG_MANIFEST_KEY: &str = "hgmanifest.sha1.89ab";

    fn set_read_from_augmented(on: bool) {
        override_just_knobs(JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:hgmanifest_read_from_augmented".to_string(),
            KnobVal::Bool(on),
        )])));
    }

    /// A checked repo blobstore that logs every unlabelled read, holding an
    /// Hg manifest and a blob of another type, and a context whose Scuba rows
    /// go to the returned file.
    async fn checked_repo_blobstore(
        fb: FacebookInit,
    ) -> Result<(RepoBlobstore, CoreContext, NamedTempFile)> {
        let log = NamedTempFile::new()?;
        let scuba = MononokeScubaSampleBuilder::with_discard().with_log_file(log.path())?;
        let ctx = CoreContext::new_with_scuba(fb, scuba);
        let storage = Memblob::default();
        for key in [HG_MANIFEST_KEY, OTHER_KEY] {
            storage
                .put(
                    &ctx,
                    format!("{}{key}", RepositoryId::new(1).prefix()),
                    BlobstoreBytes::from_bytes("blob"),
                )
                .await?;
        }
        let repo_blobstore = RepoBlobstore {
            hg_manifest_read_check: Some(Arc::new(HgManifestReadCheck {
                repo_name: "repo".to_string(),
                sample_rate: NonZeroU64::MIN,
                write_sample_rate: NonZeroU64::MIN,
            })),
            ..RepoBlobstore::new(
                Arc::new(storage),
                None,
                RepositoryId::new(1),
                MononokeScubaSampleBuilder::with_discard(),
            )
        };
        Ok((repo_blobstore, ctx, log))
    }

    async fn read_both_ways(
        ctx: &CoreContext,
        repo_blobstore: &RepoBlobstore,
        key: &str,
    ) -> Result<()> {
        assert!(repo_blobstore.get(ctx, key).await?.is_some());
        assert!(
            repo_blobstore
                .is_present(ctx, key)
                .await?
                .assume_not_found_if_unsure()
        );
        Ok(())
    }

    /// Writes a new Hg manifest, a blob of another type, and copies of each.
    async fn write_all_ways(ctx: &CoreContext, repo_blobstore: &RepoBlobstore) -> Result<()> {
        for (key, copy_key) in [
            (NEW_HG_MANIFEST_KEY, COPIED_HG_MANIFEST_KEY),
            ("hgfilenode.sha1.4567", "hgfilenode.sha1.89ab"),
        ] {
            repo_blobstore
                .put(ctx, key.to_string(), BlobstoreBytes::from_bytes("blob"))
                .await?;
            repo_blobstore.copy(ctx, key, copy_key.to_string()).await?;
        }
        Ok(())
    }

    fn logged_rows(log: &NamedTempFile) -> Result<Vec<Value>> {
        fs::read_to_string(log.path())?
            .lines()
            .map(|line| Ok(serde_json::from_str(line)?))
            .collect()
    }

    #[mononoke::fbinit_test]
    async fn test_switch_off_reads_are_not_logged(fb: FacebookInit) -> Result<()> {
        // Given: a repo that still reads Hg manifests.
        set_read_from_augmented(false);
        let (repo_blobstore, ctx, log) = checked_repo_blobstore(fb).await?;

        // When: an unlabelled context reads an Hg manifest.
        read_both_ways(&ctx, &repo_blobstore, HG_MANIFEST_KEY).await?;

        // Then: the reads pass and are not logged.
        assert!(logged_rows(&log)?.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_labelled_reads_are_not_logged(fb: FacebookInit) -> Result<()> {
        // Given: a repo that reads augmented manifests.
        set_read_from_augmented(true);
        let (repo_blobstore, ctx, log) = checked_repo_blobstore(fb).await?;

        // When: a labelled context reads an Hg manifest.
        let labelled = ctx.with_access_label("allowed_reader");
        read_both_ways(&labelled, &repo_blobstore, HG_MANIFEST_KEY).await?;

        // Then: the reads pass and are not logged.
        assert!(logged_rows(&log)?.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_unlabelled_reads_are_logged(fb: FacebookInit) -> Result<()> {
        // Given: a repo that reads augmented manifests.
        set_read_from_augmented(true);
        let (repo_blobstore, ctx, log) = checked_repo_blobstore(fb).await?;

        // When: an unlabelled context gets an Hg manifest, then checks that
        // it is present.
        read_both_ways(&ctx, &repo_blobstore, HG_MANIFEST_KEY).await?;

        // Then: both reads pass and are logged, with the repo, the key, the
        // operation and the session, which ties each row to the request that
        // made the read.
        let rows = logged_rows(&log)?;
        let logged: Vec<_> = rows
            .iter()
            .map(|row| {
                let normal = &row["normal"];
                (
                    normal["repo"].as_str(),
                    normal["blobstore_key"].as_str(),
                    normal["blobstore_operation"].as_str(),
                    normal["session_uuid"].as_str(),
                )
            })
            .collect();
        let session = ctx.metadata().session_id().to_string();
        assert_eq!(
            logged,
            vec![
                (
                    Some("repo"),
                    Some(HG_MANIFEST_KEY),
                    Some("get"),
                    Some(session.as_str())
                ),
                (
                    Some("repo"),
                    Some(HG_MANIFEST_KEY),
                    Some("is_present"),
                    Some(session.as_str())
                ),
            ],
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_wrapped_copies_keep_the_check(fb: FacebookInit) -> Result<()> {
        // Given: a repo that reads augmented manifests, a copy of its
        // blobstore over other storage holding an Hg manifest the repo's
        // storage lacks, like one a bubble serves from its own storage, and a
        // copy that only logs redacted reads.
        set_read_from_augmented(true);
        let (repo_blobstore, ctx, log) = checked_repo_blobstore(fb).await?;
        let other_storage = Memblob::default();
        let other_key = "hgmanifest.sha1.4567";
        other_storage
            .put(
                &ctx,
                format!("{}{other_key}", RepositoryId::new(1).prefix()),
                BlobstoreBytes::from_bytes("blob"),
            )
            .await?;
        let over_other_storage =
            RepoBlobstore::new_with_wrapped_inner_blobstore(repo_blobstore.clone(), |_storage| {
                Arc::new(other_storage)
            });
        let log_only_redaction = repo_blobstore.with_log_only_redaction();

        // When: an unlabelled context reads an Hg manifest through each copy.
        read_both_ways(&ctx, &over_other_storage, other_key).await?;
        read_both_ways(&ctx, &log_only_redaction, HG_MANIFEST_KEY).await?;

        // Then: every read is logged.
        assert_eq!(logged_rows(&log)?.len(), 4);
        Ok(())
    }

    #[mononoke::test]
    fn test_checked_repos_sharing_storage_keep_the_fast_copy() {
        // Given: two repos with checked blobstores over one storage.
        let storage: Arc<dyn Blobstore> = Arc::new(Memblob::default());
        let checked = |id, name| {
            RepoBlobstore::new(
                storage.clone(),
                None,
                RepositoryId::new(id),
                MononokeScubaSampleBuilder::with_discard(),
            )
            .with_hg_manifest_read_check(name)
        };
        let (source, target) = (checked(1, "source"), checked(2, "target"));

        // When/Then: copying between them still uses the storage's own copy.
        assert!(source.copier_to(&target).is_optimized());
    }

    #[mononoke::fbinit_test]
    async fn test_other_keys_are_not_logged(fb: FacebookInit) -> Result<()> {
        // Given: a repo that reads augmented manifests.
        set_read_from_augmented(true);
        let (repo_blobstore, ctx, log) = checked_repo_blobstore(fb).await?;

        // When: an unlabelled context reads a blob that is not an Hg manifest.
        read_both_ways(&ctx, &repo_blobstore, OTHER_KEY).await?;

        // Then: the reads pass and are not logged.
        assert!(logged_rows(&log)?.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_writes_are_logged(fb: FacebookInit) -> Result<()> {
        // Given: a repo that reads augmented manifests.
        set_read_from_augmented(true);
        let (repo_blobstore, ctx, log) = checked_repo_blobstore(fb).await?;

        // When: a labelled context writes and copies an Hg manifest and a
        // blob of another type.
        let labelled = ctx.with_access_label("allowed_writer");
        write_all_ways(&labelled, &repo_blobstore).await?;

        // Then: only the Hg manifest writes are logged, each as a write with
        // the repo, the key it wrote, the operation, the label and the
        // session. The label does not exempt a write.
        let rows = logged_rows(&log)?;
        let logged: Vec<_> = rows
            .iter()
            .map(|row| {
                let normal = &row["normal"];
                (
                    normal["log_tag"].as_str(),
                    normal["repo"].as_str(),
                    normal["blobstore_key"].as_str(),
                    normal["blobstore_operation"].as_str(),
                    normal["access_label"].as_str(),
                    normal["session_uuid"].as_str(),
                )
            })
            .collect();
        let session = ctx.metadata().session_id().to_string();
        let write = |key, operation| {
            (
                Some("Hg manifest write"),
                Some("repo"),
                Some(key),
                Some(operation),
                Some("allowed_writer"),
                Some(session.as_str()),
            )
        };
        assert_eq!(
            logged,
            vec![
                write(NEW_HG_MANIFEST_KEY, "put"),
                write(COPIED_HG_MANIFEST_KEY, "copy"),
            ],
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_switch_off_writes_are_not_logged(fb: FacebookInit) -> Result<()> {
        // Given: a repo that still reads Hg manifests.
        set_read_from_augmented(false);
        let (repo_blobstore, ctx, log) = checked_repo_blobstore(fb).await?;

        // When: an unlabelled context writes and copies an Hg manifest.
        write_all_ways(&ctx, &repo_blobstore).await?;

        // Then: the writes succeed and are not logged.
        assert!(
            repo_blobstore
                .get(&ctx, COPIED_HG_MANIFEST_KEY)
                .await?
                .is_some()
        );
        assert!(logged_rows(&log)?.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_wrapped_copies_keep_the_write_check(fb: FacebookInit) -> Result<()> {
        // Given: a repo that reads augmented manifests, and a copy of its
        // blobstore over other storage, like a bubble's.
        set_read_from_augmented(true);
        let (repo_blobstore, ctx, log) = checked_repo_blobstore(fb).await?;
        let over_other_storage =
            RepoBlobstore::new_with_wrapped_inner_blobstore(repo_blobstore, |_storage| {
                Arc::new(Memblob::default())
            });

        // When: an unlabelled context writes an Hg manifest through the copy.
        over_other_storage
            .put(
                &ctx,
                NEW_HG_MANIFEST_KEY.to_string(),
                BlobstoreBytes::from_bytes("blob"),
            )
            .await?;

        // Then: the write is logged.
        assert_eq!(logged_rows(&log)?.len(), 1);
        Ok(())
    }
}
