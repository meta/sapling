/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::PathBuf;
use std::slice;

use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use futures::future;
use futures::stream::StreamExt;
use mononoke_app::MononokeApp;
use tracing::warn;

use crate::import;
use crate::import::KeySource;
use crate::progress::Phase;
use crate::run::Run;
use crate::run::RunArgs;
use crate::scan;
use crate::scan::InventoryContext;
use crate::scan::ScanOptions;

/// Record an inventory from lists of keys instead of enumerating stores
#[derive(Parser)]
pub struct CommandArgs {
    #[clap(flatten)]
    run: RunArgs,

    /// Files of keys, one per line, such as `export_manifold_keys` output.
    /// Manifold key dump paths (`flat/<key>`) and packblob envelope keys are
    /// accepted
    #[clap(
        long = "keys-file",
        num_args = 1..,
        required_unless_present_any = ["keys_dir", "from_component"],
        conflicts_with_all = ["keys_dir", "from_component"]
    )]
    keys_files: Vec<PathBuf>,

    /// A directory of key files: every `*.keys` file in it, in name order
    #[clap(long, conflicts_with = "from_component")]
    keys_dir: Option<PathBuf>,

    /// Take the keys from this component's inventory, recorded by an
    /// earlier scan of the same run
    #[clap(long)]
    from_component: Option<String>,

    /// Only record inventory for these components, a partial scope
    #[clap(long = "component")]
    components: Vec<String>,

    #[clap(flatten)]
    scan: ScanOptions,
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let ctx = app.new_basic_context();
    let run = Run::open(&app, &args.run.state_dir).await?;
    let plan = run.plan();
    // Staging can take hours, so every argument is checked first.
    let components = run.components(&args.components)?;
    let source = match (&args.from_component, &args.keys_dir) {
        (Some(component), _) => {
            run.components(slice::from_ref(component))?;
            KeySource::Inventory {
                component: component.clone(),
            }
        }
        (None, Some(dir)) => KeySource::keys_dir(dir).await?,
        (None, None) => KeySource::files(&args.keys_files).await?,
    };

    let unit = match &source {
        KeySource::Files { .. } => "files",
        KeySource::Inventory { .. } => "partitions",
    };
    let reporter = args.run.progress.start(Phase::new(
        "import staging",
        unit,
        import::staging_units(plan, &source),
        &["staged", "outside plan", "rejected"],
    ));
    let staged = import::stage(&run.state, plan, source, reporter.phase()).await?;
    reporter.finish().await;
    println!(
        "staged {} of {} keys; {} outside the plan, {} rejected",
        staged.staged, staged.lines, staged.outside_plan, staged.rejected.count
    );
    for example in &staged.rejected.examples {
        println!("  rejected {example}");
    }

    let stores = run
        .open_stores(&app, &components, &args.run.adapter)
        .await?;
    let cells = run.cells(&stores);
    let digits = plan.request.partition_digits;
    let reporter = args.run.progress.start(Phase::new(
        "import",
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
                import::import_partition(ictx, cell, &hex).await
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
        bail!("{failures} partitions failed; rerun import-keys with the same source to retry them");
    }
    Ok(())
}
