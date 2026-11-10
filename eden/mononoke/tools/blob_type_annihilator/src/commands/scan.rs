/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use futures::future;
use futures::stream;
use futures::stream::StreamExt;
use mononoke_app::MononokeApp;
use tracing::warn;

use crate::progress::Phase;
use crate::run::Run;
use crate::run::RunArgs;
use crate::scan;
use crate::scan::InventoryContext;
use crate::scan::ScanOptions;
use crate::scope::Capability;

/// Enumerate retired keys in the plan's stores and record an inventory
#[derive(Parser)]
pub struct CommandArgs {
    #[clap(flatten)]
    run: RunArgs,

    /// Only report whether each cell holds any retired key; writes nothing
    #[clap(long)]
    exists: bool,

    /// Only scan these components, a partial scope
    #[clap(long = "component")]
    components: Vec<String>,

    #[clap(flatten)]
    scan: ScanOptions,
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let ctx = app.new_basic_context();
    let (run, stores) = Run::open_with_stores(&app, &args.run, &args.components).await?;
    let plan = run.plan();
    for component in &plan.components {
        match &component.capability {
            Capability::Supported => {}
            Capability::Unsupported { reason } | Capability::Excluded { reason } => println!(
                "not scanned {} {:?}: {reason}",
                component.id, component.store
            ),
        }
    }
    let cells = run.cells(&stores);
    let digits = plan.request.partition_digits;

    if args.exists {
        let reporter = args.run.progress.start(Phase::new(
            "scan --exists",
            "cells",
            cells.len(),
            &["present", "absent", "unknown"],
        ));
        let phase = reporter.phase();
        let mut results = stream::iter(&cells)
            .map(|cell| {
                let (ctx, stores) = (&ctx, &stores);
                async move {
                    let result = async {
                        scan::cell_has_keys(ctx, stores.get(&cell.component.id)?, *cell).await
                    }
                    .await;
                    phase.unit_done_as(match &result {
                        Ok(true) => "present",
                        Ok(false) => "absent",
                        Err(_) => "unknown",
                    });
                    (cell, result)
                }
            })
            .buffered(args.scan.scan_concurrency.get());
        let mut unknown = 0;
        while let Some((cell, result)) = results.next().await {
            match result {
                Ok(true) => println!("present {cell}"),
                Ok(false) => println!("absent {cell}"),
                Err(e) => {
                    unknown += 1;
                    println!("unknown {cell}: {e:#}");
                }
            }
        }
        drop(results);
        reporter.finish().await;
        if unknown > 0 {
            bail!("{unknown} cells could not be checked");
        }
        return Ok(());
    }

    let reporter = args.run.progress.start(Phase::new(
        "scan",
        "partitions",
        cells.len() * scan::partition_count(digits),
        &["failed"],
    ));
    let failures = scan::run_partitions(
        &cells,
        digits,
        args.scan.scan_concurrency,
        reporter.phase(),
        |cell, hex| {
            let (ctx, run, stores) = (&ctx, &run, &stores);
            let concurrency = args.scan.classify_concurrency;
            async move {
                let ictx = InventoryContext {
                    ctx,
                    state: &run.state,
                    store: stores.get(&cell.component.id)?,
                    concurrency,
                };
                scan::inventory_partition(ictx, cell, &hex).await
            }
        },
    )
    .filter(|(cell, hex, result)| {
        if let Err(e) = result {
            warn!("partition {hex} of {cell} failed: {e:#}");
        }
        future::ready(result.is_err())
    })
    .count()
    .await;
    reporter.finish().await;

    scan::print_inventory(&run.state, &cells, digits).await?;
    if failures > 0 {
        bail!("{failures} partitions failed; rerun scan to retry them");
    }
    Ok(())
}
