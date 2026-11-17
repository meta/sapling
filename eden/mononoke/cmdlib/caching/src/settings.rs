/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::PathBuf;
use std::time::Duration;

use super::args::CachelibArgs;

const ONE_GIB: usize = 1 << 30; // 2 ^ 30 = 1 GiB

#[derive(Clone)]
pub struct CachelibSettings {
    pub cache_size: usize,
    pub max_process_size_gib: Option<u32>,
    pub min_process_size_gib: Option<u32>,
    pub buckets_power: Option<u32>,
    pub use_tupperware_shrinker: bool,
    /// Let the manual process-size shrinker grow back pools it has emptied.
    pub empty_pool_recovery: bool,
    pub presence_cache_size: Option<usize>,
    pub commit_graph_cache_size: Option<usize>,
    pub commit_derived_data_mapping_cache_size: Option<usize>,
    pub filenodes_cache_size: Option<usize>,
    pub filenodes_history_cache_size: Option<usize>,
    pub hg_mutation_store_cache_size: Option<usize>,
    pub idmapping_cache_size: Option<usize>,
    pub bonsai_git_mapping_cache_size: Option<usize>,
    pub globalrev_cache_size: Option<usize>,
    pub svnrev_cache_size: Option<usize>,
    pub p4_changelist_cache_size: Option<usize>,
    pub blob_cache_size: Option<usize>,
    pub phases_cache_size: Option<usize>,
    pub mutable_renames_cache_size: Option<usize>,
    pub sql_cache_size: Option<usize>,
    pub synced_commit_mapping_cache_size: Option<usize>,
    pub expected_item_size_bytes: Option<usize>,
    pub rebalancing_use_lru: bool,
    pub rebalancing_interval: Duration,
    pub cache_file_path: Option<PathBuf>,
    pub cache_file_size: u64,
}

