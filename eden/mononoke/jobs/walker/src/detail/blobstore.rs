/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Error;
use anyhow::anyhow;
use cmdlib_scrubbing::ScrubStatsKey;
use cmdlib_scrubbing::zero_scrub_stats;
use metaconfig_types::BlobConfig;
use metaconfig_types::BlobstoreId;

/// The names of Walker's scrub counters for a subcommand.
pub fn scrub_stats_key(walk_stats_key: &'static str) -> ScrubStatsKey {
    ScrubStatsKey {
        tool: "walker",
        subcommand: walk_stats_key,
    }
}

pub fn replace_blobconfig(
    blob_config: &mut BlobConfig,
    inner_blobstore_id: Option<u64>,
    repo_name: &str,
    walk_stats_key: &'static str,
    is_scrubbing: bool,
) -> Result<(), Error> {
    match blob_config {
        &mut BlobConfig::MultiplexedWal { ref blobstores, .. } => {
            if is_scrubbing {
                zero_scrub_stats(
                    scrub_stats_key(walk_stats_key),
                    repo_name,
                    blobstores.iter().map(|(id, _ty, _config)| *id),
                );
            }
            if let Some(inner_blobstore_id) = inner_blobstore_id {
                let sought_id = BlobstoreId::new(inner_blobstore_id);
                let inner_blob_config = blobstores
                    .iter()
                    .find_map(|(blobstore_id, _, blobstore)| {
                        if blobstore_id == &sought_id {
                            Some(blobstore.clone())
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| {
                        anyhow!("could not find a blobstore with id {inner_blobstore_id}")
                    })?;
                *blob_config = inner_blob_config;
            }
        }
        _ => {
            if inner_blobstore_id.is_some() {
                return Err(anyhow!(
                    "inner-blobstore-id supplied but blobstore is not multiplexed"
                ));
            }
        }
    }
    Ok(())
}
