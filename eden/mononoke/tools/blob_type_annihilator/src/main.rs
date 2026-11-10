/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Removes retired derived data blob families, and their metadata, from
//! repository storage.

use anyhow::Result;
use clap::Parser;
use fbinit::FacebookInit;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;

mod adapters;
mod apply;
mod commands;
mod metadata;
mod pack_rewrite;
mod policy;
mod run;
mod scan;
mod scope;
mod state;
mod verify;

/// Remove retired derived data blob families from repository storage.
#[derive(Parser)]
struct BlobTypeAnnihilatorArgs {}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    MononokeAppBuilder::new(fb)
        .build_with_subcommands::<BlobTypeAnnihilatorArgs>(commands::subcommands())?
        .run_basic(async_main)
}

async fn async_main(app: MononokeApp) -> Result<()> {
    commands::dispatch(app).await
}
