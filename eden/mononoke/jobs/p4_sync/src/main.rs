/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Perforce -> Mononoke mirror tailer: for each repo ShardManager assigns it, imports the
//! depot's new changelists in batches with `p4_import::import_next`, from whichever of its
//! Perforce servers the repo's `p4_source.server_alias` names.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use clap::Parser;
use executor_lib::args::ShardedExecutorArgs;
use fbinit::FacebookInit;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::args::RepoFilterAppExtension;
use mononoke_app::args::TLSArgs;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use p4_source::P4Args;
use p4_source::P4AuthArgs;
use p4_source::P4Cli;
use scuba_ext::MononokeScubaSampleBuilder;

mod repo;
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

    /// Perforce user to connect as, on every server
    #[clap(long, help_heading = "PERFORCE OPTIONS")]
    p4_user: String,

    /// A Perforce server this job imports from, as `<alias>=<p4port>,<fingerprint>,<ticket-file>`:
    /// the alias as repo configs name it in `p4_source.server_alias` (e.g. `test1`), the
    /// server's address as `P4PORT` (e.g. `ssl6:[2001:db8::1]:1818`), its SSL fingerprint as
    /// `p4 trust` prints it, and a file holding a ticket for `--p4-user` on it. Once per
    /// server; shards of repos on any other server are held.
    #[clap(
        long = "p4-server",
        value_name = "ALIAS=P4PORT,FINGERPRINT,TICKET-FILE",
        value_parser = parse_server,
        required = true,
        help_heading = "PERFORCE OPTIONS"
    )]
    p4_servers: Vec<P4Server>,

    /// Changelists imported per batch (at least 1)
    #[clap(long, default_value = "100")]
    batch_size: NonZeroUsize,

    /// How long to wait before looking for new changelists once a repo is caught up (at
    /// least 1)
    #[clap(long, value_parser = parse_secs, default_value = "30")]
    idle_interval_secs: Duration,
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

/// A logged-in-on-demand client per server, by alias. Each ticket file must exist now: a
/// missing secret mount fails the job at startup, not at its first batch.
fn p4_clients(
    servers: Vec<P4Server>,
    binary: &Path,
    user: &str,
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
                .client(scuba.clone())
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
        .with_default_scuba_dataset("mononoke_p4_sync")
        .with_app_extension(MonitoringAppExtension {})
        .with_app_extension(RepoFilterAppExtension {})
        .build::<P4SyncArgs>()?;
    app.run_with_monitoring_and_logging(async_main, "p4_sync", AliveService)
}

async fn async_main(app: MononokeApp) -> Result<()> {
    let args = app.args::<P4SyncArgs>()?;
    let servers = p4_clients(
        args.p4_servers,
        &args.p4_binary,
        &args.p4_user,
        &app.environment().scuba_sample_builder,
    )?;
    let settings = sharded::Settings {
        exit_file: args.exit_file,
        servers,
        batch_size: args.batch_size,
        idle_interval: args.idle_interval_secs,
    };
    sharded::run(app, args.sharded_executor_args, settings).await
}

#[cfg(test)]
mod tests;
