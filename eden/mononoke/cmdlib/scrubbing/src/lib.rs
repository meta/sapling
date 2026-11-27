/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod stats_handler;

use std::time::Duration;

use anyhow::Result;
use blobstore_factory::ScrubAction;
use blobstore_factory::ScrubOptions;
use blobstore_factory::SrubWriteOnly;
use clap::Args;
use environment::MononokeEnvironment;
use mononoke_app::AppExtension;
pub use stats_handler::ScrubStatsKey;
pub use stats_handler::StatsScrubHandler;
pub use stats_handler::zero_scrub_stats;

#[derive(Args, Debug)]
pub struct ScrubArgs {
    /// Enable ScrubBlobstore with the given action
    ///
    /// Checks for keys missing from the stores.  In ReportOnly mode, this
    /// only logs, otherwise it performs a copy to the missing stores.
    #[clap(long, help_heading = "BLOBSTORE OPTIONS")]
    pub blobstore_scrub_action: Option<ScrubAction>,

    /// Number of seconds grace to give for key to arrive in multiple
    /// blobstores or the healer queue when scrubbing
    #[clap(
        long,
        help_heading = "BLOBSTORE OPTIONS",
        requires = "blobstore_scrub_action"
    )]
    pub blobstore_scrub_grace: Option<u64>,

    /// Number of seconds within which we consider it worth checking the
    /// healer queue
    #[clap(
        long,
        help_heading = "BLOBSTORE OPTIONS",
        requires = "blobstore_scrub_action"
    )]
    pub blobstore_scrub_queue_peek_bound: Option<u64>,

    /// Whether to allow missing values from write-only stores when
    /// scrubbing
    #[clap(
        long,
        help_heading = "BLOBSTORE OPTIONS",
        requires = "blobstore_scrub_action"
    )]
    pub blobstore_scrub_write_only_missing: Option<SrubWriteOnly>,
}

#[derive(Default, Debug)]
pub struct ScrubAppExtension {
    pub action: Option<ScrubAction>,
    pub grace: Option<Duration>,
    pub queue_peek_bound: Option<Duration>,
    pub write_only_missing: Option<SrubWriteOnly>,
}

impl ScrubAppExtension {
    pub fn new() -> Self {
        ScrubAppExtension::default()
    }
}

impl AppExtension for ScrubAppExtension {
    type Args = ScrubArgs;

    fn arg_defaults(&self) -> Vec<(&'static str, String)> {
        let mut defaults = Vec::new();
        if let Some(action) = self.action {
            defaults.push((
                "blobstore_scrub_action",
                <&'static str>::from(action).to_string(),
            ));
        }
        if let Some(grace) = self.grace {
            defaults.push(("blobstore_scrub_grace", grace.as_secs().to_string()));
        }
        if let Some(queue_peek_bound) = self.queue_peek_bound {
            defaults.push((
                "blobstore_scrub_queue_peek_bound",
                queue_peek_bound.as_secs().to_string(),
            ));
        }
        if let Some(write_only_missing) = self.write_only_missing {
            defaults.push((
                "blobstore_scrub_write_only_missing",
                <&'static str>::from(write_only_missing).to_string(),
            ));
        }
        defaults
    }

    fn environment_hook(&self, args: &ScrubArgs, env: &mut MononokeEnvironment) -> Result<()> {
        if let Some(scrub_action) = args.blobstore_scrub_action {
            let mut scrub_options = ScrubOptions {
                scrub_action,
                ..ScrubOptions::default()
            };

            if let Some(scrub_grace) = args.blobstore_scrub_grace {
                scrub_options.scrub_grace = Some(Duration::from_secs(scrub_grace));
            }
            if let Some(action_on_missing) = args.blobstore_scrub_write_only_missing {
                scrub_options.scrub_action_on_missing_write_only = action_on_missing;
            }
            if let Some(queue_peek_bound) = args.blobstore_scrub_queue_peek_bound {
                scrub_options.queue_peek_bound = Duration::from_secs(queue_peek_bound);
            }
            env.blobstore_options.set_scrub_options(scrub_options);
        }
        Ok(())
    }
}
