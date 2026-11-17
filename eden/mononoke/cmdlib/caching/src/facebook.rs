/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::cmp::max;
use std::cmp::min;
use std::fs::OpenOptions;
use std::time::Duration;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::bail;
use fbinit::FacebookInit;

use super::settings::CachelibSettings;

const MIN_BUCKET_POWER: u32 = 20;

pub fn init_cachelib_from_settings(
    fb: FacebookInit,
    settings: CachelibSettings,
    enable_cacheadmin: bool,
) -> Result<()> {
    // Millions of lookups per second
    let lock_power = 10;

    let expected_item_size_bytes = settings.expected_item_size_bytes.unwrap_or(200);
    let cache_size_bytes = settings.cache_size;
    let item_count = cache_size_bytes / expected_item_size_bytes;

    let buckets_power = if let Some(buckets_power) = settings.buckets_power {
        max(buckets_power, MIN_BUCKET_POWER)
    } else {
        // Because `bucket_count` is a power of 2, bucket_count.trailing_zeros() is log2(bucket_count)
        let bucket_count = item_count
            .checked_next_power_of_two()
            .ok_or_else(|| Error::msg("Cache has too many objects to fit a `usize`?!?"))?;

        min(bucket_count.trailing_zeros() + 1_u32, 32)
    };

    let strategy = if settings.rebalancing_use_lru {
        cachelib::RebalanceStrategy::LruTailAge {
            // Defaults from cachelib (cachelib/allocator/LruTailAgeStrategy.h):
            age_difference_ratio: 0.25,
            min_retained_slabs: 1,
        }
    } else {
        cachelib::RebalanceStrategy::HitsPerSlab {
            // A small increase in hit ratio is desired
            diff_ratio: 0.05,
            min_retained_slabs: 1,
            // Objects newer than 30 seconds old might be about to become interesting
            min_tail_age: Duration::new(30, 0),
        }
    };

    let mut cache_config = cachelib::LruCacheConfig::new(cache_size_bytes)
        .set_pool_rebalance(cachelib::PoolRebalanceConfig {
            interval: settings.rebalancing_interval,
            strategy,
        })
        .set_access_config(buckets_power, lock_power)
        .set_cache_name("mononoke");

    if let Some(cache_file_path) = settings.cache_file_path {
        if let Some(directory) = cache_file_path.parent() {
            std::fs::create_dir_all(directory)
                .with_context(|| "Failed to create cachelib cache directory")?;
        }
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&cache_file_path)
            .with_context(|| "Failed to create cachelib cache file")?;

        cache_config = cache_config.set_nvm_cache_config(cachelib::NvmCacheConfig {
            // Default values from cachelib codebase
            block_size: 4096,
            region_size: 16 * 1024 * 1024,
            device: cachelib::NvmDevice::SimpleFile {
                file_name: cache_file_path,
                file_size: settings.cache_file_size,
                truncate: true,
            },
        });
    }

    if settings.use_tupperware_shrinker {
        if settings.max_process_size_gib.is_some() || settings.min_process_size_gib.is_some() {
            bail!("Can't use both Tupperware shrinker and manually configured shrinker");
        }
        cache_config = cache_config.set_container_shrinker();
    } else {
        match (settings.max_process_size_gib, settings.min_process_size_gib) {
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) => {
                bail!("If setting process size limits, must set both max and min");
            }
            (Some(max), Some(min)) if min > max => {
                bail!("Max process size cannot be smaller than min process size")
            }
            (Some(max), Some(min)) => {
                cache_config = cache_config.set_shrinker(cachelib::ShrinkMonitor {
                    shrinker_type: cachelib::ShrinkMonitorType::ResidentSize {
                        max_process_size_gib: max,
                        min_process_size_gib: min,
                    },
                    interval: Duration::new(10, 0),
                    max_resize_per_iteration_percent: 25,
                    max_removed_percent: 50,
                    strategy,
                });
            }
        };
    }

    cache_config = cache_config.set_empty_pool_recovery(settings.empty_pool_recovery);

    if enable_cacheadmin {
        cachelib::init_cache_with_cacheadmin(fb, cache_config, "scm_server_infra")?;
    } else {
        cachelib::init_cache(fb, cache_config)?;
    }

    // Give each cache 5% of the available space, bar the blob cache which gets everything left
    // over. We can adjust this with data.
    let available_space = cachelib::get_available_space()?;
    cachelib::get_or_create_volatile_pool(
        "blobstore-presence",
        settings.presence_cache_size.unwrap_or(available_space / 20),
    )?;

    cachelib::get_or_create_volatile_pool(
        "commit_graph",
        settings
            .commit_graph_cache_size
            .unwrap_or(available_space / 20),
    )?;
    cachelib::get_or_create_volatile_pool(
        "commit_derived_data_mapping",
        settings
            .commit_derived_data_mapping_cache_size
            .unwrap_or(32 * 1024 * 1024),
    )?;
    cachelib::get_or_create_volatile_pool(
        "filenodes",
        settings
            .filenodes_cache_size
            .unwrap_or(available_space / 20),
    )?;
    cachelib::get_or_create_volatile_pool(
        "filenodes_history",
        settings
            .filenodes_history_cache_size
            .unwrap_or(available_space / 20),
    )?;
    cachelib::get_or_create_volatile_pool(
        "hg_mutation_store",
        settings
            .hg_mutation_store_cache_size
            .unwrap_or(available_space / 20),
    )?;
    cachelib::get_or_create_volatile_pool(
        "bonsai_hg_mapping",
        settings
            .idmapping_cache_size
            .unwrap_or(available_space / 20),
    )?;
    cachelib::get_or_create_volatile_pool(
        "bonsai_git_mapping",
        settings.bonsai_git_mapping_cache_size.unwrap_or(1024),
    )?;
    // Defaults to a very small cache. Jobs that need it can increase its size.
    cachelib::get_or_create_volatile_pool(
        "bonsai_globalrev_mapping",
        settings.globalrev_cache_size.unwrap_or(1024),
    )?;

    // Defaults to a very small cache. Jobs that need it can increase its size.
    cachelib::get_or_create_volatile_pool(
        "bonsai_svnrev_mapping",
        settings.svnrev_cache_size.unwrap_or(1024),
    )?;

    // Defaults to a very small cache. Jobs that need it can increase its size.
    cachelib::get_or_create_volatile_pool(
        "bonsai_p4_mapping",
        settings.p4_changelist_cache_size.unwrap_or(1024),
    )?;

    cachelib::get_or_create_volatile_pool(
        "phases",
        settings.phases_cache_size.unwrap_or(available_space / 20),
    )?;

    cachelib::get_or_create_volatile_pool(
        "mutable_renames",
        settings
            .mutable_renames_cache_size
            .unwrap_or(available_space / 20),
    )?;

    cachelib::get_or_create_volatile_pool(
        "sql",
        settings.sql_cache_size.unwrap_or(32 * 1024 * 1024),
    )?;

    cachelib::get_or_create_volatile_pool(
        "synced_commit_mapping",
        settings
            .synced_commit_mapping_cache_size
            .unwrap_or(32 * 1024 * 1024),
    )?;

    cachelib::get_or_create_volatile_pool(
        "blobstore-blobs",
        settings
            .blob_cache_size
            .unwrap_or(cachelib::get_available_space()?),
    )?;

    Ok(())
}
