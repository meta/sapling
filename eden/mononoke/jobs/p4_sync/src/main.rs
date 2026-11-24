/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Perforce -> Mononoke mirror tailer: for each repo ShardManager assigns it, imports the
//! depot's new changelists in batches with `p4_import::import_next`, from the server the repo's
//! `p4_source.server_alias` names: its `--p4-server`, else its entry in the tier's `p4_servers`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use clap::Args;
use clap::Parser;
use clap::ValueEnum;
use executor_lib::args::ShardedExecutorArgs;
use fbinit::FacebookInit;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::MononokeService;
use mononoke_app::args::RepoFilterAppExtension;
use mononoke_app::args::TLSArgs;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use p4_import::ContentReuse;
use p4_import::ImportOptions;
use p4_import::UnreadableRevision;
use p4_source::Keychain;
use p4_source::P4Args;
use p4_source::P4AuthArgs;
use p4_source::P4Cli;
use p4_source::SecretReader;
use scuba_ext::MononokeScubaSampleBuilder;

mod repo;
mod server;
mod sharded;

use crate::repo::Repo;

/// Perforce -> Mononoke mirror tailer
#[derive(Parser)]
struct P4SyncArgs {
    #[clap(flatten)]
    sharded_executor_args: ShardedExecutorArgs,

    /// Update ODS counters
    #[clap(long)]
    log_to_ods: bool,

    /// Path to file to check for in order to exit
    #[clap(long)]
    exit_file: PathBuf,

    /// TLS parameters for this service
    #[clap(flatten)]
    tls_params: Option<TLSArgs>,

    /// Path to the `p4` client binary
    #[clap(long, help_heading = "PERFORCE OPTIONS")]
    p4_binary: PathBuf,

    /// Perforce user to connect as, on every `--p4-server`
    #[clap(long, help_heading = "PERFORCE OPTIONS")]
    p4_user: Option<String>,

    /// A Perforce server this job imports from, as `<alias>=<p4port>,<fingerprint>,<ticket-file>`:
    /// the alias as repo configs name it in `p4_source.server_alias` (e.g. `test1`), the
    /// server's address as `P4PORT` (e.g. `ssl6:[2001:db8::1]:1818`), its SSL fingerprint as
    /// `p4 trust` prints it, and a file holding a ticket for `--p4-user` on it. Once per
    /// server; it wins over the tier's `p4_servers` entry for the same alias. Shards of repos
    /// on any other server are held.
    #[clap(
        long = "p4-server",
        value_name = "ALIAS=P4PORT,FINGERPRINT,TICKET-FILE",
        value_parser = parse_server,
        requires = "p4_user",
        help_heading = "PERFORCE OPTIONS"
    )]
    p4_servers: Vec<P4Server>,

    /// A revision whose archive the Perforce admins have confirmed is gone from the server,
    /// as `<alias>:<change>:<depot path>#<rev>`, e.g.
    /// `p4main:7881://depot/dir/file.h#3`: the server's alias, the changelist, and the
    /// depot path exactly as `p4 describe` lists it. If the server cannot read that revision's
    /// archive, it is imported as a deletion of its path instead of failing the changelist.
    /// Once per revision; any other revision the server cannot read still fails.
    #[clap(
        long = "unreadable-revision",
        value_name = "ALIAS:CHANGE:DEPOT-PATH#REV",
        value_parser = parse_unreadable_revision,
        help_heading = "PERFORCE OPTIONS"
    )]
    unreadable_revisions: Vec<UnreadableRevisionFlag>,

    /// Changelists imported per batch (at least 1)
    #[clap(long, default_value = "100")]
    batch_size: NonZeroUsize,

    /// How long to wait before looking for new changelists once a repo is caught up (at
    /// least 1)
    #[clap(long, value_parser = parse_secs, default_value = "30")]
    idle_interval_secs: Duration,

    #[clap(flatten)]
    import_args: ImportArgs,
}

/// Each unset flag takes `ImportOptions::default()`'s value.
#[derive(Args)]
struct ImportArgs {
    /// Changelists fetched at once while an earlier one is built
    #[clap(long)]
    changelist_lookahead: Option<usize>,

    /// Files fetched at once per changelist
    #[clap(long)]
    file_concurrency: Option<usize>,

    /// Seconds a batch builds before it stops taking changelists: the one in progress is
    /// finished, and the rest are left for the next batch
    #[clap(long)]
    max_batch_build_secs: Option<u64>,

    /// Changelist numbers a batch's first listing covers after the last one imported; each
    /// listing that leaves the batch short covers twice as many. Unset: one listing up to
    /// the server's latest
    #[clap(long)]
    listing_window: Option<NonZeroU64>,

