/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;

use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use context::CoreContext;
use futures::future;
use futures::stream::StreamExt;
use mononoke_app::MononokeApp;
use tracing::warn;

use crate::apply::ApplyContext;
use crate::apply::ApplyOptions;
use crate::apply::ApplyTotals;
use crate::apply::apply_partition;
use crate::pack_rewrite::PackOptions;
use crate::pack_rewrite::freeze;
use crate::pack_rewrite::missing_pack_note;
use crate::run::Run;
use crate::run::RunArgs;
use crate::run::Stores;
use crate::scan;
use crate::scan::Cell;
use crate::scan::CellKey;
use crate::scope::Plan;

/// Delete the inventoried retired keys from the plan's stores
///
/// Requires a complete inventory.  Run only under a maintenance fence: no
/// readers or writers of the retired types, and no healing, scrubbing or
/// packing of the affected stores, for the whole run.
#[derive(Parser)]
pub struct CommandArgs {
    #[clap(flatten)]
    run: RunArgs,

    /// Digest of the reviewed plan; the run is refused for any other plan
    #[clap(long)]
    confirm_plan_digest: String,

    /// Only apply to these components, a partial scope
    #[clap(long = "component")]
    components: Vec<String>,

    #[clap(flatten)]
    apply: ApplyOptions,

    #[clap(flatten)]
    pack: PackOptions,
}

/// One `apply` invocation.
struct Apply<'a> {
    ctx: &'a CoreContext,
    run: &'a Run,
    stores: &'a Stores,
    cells: &'a [Cell<'a>],
    args: &'a CommandArgs,
}

impl<'a> Apply<'a> {
    fn plan(&self) -> &Plan {
        self.run.plan()
    }

    fn digits(&self) -> u8 {
        self.plan().request.partition_digits
    }

    async fn ensure_inventoried(&self) -> Result<()> {
        let mut missing = 0;
        for cell in self.cells {
            for hex in scan::partition_hexes(self.digits()) {
                if cell.committed(&self.run.state, &hex).await?.is_none() {
                    missing += 1;
                }
            }
        }
        if missing > 0 {
            bail!("{missing} partitions are not inventoried; complete `scan` first");
        }
        Ok(())
    }

    /// Freezes the pack tasks of every component and repo.
    async fn freeze_all(&self) -> Result<()> {
        // Cells are ordered by component, then repo.
        let groups: Vec<&[Cell<'_>]> = self
            .cells
            .chunk_by(|a, b| a.component.id == b.component.id && a.repo_id == b.repo_id)
            .collect();
        for group in groups {
            let summary =
                freeze(&self.run.state, &self.plan().digest, self.digits(), group).await?;
            if let Some(cell) = group.first() {
                println!(
                    "{} repo {}: {} pack tasks covering {} packed aliases",
                    cell.component.id, cell.repo_id, summary.tasks, summary.packed_aliases
                );
            }
        }
        Ok(())
    }

    /// Applies every partition, returning the totals of each cell.
    async fn apply_partitions(&self) -> Result<BTreeMap<CellKey<'a>, ApplyTotals>> {
        let unlink = self.args.apply.unlink_settings();
        let totals = scan::run_partitions(
            self.cells,
            self.digits(),
            self.args.apply.partition_concurrency,
            |cell, hex| {
                let unlink = &unlink;
                async move {
                    let actx = ApplyContext {
                        ctx: self.ctx,
                        state: &self.run.state,
                        store: self.stores.get(&cell.component.id)?,
                        cell,
                        selection: &self.plan().request.selection,
                        batch_size: self.args.apply.batch_size,
                        unlink,
                        pack: &self.args.pack,
                    };
                    apply_partition(&actx, &hex).await
                }
            },
        )
        .fold(BTreeMap::new(), |mut totals, (cell, hex, result)| {
            if let Err(e) = &result {
                warn!("partition {hex} of {cell} failed: {e:#}");
            }
            totals
                .entry(cell.key())
                .or_insert_with(ApplyTotals::default)
                .record(result.as_ref().ok());
            future::ready(totals)
        })
        .await;
        Ok(totals)
    }
}

