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
//! changeset derivation and the backfill of augmented manifests from Hg may
//! read Hg manifests, and they read through a context carrying an access label
//! (`CoreContext::with_access_label`). Any other read comes from a reader that
//! was missed, and it would break once Hg manifests stop being written. This
//! check counts labelled reads by label, and counts and logs unlabelled ones,
//! so those readers can be found.

use std::num::NonZeroU64;

use context::CoreContext;
use stats::prelude::*;

/// The key prefix of Hg manifest blobs, as `HgManifestId::blobstore_key`
/// builds them.
const HG_MANIFEST_KEY_PREFIX: &str = "hgmanifest.sha1.";

/// One unlabelled read in this many is logged to Scuba. A low rate, so that a
/// reader making only a few reads is still likely to be logged.
const UNLABELLED_READ_SAMPLE_RATE: NonZeroU64 = NonZeroU64::new(10).expect("10 is not zero");

define_stats! {
    prefix = "mononoke.hg_manifest_read_check";
    unlabelled_reads: dynamic_timeseries("{}.unlabelled_reads", (repo: String); Rate, Sum),
    labelled_reads: dynamic_timeseries(
        "{}.labelled_reads.{}",
        (repo: String, label: &'static str);
        Rate, Sum
    ),
}

#[derive(Debug)]
pub(crate) struct HgManifestReadCheck {
    repo_name: String,
    sample_rate: NonZeroU64,
}

impl HgManifestReadCheck {
    pub(crate) fn new(repo_name: String) -> Self {
        Self {
            repo_name,
            sample_rate: UNLABELLED_READ_SAMPLE_RATE,
        }
    }

    /// Check a read of `key`, a key without the repo prefix.
    pub(crate) fn check_read(&self, ctx: &CoreContext, key: &str, operation: &str) {
        if !key.starts_with(HG_MANIFEST_KEY_PREFIX)
            || !justknobs::eval(
                "scm/mononoke:hgmanifest_read_from_augmented",
                None,
                Some(&self.repo_name),
            )
        {
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
        let repo_blobstore = RepoBlobstore {
            hg_manifest_read_check: Some(Arc::new(HgManifestReadCheck {
                repo_name: "repo".to_string(),
                sample_rate: NonZeroU64::MIN,
            })),
            ..RepoBlobstore::new(
                Arc::new(Memblob::default()),
                None,
                RepositoryId::new(1),
                MononokeScubaSampleBuilder::with_discard(),
            )
        };
        for key in [HG_MANIFEST_KEY, OTHER_KEY] {
            repo_blobstore
                .put(&ctx, key.to_string(), BlobstoreBytes::from_bytes("blob"))
                .await?;
        }
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
}