    /// Ask for a changelist's integration records while its files are fetched, not after
    #[clap(long)]
    copy_sources_with_uploads: bool,

    /// Whether a branch, move/add or integrate whose content is already stored, found by
    /// its MD5 and size, is printed: `verify` looks the content up but prints every file
    #[clap(long, value_enum, default_value_t = ContentReuseFlag::Off)]
    content_reuse: ContentReuseFlag,
}

/// `--content-reuse`'s values, one per `ContentReuse`, which p4_import keeps free of clap.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum ContentReuseFlag {
    Off,
    Verify,
    On,
}

impl From<ContentReuseFlag> for ContentReuse {
    fn from(flag: ContentReuseFlag) -> Self {
        match flag {
            ContentReuseFlag::Off => ContentReuse::Off,
            ContentReuseFlag::Verify => ContentReuse::Verify,
            ContentReuseFlag::On => ContentReuse::On,
        }
    }
}

impl ImportArgs {
    fn import_options(self) -> ImportOptions {
        let defaults = ImportOptions::default();
        ImportOptions {
            changelist_lookahead: self
                .changelist_lookahead
                .unwrap_or(defaults.changelist_lookahead),
            file_concurrency: self.file_concurrency.unwrap_or(defaults.file_concurrency),
            max_build_time: self
                .max_batch_build_secs
                .map_or(defaults.max_build_time, Duration::from_secs),
            listing_window: self.listing_window,
            copy_sources_with_uploads: self.copy_sources_with_uploads,
            content_reuse: self.content_reuse.into(),
            ..defaults
        }
    }
}

/// One `--p4-server`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct P4Server {
    alias: String,
    port: String,
    fingerprint: String,
    ticket_file: PathBuf,
}

impl P4Server {
    /// p4import's flags for this one server, so both get their client the same way.
    fn p4_args(&self, binary: &Path, user: &str) -> P4Args {
        P4Args {
            p4_binary: binary.to_owned(),
            p4_port: self.port.clone(),
            p4_user: user.to_owned(),
            p4_auth: P4AuthArgs {
                p4_password_file: None,
                p4_ticket_file: Some(self.ticket_file.clone()),
                p4_ticket_secret: None,
            },
            p4_fingerprint: Some(self.fingerprint.clone()),
        }
    }
}

/// `<alias>=<p4port>,<fingerprint>,<ticket-file>`, no part empty. The ticket file is the
/// rest of the value, so a comma in its path is fine.
fn parse_server(arg: &str) -> Result<P4Server> {
    const SHAPE: &str = "<alias>=<p4port>,<fingerprint>,<ticket-file>";
    let Some((alias, server)) = arg.split_once('=') else {
        bail!("--p4-server {arg:?} is not {SHAPE}");
    };
    let mut parts = server.splitn(3, ',');
    let (Some(port), Some(fingerprint), Some(ticket_file)) =
        (parts.next(), parts.next(), parts.next())
    else {
        bail!("--p4-server {arg:?} is not {SHAPE}");
    };
    ensure!(
        [alias, port, fingerprint, ticket_file]
            .iter()
            .all(|part| !part.is_empty()),
        "--p4-server {arg:?} leaves a part of {SHAPE} empty"
    );
    Ok(P4Server {
        alias: alias.to_owned(),
        port: port.to_owned(),
        fingerprint: fingerprint.to_owned(),
        ticket_file: PathBuf::from(ticket_file),
    })
}

/// Keyed by alias; the same alias twice is an error.
fn by_alias(servers: Vec<P4Server>) -> Result<HashMap<String, P4Server>> {
    let mut by_alias = HashMap::with_capacity(servers.len());
    for server in servers {
        match by_alias.entry(server.alias.clone()) {
            Entry::Occupied(_) => bail!("--p4-server {} is given more than once", server.alias),
            Entry::Vacant(entry) => {
                entry.insert(server);
            }
        }
    }
    Ok(by_alias)
}

/// One `--unreadable-revision`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct UnreadableRevisionFlag {
    alias: String,
    revision: UnreadableRevision,
}

