/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Configure various libraries
//!
//! After loading the config files, configure related libraries accordingly.
//!
//! Note: this only affects `sl` CLI. For EdenFS, you might also need to update
//! `lib/backingstore`.
//!
//! Some libraries take Sapling's `Config` trait for configuration. Some
//! libraries like `hgtime` are low-level and shouldn't depend on Sapling's
//! `Config` trait. Their configuration logic is here.
//!
//! For initialization that does not need a `config`.
//! Check `lib/constructors` - it affects both `sl` and `eden`
//! (via `lib/backingstore`).

use std::sync::atomic::Ordering;

use configmodel::Config;
use configmodel::ConfigExt;
use hgtime::HgTime;
use io::IO;
use io::IsTty;
use repo::CoreRepo;

use crate::OptionalRepo;
use crate::dispatch::Dispatcher;
use crate::dispatch::Result;

impl Dispatcher {
    pub(crate) fn configure_libraries(&self, io: &IO) -> Result<()> {
        let config = self.config();
        indexedlog_config::configure(config)?;
        if let Some(limit) = config.get_opt::<usize>("dag", "idmap-cache-flush-limit")? {
            dag::config::set_idmap_cache_flush_limit(limit);
        }
        manifest_tree::config::set_load_parent_tree_entry(config.get_or(
            "treemanifest",
            "load-parent-tree-entry",
            || true,
        )?);
        gitcompat::GlobalGit::set_default_config(config);
        initialize_hgtime(config)?;
        initialize_blackbox(&self.optional_repo)?;

        let is_tty = io.output().is_tty();
        configure_working_copy(is_tty);

        Ok(())
    }
}

fn initialize_blackbox(optional_repo: &OptionalRepo) -> Result<()> {
    if let OptionalRepo::CoreRepo(CoreRepo::Disk(repo)) = optional_repo {
        let config = repo.config();

        if !config.get_or("blackbox", "enable", || true)? {
            tracing::debug!("blackbox disabled");
            return Ok(());
        }

        let max_size = config
            .get_or("blackbox", "maxsize", || {
                configloader::convert::ByteCount::from(1u64 << 12)
            })?
            .value();
        let max_files = config.get_or("blackbox", "maxfiles", || 3)?;
        let path = repo.shared_dot_hg_path().join("blackbox/v1");
        if let Ok(blackbox) = ::blackbox::BlackboxOptions::new()
            .max_bytes_per_log(max_size)
            .max_log_count(max_files as u8)
            .open(path)
        {
            ::blackbox::init(blackbox);
        }
    }
    Ok(())
}

fn initialize_hgtime(config: &dyn Config) -> Result<()> {
    let mut should_clear = true;
    if let Some(now_str) = config.get("devel", "default-date") {
        let now_str = now_str.trim();
        if !now_str.is_empty() {
            if let Some(now) = HgTime::parse(now_str) {
                tracing::info!(?now, "set 'now' for testing");
                now.set_as_now_for_testing();
                should_clear = false;
            }
        }
    }
    if should_clear {
        tracing::debug!("unset 'now' for testing");
        HgTime::clear_now_for_testing();
    }
    Ok(())
}

fn configure_working_copy(is_tty: bool) {
    // Many automation  runs `sl status` without `SL_AUTOMATION=1`
    // (e.g. D97984219, D97218795). They can cause serious UX issues
    // like 40% user-typed `checkout` commands are failing
    // (e.g. https://fburl.com/workplace/avjspdkk).
    //
    // Let's just set `no-optional-locks` for non-tty users.
    if !is_tty && !is_test() {
        workingcopy::config::DOTGIT_NO_OPTIONAL_LOCKS.store(true, Ordering::Release);
    }
}

fn is_test() -> bool {
    std::env::var_os("TESTTMP").is_some()
}
