/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(trait_alias)]

//! Command-line client for the Source Control Service.

use std::env;
use std::io::IsTerminal;
use std::io::stderr;
#[cfg(not(fbcode_build))]
use std::process::ExitCode;
use std::sync::LazyLock;

use ansi_term::Colour;
use base_app::BaseApp;
use clap::ArgMatches;
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
#[cfg(fbcode_build)]
use cli::ExitCode;
use fbinit::FacebookInit;
use scs_client_raw::ScsClient;

use crate::render::OutputFormat;

mod args;
mod commands;
mod errors;
#[cfg(fbcode_build)]
mod facebook;
pub(crate) mod library;
#[cfg(not(fbcode_build))]
mod oss;
mod render;
pub(crate) mod util;

#[cfg(fbcode_build)]
use facebook as platform;
#[cfg(not(fbcode_build))]
use oss as platform;
use platform::ConnectionArgs;

const SCSC_ADMIN_ENABLED_ENV: &str = "SCSC_ADMIN_ENABLED";
const SCSC_PRINT_CORRELATOR_ENV: &str = "SCSC_PRINT_CORRELATOR";

static SHORT_VERSION: LazyLock<String> = LazyLock::new(platform::short_version);
static LONG_VERSION: LazyLock<String> = LazyLock::new(platform::long_version);

pub(crate) struct ScscApp {
    matches: ArgMatches,
    connection_args: ConnectionArgs,
    print_correlator: bool,
    target: OutputFormat,
    fb: FacebookInit,
}

impl ScscApp {
    #[cfg(fbcode_build)]
    pub(crate) fn scs_host(&self) -> Option<&str> {
        self.connection_args.host()
    }

    async fn get_connection(&self, repo: Option<&str>) -> anyhow::Result<ScsClient> {
        let conn = self.connection_args.get_connection(self.fb, repo).await?;
        if self.print_correlator {
            match conn.get_client_corrrelator() {
                Some(correlator) => println!("Client correlator: {correlator}"),
                None => println!("Client correlator: <none>"),
            }
        }
        Ok(conn)
    }
}

impl BaseApp for ScscApp {
    fn subcommand(&self) -> Option<(&str, &ArgMatches)> {
        self.matches.subcommand()
    }
}

#[derive(Parser)]
#[clap(
    version(&**SHORT_VERSION),
    long_version(&**LONG_VERSION),
    term_width(textwrap::termwidth()),
)]
/// Send requests to the Source Control Service
struct ScscArgs {
    /// Should the output of the command be JSON?
    #[clap(long, global = true)]
    json: bool,

    #[clap(flatten)]
    connection_args: ConnectionArgs,

    #[clap(flatten)]
    platform_args: platform::PlatformArgs,
}

/// Wrapper that lets `cli::main` drive clap parsing while still injecting
/// scsc's dynamically-built (`base_app::subcommands!`) subcommands, which are
/// not part of the derived `ScscArgs::command()`.
struct ScscCli {
    args: ScscArgs,
    matches: ArgMatches,
}

impl CommandFactory for ScscCli {
    fn command() -> clap::Command {
        let subcommands = commands::subcommands();
        assert!(!subcommands.is_empty());
        ScscArgs::command()
            .subcommands(subcommands)
            .subcommand_required(true)
            .arg_required_else_help(true)
    }

    fn command_for_update() -> clap::Command {
        Self::command()
    }
}

impl FromArgMatches for ScscCli {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        Ok(ScscCli {
            args: ScscArgs::from_arg_matches(matches)?,
            matches: matches.clone(),
        })
    }

    fn update_from_arg_matches(&mut self, m: &ArgMatches) -> Result<(), clap::Error> {
        self.args.update_from_arg_matches(m)?;
        self.matches = m.clone();
        Ok(())
    }
}

async fn main_impl(fb: FacebookInit, cli: ScscCli) -> anyhow::Result<()> {
    platform::disable_cpp_log(fb);
    // Continue an inbound Artillery trace (if any) for this whole invocation so the
    // SCS calls below join the caller's trace. fbcode-only: OSS builds have no
    // Artillery, and the guard must live here (not inside `prepare`, where it
    // would drop at return).
    #[cfg(fbcode_build)]
    let _artillery_trace_block = art_cli_lite_rs::trace_cli("scsc");
    platform::prepare(fb)?;

    let ScscCli {
        args: common_args,
        matches,
    } = cli;
    let scsc_admin_enabled = env::var_os(SCSC_ADMIN_ENABLED_ENV).is_some();
    common_args.platform_args.initialize().await?;
    let connection_args = common_args.connection_args;
    let target = if common_args.json {
        OutputFormat::Json
    } else {
        OutputFormat::Text
    };
    let print_correlator = scsc_admin_enabled && env::var_os(SCSC_PRINT_CORRELATOR_ENV).is_some();
    let app = ScscApp {
        matches,
        connection_args,
        print_correlator,
        target,
        fb,
    };
    commands::dispatch(app).await
}

fn report_error(error: &anyhow::Error) {
    let prog_name = env::args().next().unwrap_or_else(|| "scsc".to_string());
    if stderr().is_terminal() {
        eprintln!(
            "{}: {} {:#}",
            prog_name,
            Colour::Red.bold().paint("error:"),
            error
        );
    } else {
        eprintln!("{prog_name}: error: {error:#}");
    }
}

// `cli::main` requires a `Result`; handled errors are encoded in the `ExitCode`
// so usage telemetry records failures without changing scsc's error format.
// The `cli` crate is fbcode-only; OSS builds parse with the same `ScscCli`
// command definition under `fbinit::main`.
#[cfg(fbcode_build)]
#[cli::main("scsc", version = false, author_help = false)]
async fn main(fb: FacebookInit, cli: ScscCli) -> anyhow::Result<ExitCode> {
    if let Err(error) = main_impl(fb, cli).await {
        report_error(&error);
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(not(fbcode_build))]
#[fbinit::main]
async fn main(fb: FacebookInit) -> ExitCode {
    let matches = ScscCli::command().get_matches();
    let cli = match ScscCli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    if let Err(error) = main_impl(fb, cli).await {
        report_error(&error);
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