impl CachelibSettings {
    pub fn arg_defaults(&self) -> Vec<(&'static str, String)> {
        let mut defaults = vec![
            ("cache_size_gb", (self.cache_size / ONE_GIB).to_string()),
            (
                "cache_file_size_gb",
                (self.cache_file_size as f64 / ONE_GIB as f64).to_string(),
            ),
            (
                "cachelib_rebalancing_interval_secs",
                self.rebalancing_interval.as_secs().to_string(),
            ),
        ];

        fn set_default<T: ToString>(
            defaults: &mut Vec<(&'static str, String)>,
            name: &'static str,
            value: &Option<T>,
        ) {
            if let Some(value) = value.as_ref() {
                defaults.push((name, value.to_string()));
            }
        }

        set_default(
            &mut defaults,
            "max-process-size",
            &self.max_process_size_gib,
        );
        set_default(
            &mut defaults,
            "min-process-size",
            &self.min_process_size_gib,
        );
        set_default(&mut defaults, "buckets-power", &self.buckets_power);
        set_default(
            &mut defaults,
            "presence-cache-size",
            &self.presence_cache_size,
        );
        set_default(
            &mut defaults,
            "commit-graph-cache-size",
            &self.commit_graph_cache_size,
        );
        set_default(
            &mut defaults,
            "commit-derived-data-mapping-cache-size",
            &self.commit_derived_data_mapping_cache_size,
        );
        set_default(
            &mut defaults,
            "filenodes-cache-size",
            &self.filenodes_cache_size,
        );
        set_default(
            &mut defaults,
            "filenodes-history-cache-size",
            &self.filenodes_history_cache_size,
        );
        set_default(
            &mut defaults,
            "hg-mutation-store-cache-size",
            &self.hg_mutation_store_cache_size,
        );
        set_default(
            &mut defaults,
            "idmapping-cache-size",
            &self.idmapping_cache_size,
        );
        set_default(
            &mut defaults,
            "bonsai-git-mapping-cache-size",
            &self.bonsai_git_mapping_cache_size,
        );
        set_default(
            &mut defaults,
            "globalrevs-cache-size",
            &self.globalrev_cache_size,
        );
        set_default(&mut defaults, "svnrevs-cache-size", &self.svnrev_cache_size);
        set_default(
            &mut defaults,
            "p4_changelists_cache_size",
            &self.p4_changelist_cache_size,
        );
        set_default(&mut defaults, "blob-cache-size", &self.blob_cache_size);
        set_default(&mut defaults, "phases-cache-size", &self.phases_cache_size);
        set_default(
            &mut defaults,
            "mutable-renames-cache-size",
            &self.mutable_renames_cache_size,
        );
        set_default(&mut defaults, "sql-cache-size", &self.sql_cache_size);
        set_default(
            &mut defaults,
            "synced-commit-mapping-cache-size",
            &self.synced_commit_mapping_cache_size,
        );
        set_default(
            &mut defaults,
            "cache-file-path",
            &self
                .cache_file_path
                .as_ref()
                .map(|path| path.clone().into_os_string().into_string().unwrap()),
        );

        defaults
    }

    pub fn update_from_args(&mut self, args: &CachelibArgs) {
        self.cache_size = (args.cache_size_gb * ONE_GIB as f64) as usize;
        self.cache_file_size = (args.cache_file_size_gb * ONE_GIB as f64) as u64;
        self.use_tupperware_shrinker = args.use_tupperware_shrinker;
        self.rebalancing_use_lru = args.cachelib_rebalancing_use_lru;
        self.rebalancing_interval = Duration::from_secs(args.cachelib_rebalancing_interval_secs);

        fn replace<T: Clone>(target: &mut Option<T>, value: &Option<T>) {
            if value.is_some() {
                *target = value.as_ref().cloned();
            }
        }

        replace(&mut self.max_process_size_gib, &args.max_process_size);
        replace(&mut self.min_process_size_gib, &args.min_process_size);
        replace(&mut self.buckets_power, &args.buckets_power);
        replace(&mut self.presence_cache_size, &args.presence_cache_size);
        replace(
            &mut self.commit_graph_cache_size,
            &args.commit_graph_cache_size,
        );
        replace(
            &mut self.commit_derived_data_mapping_cache_size,
            &args.commit_derived_data_mapping_cache_size,
        );
        replace(&mut self.filenodes_cache_size, &args.filenodes_cache_size);
        replace(
            &mut self.filenodes_history_cache_size,
            &args.filenodes_history_cache_size,
        );
        replace(
            &mut self.hg_mutation_store_cache_size,
            &args.hg_mutation_store_cache_size,
        );
        replace(&mut self.idmapping_cache_size, &args.idmapping_cache_size);
        replace(
            &mut self.bonsai_git_mapping_cache_size,
            &args.bonsai_git_mapping_cache_size,
        );
        replace(&mut self.globalrev_cache_size, &args.globalrevs_cache_size);
        replace(&mut self.svnrev_cache_size, &args.svnrevs_cache_size);
        replace(
            &mut self.p4_changelist_cache_size,
            &args.p4_changelists_cache_size,
        );
        replace(&mut self.blob_cache_size, &args.blob_cache_size);
        replace(&mut self.phases_cache_size, &args.phases_cache_size);
        replace(
            &mut self.mutable_renames_cache_size,
            &args.mutable_renames_cache_size,
        );
        replace(&mut self.sql_cache_size, &args.sql_cache_size);
        replace(
            &mut self.synced_commit_mapping_cache_size,
            &args.synced_commit_mapping_cache_size,
        );
        replace(&mut self.cache_file_path, &args.cache_file_path);
    }
}

impl Default for CachelibSettings {
    fn default() -> Self {
        Self {
            cache_size: 20 * ONE_GIB,
            max_process_size_gib: None,
            min_process_size_gib: None,
            buckets_power: None,
            use_tupperware_shrinker: false,
            empty_pool_recovery: false,
            presence_cache_size: None,
            commit_graph_cache_size: None,
            commit_derived_data_mapping_cache_size: None,
            filenodes_cache_size: None,
            filenodes_history_cache_size: None,
            hg_mutation_store_cache_size: None,
            idmapping_cache_size: None,
            bonsai_git_mapping_cache_size: None,
            globalrev_cache_size: None,
            svnrev_cache_size: None,
            p4_changelist_cache_size: None,
            blob_cache_size: None,
            phases_cache_size: None,
            mutable_renames_cache_size: None,
            sql_cache_size: None,
            synced_commit_mapping_cache_size: None,
            expected_item_size_bytes: None,
            rebalancing_use_lru: false,
            rebalancing_interval: Duration::from_mins(5),
            cache_file_path: None,
            cache_file_size: 32 * ONE_GIB as u64,
        }
    }
}

#[cfg(test)]
mod test {
    use clap::Args;
    use clap::Command;
    use mononoke_macros::mononoke;

    use super::*;
    use crate::CachelibArgs;

    /// Every default name must be the clap id of a real argument, or the app builder's
    /// `mut_arg` panics at startup.
    #[mononoke::test]
    fn test_p4_changelist_cache_size_default_names_a_real_arg() {
        let settings = CachelibSettings {
            p4_changelist_cache_size: Some(4096),
            ..Default::default()
        };
        let mut command = CachelibArgs::augment_args(Command::new("test"));
        for (name, default) in settings.arg_defaults() {
            command = command.mut_arg(name, |arg| arg.default_value(default));
        }
        let p4_default = command
            .get_arguments()
            .find(|arg| arg.get_id() == "p4_changelists_cache_size")
            .map(|arg| arg.get_default_values().to_vec());
        assert_eq!(p4_default, Some(vec!["4096".into()]));
    }
}