/// `<alias>:<change>:<depot path>#<rev>`: the alias up to the first `:`, the changelist up to
/// the next, the depot path up to the last `#`, taken as given. The path must start with `//`.
fn parse_unreadable_revision(arg: &str) -> Result<UnreadableRevisionFlag> {
    const SHAPE: &str = "<alias>:<change>:<depot path>#<rev>";
    let Some((alias, rest)) = arg.split_once(':') else {
        bail!("--unreadable-revision {arg:?} is not {SHAPE}");
    };
    let Some((change, rest)) = rest.split_once(':') else {
        bail!("--unreadable-revision {arg:?} is not {SHAPE}");
    };
    let Some((depot_path, revision)) = rest.rsplit_once('#') else {
        bail!("--unreadable-revision {arg:?} is not {SHAPE}");
    };
    ensure!(
        !alias.is_empty(),
        "--unreadable-revision {arg:?} has no server alias"
    );
    ensure!(
        depot_path.starts_with("//") && depot_path.len() > 2,
        "--unreadable-revision {arg:?}: {depot_path:?} is not a depot path"
    );
    let change: u64 = change
        .parse()
        .with_context(|| format!("--unreadable-revision {arg:?}: bad changelist {change:?}"))?;
    let revision: u32 = revision
        .parse()
        .with_context(|| format!("--unreadable-revision {arg:?}: bad revision {revision:?}"))?;
    ensure!(
        change > 0 && revision > 0,
        "--unreadable-revision {arg:?}: changelists and revisions start at 1"
    );
    Ok(UnreadableRevisionFlag {
        alias: alias.to_owned(),
        revision: UnreadableRevision {
            change,
            depot_path: depot_path.to_owned(),
            revision,
        },
    })
}

/// The `--unreadable-revision`s by server alias, so each repo's import gets its own server's.
/// An alias no repo's server has applies to nothing: a server can come from the tier's
/// `p4_servers` as well as from a `--p4-server`, so it is not an error here.
fn unreadable_by_server(
    flags: Vec<UnreadableRevisionFlag>,
) -> HashMap<String, Arc<HashSet<UnreadableRevision>>> {
    let mut by_server: HashMap<String, HashSet<UnreadableRevision>> = HashMap::new();
    for UnreadableRevisionFlag { alias, revision } in flags {
        by_server.entry(alias).or_default().insert(revision);
    }
    by_server
        .into_iter()
        .map(|(alias, revisions)| (alias, Arc::new(revisions)))
        .collect()
}

/// A logged-in-on-demand client per server, by alias. Each ticket file must exist now: a
/// missing secret mount fails the job at startup, not at its first batch.
fn p4_clients(
    servers: Vec<P4Server>,
    binary: &Path,
    user: &str,
    keychain: Arc<dyn SecretReader>,
    scuba: &MononokeScubaSampleBuilder,
) -> Result<HashMap<String, P4Cli>> {
    by_alias(servers)?
        .into_iter()
        .map(|(alias, server)| {
            ensure!(
                server.ticket_file.is_file(),
                "--p4-server {alias}: ticket file {} does not exist",
                server.ticket_file.display()
            );
            let p4 = server
                .p4_args(binary, user)
                .client(keychain.clone(), scuba.clone())
                .with_context(|| format!("--p4-server {alias}"))?;
            Ok((alias, p4))
        })
        .collect()
}

/// Whole seconds, at least 1: a zero wait would poll Perforce in a tight loop.
fn parse_secs(arg: &str) -> Result<Duration> {
    let secs: NonZeroU64 = arg.parse()?;
    Ok(Duration::from_secs(secs.get()))
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    let app = MononokeAppBuilder::new(fb)
        .with_default_service(MononokeService::P4_SYNC)
        .with_default_scuba_dataset("mononoke_p4_sync")
        .with_app_extension(MonitoringAppExtension {})
        .with_app_extension(RepoFilterAppExtension {})
        .build::<P4SyncArgs>()?;
    app.run_with_monitoring_and_logging(async_main, "p4_sync", AliveService)
}

async fn async_main(app: MononokeApp) -> Result<()> {
    let args = app.args::<P4SyncArgs>()?;
    // `--p4-server` requires `--p4-user`, so the empty default never reaches a server.
    let servers = p4_clients(
        args.p4_servers,
        &args.p4_binary,
        &args.p4_user.unwrap_or_default(),
        Arc::new(Keychain(app.fb)),
        &app.environment().scuba_sample_builder,
    )?;
    let unreadable_revisions = unreadable_by_server(args.unreadable_revisions);
    let settings = sharded::Settings {
        exit_file: args.exit_file,
        servers,
        p4_binary: args.p4_binary,
        unreadable_revisions,
        batch_size: args.batch_size,
        idle_interval: args.idle_interval_secs,
        import_options: args.import_args.import_options(),
    };
    sharded::run(app, args.sharded_executor_args, settings).await
}

#[cfg(test)]
mod tests;
