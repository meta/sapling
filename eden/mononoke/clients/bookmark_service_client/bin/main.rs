/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use bookmark_service_client::RepoBookmarkServiceClient;
use bookmarks_cache::BookmarksCache;
use bookmarks_cache::WarmerRequirement;
use bookmarks_types::BookmarkKey;
use bookmarks_types::BookmarkPagination;
use bookmarks_types::BookmarkPrefix;
use clap::Parser;
use clap::ValueEnum;
use clientinfo::ClientEntryPoint;
use clientinfo::ClientInfo;
use cmdlib_logging::LoggingArgs;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::TryFutureExt;
use prettytable::Table;
use prettytable::row;

#[cfg(fbcode_build)]
mod facebook;
#[cfg(not(fbcode_build))]
mod oss;

#[cfg(fbcode_build)]
use facebook as platform;
#[cfg(not(fbcode_build))]
use oss as platform;

/// CLI argument enum for warmer requirement selection.
/// Maps to bookmarks_cache::WarmerRequirement.
#[derive(Debug, Clone, Copy, ValueEnum)]
#[clap(rename_all = "kebab-case")]
enum CliWarmerRequirement {
    /// Only wait for Hg-related derived data
    HgOnly,
    /// Only wait for Git-related derived data
    GitOnly,
    /// Wait for all configured derived data types
    AllKinds,
}

impl From<CliWarmerRequirement> for WarmerRequirement {
    fn from(cli: CliWarmerRequirement) -> Self {
        match cli {
            CliWarmerRequirement::HgOnly => WarmerRequirement::HgOnly,
            CliWarmerRequirement::GitOnly => WarmerRequirement::GitOnly,
            CliWarmerRequirement::AllKinds => WarmerRequirement::AllKinds,
        }
    }
}

#[derive(Debug, Parser)]
#[clap(about = "Thin CLI for the Bookmark Service (used for testing and debug)")]
struct BookmarkServiceClientArgs {
    #[arg(long)]
    host: Option<String>,
    #[clap(long, conflicts_with = "host", conflicts_with = "sm_tier")]
    tier: Option<String>,
    #[clap(long, conflicts_with = "host", conflicts_with = "tier")]
    sm_tier: Option<String>,
    #[clap(long, value_enum, default_value_t = CliWarmerRequirement::HgOnly)]
    warmer_requirement: CliWarmerRequirement,
    #[clap(flatten, next_help_heading = "LOGGING OPTIONS")]
    logging_args: LoggingArgs,
    #[structopt(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    Get {
        #[arg(long, short = 'R')]
        repo_name: String,
        bookmark_name: String,
    },
    List {
        #[arg(long, short = 'R')]
        repo_name: String,
        #[arg(long, default_value = "")]
        prefix: String,
        #[arg(long)]
        after: Option<String>,
        #[arg(long, short)]
        limit: Option<u64>,
    },
    Status,
}

#[fbinit::main]
async fn main(fb: FacebookInit) -> Result<()> {
    let args = BookmarkServiceClientArgs::parse();
    args.logging_args.setup_logging(fb)?;
    platform::setup(fb);

    let ctx = CoreContext::new_with_client_info(
        fb,
        ClientInfo::default_with_entry_point(ClientEntryPoint::BookmarkServiceClientCli),
    );

    let client = platform::connect(fb, &args)?;

    let warmer_requirement: WarmerRequirement = args.warmer_requirement.into();

    match args.command {
        Command::Get {
            repo_name,
            bookmark_name,
        } => {
            let repo_client = RepoBookmarkServiceClient::new(repo_name, client, warmer_requirement);
            let bookmark_name = BookmarkKey::new(bookmark_name)?;
            let resp = repo_client.get(&ctx, &bookmark_name).await?;
            println!(
                "{}",
                resp.map_or("bookmark not found".into(), |x| x.to_string())
            );
            Ok(())
        }
        Command::List {
            repo_name,
            prefix,
            after,
            limit,
        } => {
            let repo_client = RepoBookmarkServiceClient::new(repo_name, client, warmer_requirement);
            let prefix = BookmarkPrefix::new(prefix)?;
            let pagination = match after {
                None => BookmarkPagination::FromStart,
                Some(after) => BookmarkPagination::After(after.parse()?),
            };
            let mut resp = repo_client.list(&ctx, &prefix, &pagination, limit).await?;
            resp.sort_by_key(|x| x.0.clone());

            let mut table = Table::new();
            table.add_row(row!["Bookmark", "Hash", "Kind"]);
            for (bookmark, (hash, kind)) in resp {
                table.add_row(row![bookmark, hash.to_hex(), kind]);
            }
            table.printstd();
            Ok(())
        }
        Command::Status => client.status().map_ok(|_| ()).await,
    }
}
