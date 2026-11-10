/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::num::NonZeroU32;
use std::num::NonZeroUsize;

use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use futures::stream;
use futures::stream::StreamExt;
use mononoke_app::MononokeApp;

use crate::progress::Phase;
use crate::run::Run;
use crate::run::RunArgs;
use crate::verify;
use crate::verify::CellResult;
use crate::verify::CellState;
use crate::verify::Family;
use crate::verify::FamilyAudit;
use crate::verify::MetadataProbe;
use crate::verify::ReportInputs;
use crate::verify::Verdict;

/// Verify afresh that the plan's retired keys and metadata are gone, and
/// write a completion report
#[derive(Parser)]
pub struct CommandArgs {
    #[clap(flatten)]
    run: RunArgs,

    /// Cells, families and repositories checked at the same time
    #[clap(long, default_value_t = NonZeroUsize::new(8).expect("8 is non-zero"))]
    concurrency: NonZeroUsize,

    /// Mapping rows counted per type and connection when metadata remains
    #[clap(long, default_value_t = NonZeroU32::new(100).expect("100 is non-zero"))]
    metadata_sample: NonZeroU32,
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let ctx = app.new_basic_context();
    let started_at = verify::unix_now();
    let (run, stores) = Run::open_with_stores(&app, &args.run, &[]).await?;
    let plan = run.plan();
    let concurrency = args.concurrency.get();
    let progress = &args.run.progress;

    let cells = run.cells(&stores);
    let reporter = progress.start(Phase::new("verify cells", "cells", cells.len(), &[]));
    let phase = reporter.phase();
    let cells = stream::iter(cells)
        .map(|cell| {
            let (ctx, stores) = (&ctx, &stores);
            async move {
                let result = match stores.get(&cell.component.id) {
                    Ok(store) => verify::check_cell(ctx, store, cell).await,
                    Err(e) => CellResult {
                        component: cell.component.id.clone(),
                        repo_id: cell.repo_id,
                        recipe: cell.recipe.id.to_string(),
                        state: CellState::Unknown {
                            error: format!("{e:#}"),
                        },
                    },
                };
                phase.unit_done();
                result
            }
        })
        .buffered(concurrency)
        .collect()
        .await;
    reporter.finish().await;

    let components = run.components(&[])?;
    let families: Vec<Family<'_>> = components
        .iter()
        .flat_map(|component| {
            component.repo_ids().flat_map(move |repo_id| {
                plan.request.selection.types().flat_map(move |blob_type| {
                    blob_type.audit_families().iter().map(move |family| Family {
                        component,
                        repo_id,
                        family,
                    })
                })
            })
        })
        .collect();
    let reporter = progress.start(Phase::new(
        "verify family audits",
        "families",
        families.len(),
        &[],
    ));
    let phase = reporter.phase();
    let audits = stream::iter(families)
        .map(|family| {
            let (ctx, stores) = (&ctx, &stores);
            async move {
                let audit = match stores.get(&family.component.id) {
                    Ok(store) => verify::audit_family(ctx, store, family).await,
                    Err(e) => FamilyAudit {
                        component: family.component.id.clone(),
                        repo_id: family.repo_id,
                        family: family.family.to_string(),
                        registered: 0,
                        unknown: Default::default(),
                        error: Some(format!("{e:#}")),
                    },
                };
                phase.unit_done();
                audit
            }
        })
        .buffered(concurrency)
        .collect()
        .await;
    reporter.finish().await;

    let probe = MetadataProbe {
        ctx: &ctx,
        state: &run.state,
        selection: &plan.request.selection,
        sample: args.metadata_sample,
    };
    let reporter = progress.start(Phase::new(
        "verify metadata",
        "repos",
        plan.repos.len(),
        &[],
    ));
    let phase = reporter.phase();
    let metadata = stream::iter(&plan.repos)
        .map(|repo| {
            let (app, probe) = (&app, &probe);
            async move {
                let result = verify::check_repo_metadata(app, probe, repo).await;
                phase.unit_done();
                result
            }
        })
        .buffered(concurrency)
        .collect()
        .await;
    reporter.finish().await;

    let packs = stream::iter(&components)
        .flat_map(|component| {
            stream::iter(
                component
                    .repo_ids()
                    .map(move |repo_id| (component, repo_id)),
            )
        })
        .map(|(component, repo_id)| verify::pack_result(&run.state, &component.id, repo_id))
        .buffered(concurrency)
        .collect()
        .await;

    let report = verify::build_report(
        plan,
        ReportInputs {
            started_at,
            cells,
            audits,
            metadata,
            packs,
        },
    );
    let path = verify::report_path(&report);
    run.state.write_json(&path, &report).await?;
    println!("{}", verify::summarise(&report));
    println!("Report: {}", run.state.path(&path).display());
    if report.verdict == Verdict::Incomplete {
        bail!("verification found retired data, failures or unchecked cells");
    }
    Ok(())
}