/// Prints a line for every cell with something to report, then examples of
/// what needs attention, and returns the totals over all cells.
fn print_summary(cells: &[Cell<'_>], totals: &BTreeMap<CellKey<'_>, ApplyTotals>) -> ApplyTotals {
    let mut all = ApplyTotals::default();
    let mut quiet_cells = 0;
    for cell in cells {
        let Some(t) = totals.get(&cell.key()) else {
            continue;
        };
        all.add(t);
        if t.done == t.partitions
            && t.unlinked == 0
            && t.already_absent == 0
            && t.failed.is_empty()
            && t.blocked.is_empty()
            && t.packed_deferred == 0
            && t.tasks == Default::default()
        {
            quiet_cells += 1;
            continue;
        }
        let mut line = format!(
            "{cell}: {}/{} partitions done; unlinked {}, already absent {}, failed {}, blocked {}, packed {}",
            t.done,
            t.partitions,
            t.unlinked,
            t.already_absent,
            t.failed.count,
            t.blocked.count,
            t.packed_deferred,
        );
        let p = &t.tasks;
        if *p != Default::default() {
            line.push_str(&format!(
                "; pack tasks complete {} (rewritten {}), retired aliases unlinked {}, survivors relinked {}, source missing {}, conflicts {}, failed {}",
                p.complete,
                p.rewritten,
                p.retired_unlinked + p.retired_already_absent,
                p.survivors_relinked,
                p.source_missing.count,
                p.conflicts.count,
                p.failed.count,
            ));
        }
        println!("{line}");
    }
    println!("{quiet_cells} cells had nothing to delete");

    let more = |shown: usize, count: u64, what: &str| {
        if count > shown as u64 {
            println!(
                "  and {} more {what}; the state directory records them all",
                count - shown as u64
            );
        }
    };
    for failed in &all.failed.examples {
        println!("  failed {}: {}", failed.key, failed.error);
    }
    more(all.failed.examples.len(), all.failed.count, "failed keys");
    for blocked in &all.blocked.examples {
        println!("  blocked {blocked}: unreadable envelope");
    }
    more(
        all.blocked.examples.len(),
        all.blocked.count,
        "blocked keys",
    );
    let tasks = &all.tasks;
    for issue in tasks
        .conflicts
        .examples
        .iter()
        .chain(&tasks.failed.examples)
    {
        println!("  pack task {issue}");
    }
    more(
        tasks.conflicts.examples.len() + tasks.failed.examples.len(),
        tasks.conflicts.count + tasks.failed.count,
        "pack task issues",
    );
    for pack_key in &tasks.source_missing.examples {
        println!("  warning: {}", missing_pack_note(pack_key));
    }
    more(
        tasks.source_missing.examples.len(),
        tasks.source_missing.count,
        "missing packs",
    );
    all
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let ctx = app.new_basic_context();
    let (run, stores) = Run::open_with_stores(&app, &args.run, &args.components).await?;
    let plan = run.plan();
    if args.confirm_plan_digest != plan.digest {
        bail!(
            "confirmed digest {} is not the plan's digest {}",
            args.confirm_plan_digest,
            plan.digest
        );
    }
    let cells = run.cells(&stores);
    let apply = Apply {
        ctx: &ctx,
        run: &run,
        stores: &stores,
        cells: &cells,
        args: &args,
    };
    apply.ensure_inventoried().await?;
    apply.freeze_all().await?;
    let totals = apply.apply_partitions().await?;
    let all = print_summary(&cells, &totals);

    let incomplete = all.partitions - all.done;
    if incomplete > 0 {
        bail!("{incomplete} partitions are incomplete; resolve them and rerun apply");
    }
    Ok(())
}
