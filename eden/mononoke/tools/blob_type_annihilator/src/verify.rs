/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Independent verification and the completion report.
//!
//! Verification enumerates every cell afresh, audits the retired types'
//! wider key families for unregistered formats, and reads the remaining
//! metadata from the primary.  Anything not verified absent, including
//! excluded and unsupported stores, keeps the result short of complete.

use std::iter;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use context::CoreContext;
use futures::stream::TryStreamExt;
use mononoke_app::MononokeApp;
use mononoke_types::RepositoryId;
use serde::Serialize;

use crate::adapters::ComponentStore;
use crate::metadata;
use crate::metadata::FinalPass;
use crate::metadata::MetadataRemaining;
use crate::metadata::RepoMetadata;
use crate::pack_rewrite::FreezeSummary;
use crate::pack_rewrite::TaskTotals;
use crate::pack_rewrite::completed_tasks;
use crate::pack_rewrite::frozen_summary;
use crate::pack_rewrite::missing_pack_note;
use crate::policy::Selection;
use crate::policy::parse_physical_key;
use crate::scan::Cell;
use crate::scan::Sampled;
use crate::scan::cell_has_keys;
use crate::scope::Capability;
use crate::scope::PhysicalStore;
use crate::scope::Plan;
use crate::scope::PlannedComponent;
use crate::scope::PlannedRepo;
use crate::scope::RepoSelection;
use crate::state::StateDir;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CellState {
    Absent,
    Present,
    Unknown { error: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct CellResult {
    pub component: String,
    pub repo_id: RepositoryId,
    pub recipe: String,
    #[serde(flatten)]
    pub state: CellState,
}

/// One retired type's key family in one component and repository.
#[derive(Clone, Copy)]
pub struct Family<'a> {
    pub component: &'a PlannedComponent,
    pub repo_id: RepositoryId,
    pub family: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct FamilyAudit {
    pub component: String,
    pub repo_id: RepositoryId,
    pub family: String,
    /// Keys matching a registered recipe.
    pub registered: u64,
    /// Keys matching no registered recipe: an unknown format.
    pub unknown: Sampled<String>,
    pub error: Option<String>,
}

impl FamilyAudit {
    fn is_clean(&self) -> bool {
        self.registered == 0 && self.unknown.is_empty() && self.error.is_none()
    }
}

/// Enumerates every key under a family prefix, which spans all of the
/// family's recipes and any unregistered formats.
pub async fn audit_family(
    ctx: &CoreContext,
    store: &ComponentStore,
    family: Family<'_>,
) -> FamilyAudit {
    let mut audit = FamilyAudit {
        component: family.component.id.clone(),
        repo_id: family.repo_id,
        family: family.family.to_string(),
        registered: 0,
        unknown: Sampled::default(),
        error: None,
    };
    let prefix = format!("{}{}", family.repo_id.prefix(), family.family);
    let mut pages = store.pages(ctx, &prefix);
    loop {
        let keys = match pages.try_next().await {
            Ok(Some(keys)) => keys,
            Ok(None) => return audit,
            Err(e) => {
                audit.error = Some(format!("{e:#}"));
                return audit;
            }
        };
        for key in keys {
            match parse_physical_key(&key) {
                Some((parsed, _)) if parsed.repo_id == family.repo_id => audit.registered += 1,
                _ => audit.unknown.note(key),
            }
        }
    }
}

pub async fn check_cell(ctx: &CoreContext, store: &ComponentStore, cell: Cell<'_>) -> CellResult {
    let state = match cell_has_keys(ctx, store, cell).await {
        Ok(true) => CellState::Present,
        Ok(false) => CellState::Absent,
        Err(e) => CellState::Unknown {
            error: format!("{e:#}"),
        },
    };
    CellResult {
        component: cell.component.id.clone(),
        repo_id: cell.repo_id,
        recipe: cell.recipe.id.to_string(),
        state,
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "check", rename_all = "snake_case")]
pub enum MetadataCheck {
    Checked {
        remaining: MetadataRemaining,
        /// `None` until `apply` has run its final metadata pass under this
        /// plan, after the blobs were gone.
        final_pass: Option<FinalPass>,
    },
    Failed {
        error: String,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct RepoMetadataResult {
    pub repo_id: RepositoryId,
    #[serde(flatten)]
    pub check: MetadataCheck,
}

impl RepoMetadataResult {
    fn is_clean(&self) -> bool {
        matches!(
            &self.check,
            MetadataCheck::Checked {
                remaining,
                final_pass: Some(pass),
            } if remaining.is_clean() && pass.is_clean()
        )
    }
}

/// What checking a repository's metadata needs.
pub struct MetadataProbe<'a> {
    pub ctx: &'a CoreContext,
    pub state: &'a StateDir,
    pub selection: &'a Selection,
    /// Mapping rows counted per type and connection.
    pub sample: NonZeroU32,
}

/// Reads a repository's remaining metadata from the primary, and the final
/// metadata pass recorded under this plan.
pub async fn check_repo_metadata(
    app: &MononokeApp,
    probe: &MetadataProbe<'_>,
    repo: &PlannedRepo,
) -> RepoMetadataResult {
    let check = async {
        let md = RepoMetadata::open(app, &repo.name).await?;
        let remaining = md
            .remaining(probe.ctx, repo.id, probe.selection, probe.sample)
            .await?;
        let final_pass = metadata::final_pass(probe.state, repo.id).await?;
        anyhow::Ok(MetadataCheck::Checked {
            remaining,
            final_pass,
        })
    }
    .await;
    RepoMetadataResult {
        repo_id: repo.id,
        check: check.unwrap_or_else(|e| MetadataCheck::Failed {
            error: format!("{e:#}"),
        }),
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "check", rename_all = "snake_case")]
pub enum PackCheck {
    Checked {
        /// `None` if `apply` has not frozen the pack tasks, so packed
        /// aliases may not have been handled.
        frozen: Option<FreezeSummary>,
        tasks: TaskTotals,
    },
    Failed {
        error: String,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct PackResult {
    pub component: String,
    pub repo_id: RepositoryId,
    #[serde(flatten)]
    pub check: PackCheck,
}

impl PackResult {
    /// Every frozen task completed.
    fn is_complete(&self) -> bool {
        matches!(
            &self.check,
            PackCheck::Checked {
                frozen: Some(frozen),
                tasks,
            } if frozen.tasks == tasks.complete
        )
    }
}

/// Gathers pack task completion from the state directory.
pub async fn pack_result(state: &StateDir, component: &str, repo_id: RepositoryId) -> PackResult {
    let check = async {
        let frozen = frozen_summary(state, component, repo_id).await?;
        let tasks = completed_tasks(state, component, repo_id)
            .await?
            .iter()
            .fold(TaskTotals::default(), |mut tasks, outcome| {
                tasks.record(outcome);
                tasks
            });
        anyhow::Ok(PackCheck::Checked { frozen, tasks })
    }
    .await;
    PackResult {
        component: component.to_string(),
        repo_id,
        check: check.unwrap_or_else(|e| PackCheck::Failed {
            error: format!("{e:#}"),
        }),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every store and repository is covered and verified clean.
    Complete,
    /// Every covered cell is verified clean, but the scope excludes stores,
    /// storage or repositories, listed in the report.
    CompleteForCoveredScope,
    /// Something remains, failed or could not be checked.
    Incomplete,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub plan_digest: String,
    pub policy_version: String,
    pub started_at: u64,
    pub finished_at: u64,
    pub repo_scope: String,
    pub cells: Vec<CellResult>,
    pub audits: Vec<FamilyAudit>,
    pub metadata: Vec<RepoMetadataResult>,
    pub packs: Vec<PackResult>,
    /// Stores, storage and scope this report does not cover.
    pub not_covered: Vec<String>,
    pub physical_reclamation: Vec<String>,
    pub operator_actions: Vec<String>,
    pub verdict: Verdict,
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The results a report is built from.
pub struct ReportInputs {
    pub started_at: u64,
    pub cells: Vec<CellResult>,
    pub audits: Vec<FamilyAudit>,
    pub metadata: Vec<RepoMetadataResult>,
    pub packs: Vec<PackResult>,
}

/// Assembles the report and decides its verdict.
pub fn build_report(plan: &Plan, inputs: ReportInputs) -> Report {
    let mut not_covered = Vec::new();
    let repo_scope = match &plan.request.repos {
        RepoSelection::AllConfigured => {
            "all configured repositories; repositories absent from the configuration are not covered"
                .to_string()
        }
        RepoSelection::Explicit(ids) => {
            let ids: Vec<i32> = ids.iter().map(|id| id.id()).collect();
            not_covered.push(format!("repositories other than {ids:?}"));
            format!("partial: repositories {ids:?}")
        }
    };
    let mut physical_reclamation = Vec::new();
    for component in &plan.components {
        match &component.capability {
            Capability::Supported => match &component.store {
                PhysicalStore::Sqlite { .. } | PhysicalStore::Mysql { .. } => physical_reclamation
                    .push(format!(
                        "{}: pending; unlinked keys leave chunks for sqlblob GC to reclaim",
                        component.id
                    )),
                PhysicalStore::Manifold { .. } => physical_reclamation.push(format!(
                    "{}: pending; Manifold reclaims deleted objects under its own retention",
                    component.id
                )),
                _ => {}
            },
            Capability::Unsupported { reason } => not_covered.push(format!(
                "UNSUPPORTED {} {:?}: {reason}",
                component.id, component.store
            )),
            Capability::Excluded { reason } => not_covered.push(format!(
                "EXCLUDED {} {:?}: {reason}",
                component.id, component.store
            )),
        }
    }
    for repo in &plan.repos {
        for excluded in &repo.excluded_storage {
            not_covered.push(format!("repo {} {excluded}", repo.id));
        }
    }
    let rewritten: u64 = inputs
        .packs
        .iter()
        .map(|pack| match &pack.check {
            PackCheck::Checked { tasks, .. } => tasks.rewritten,
            PackCheck::Failed { .. } => 0,
        })
        .sum();
    if rewritten > 0 {
        physical_reclamation.push(format!(
            "{rewritten} packs rewritten; each original pack is released when its last link is gone"
        ));
    }
    // A report covering nothing certifies nothing.
    let clean = !plan.repos.is_empty()
        && !inputs.cells.is_empty()
        && inputs
            .cells
            .iter()
            .all(|cell| cell.state == CellState::Absent)
        && inputs.audits.iter().all(FamilyAudit::is_clean)
        && inputs.metadata.iter().all(RepoMetadataResult::is_clean)
        && inputs.packs.iter().all(PackResult::is_complete);
    let verdict = match (clean, not_covered.is_empty()) {
        (false, _) => Verdict::Incomplete,
        (true, true) => Verdict::Complete,
        (true, false) => Verdict::CompleteForCoveredScope,
    };
    Report {
        plan_digest: plan.digest.clone(),
        policy_version: plan.policy_version.clone(),
        started_at: inputs.started_at,
        finished_at: unix_now(),
        repo_scope,
        cells: inputs.cells,
        audits: inputs.audits,
        metadata: inputs.metadata,
        packs: inputs.packs,
        not_covered,
        physical_reclamation,
        operator_actions: vec![
            "Restart or redeploy every process holding cachelib entries of the retired types' mappings (derived data service, derivation workers, SCS and other readers) before any rebuild.".to_string(),
            "Confirm replicas of the metadata databases have converged before replica reads resume.".to_string(),
            "Keep producers, consumers, healing and packing of the affected stores fenced until the rebuild starts.".to_string(),
        ],
        verdict,
    }
}

pub fn report_path(report: &Report) -> PathBuf {
    PathBuf::from("reports").join(format!("verify-{}.json", report.finished_at))
}

fn metadata_line(md: &RepoMetadataResult) -> String {
    let state = match &md.check {
        MetadataCheck::Failed { error } => {
            return format!("Metadata repo {}: unknown ({error})", md.repo_id);
        }
        MetadataCheck::Checked { remaining, .. } if remaining.is_clean() => "clean".to_string(),
        MetadataCheck::Checked { remaining, .. } => format!(
            "enabled {:?}, sampled mapping rows {:?}",
            remaining.enabled, remaining.mapping_rows
        ),
    };
    let cache = match &md.check {
        MetadataCheck::Checked {
            final_pass: Some(pass),
            ..
        } => format!("{:?}", pass.batches),
        _ => "no final metadata pass recorded".to_string(),
    };
    format!(
        "Metadata repo {}: {state}; shared cache: {cache}",
        md.repo_id
    )
}

fn pack_lines(pack: &PackResult) -> Vec<String> {
    let title = format!("Packs {} repo {}", pack.component, pack.repo_id);
    match &pack.check {
        PackCheck::Failed { error } => vec![format!("{title}: unknown ({error})")],
        PackCheck::Checked { frozen: None, .. } => {
            vec![format!("{title}: pack tasks were never frozen")]
        }
        PackCheck::Checked {
            frozen: Some(frozen),
            ..
        } if frozen.tasks == 0 => vec![],
        PackCheck::Checked {
            frozen: Some(frozen),
            tasks,
        } => iter::once(format!(
            "{title}: {}/{} tasks complete, {} rewritten, {} packs not found",
            tasks.complete, frozen.tasks, tasks.rewritten, tasks.source_missing.count
        ))
        .chain(
            tasks
                .source_missing
                .examples
                .iter()
                .map(|pack_key| format!("  note: {}", missing_pack_note(pack_key))),
        )
        .collect(),
    }
}

/// Describes the report for an operator.
pub fn summarise(report: &Report) -> String {
    let mut lines = vec![format!("Plan {}", report.plan_digest)];
    lines.push(format!("Repositories: {}", report.repo_scope));
    let present: Vec<_> = report
        .cells
        .iter()
        .filter(|cell| cell.state != CellState::Absent)
        .collect();
    lines.push(format!(
        "Cells: {} checked, {} not verified absent",
        report.cells.len(),
        present.len()
    ));
    for cell in present {
        lines.push(format!(
            "  {:?} {} repo {} {}",
            cell.state, cell.component, cell.repo_id, cell.recipe
        ));
    }
    for audit in report.audits.iter().filter(|audit| !audit.is_clean()) {
        lines.push(format!(
            "Family {} in {} repo {}: {} registered keys, {} unknown keys{}",
            audit.family,
            audit.component,
            audit.repo_id,
            audit.registered,
            audit.unknown.count,
            audit
                .error
                .as_ref()
                .map_or(String::new(), |e| format!(", error: {e}"))
        ));
        for example in &audit.unknown.examples {
            lines.push(format!("  unknown key {example}"));
        }
    }
    lines.extend(report.metadata.iter().map(metadata_line));
    lines.extend(report.packs.iter().flat_map(pack_lines));
    for item in &report.not_covered {
        lines.push(format!("Not covered: {item}"));
    }
    for item in &report.physical_reclamation {
        lines.push(format!("Physical reclamation: {item}"));
    }
    for action in &report.operator_actions {
        lines.push(format!("Operator action: {action}"));
    }
    lines.push(format!("Verdict: {:?}", report.verdict));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mononoke_macros::mononoke;

    use super::*;
    use crate::metadata::CacheState;
    use crate::scan::tests::test_plan;

    fn absent_cell() -> CellResult {
        CellResult {
            component: "ctest".to_string(),
            repo_id: RepositoryId::new(1),
            recipe: "historymf.file".to_string(),
            state: CellState::Absent,
        }
    }

    fn metadata(final_pass: Option<FinalPass>) -> RepoMetadataResult {
        RepoMetadataResult {
            repo_id: RepositoryId::new(1),
            check: MetadataCheck::Checked {
                remaining: MetadataRemaining::default(),
                final_pass,
            },
        }
    }

    fn evicted(batches: u64) -> FinalPass {
        FinalPass {
            batches: BTreeMap::from([(CacheState::Evicted, batches)]),
            entries_evicted: batches,
        }
    }

    fn packs(frozen: Option<FreezeSummary>, tasks: TaskTotals) -> PackResult {
        PackResult {
            component: "ctest".to_string(),
            repo_id: RepositoryId::new(1),
            check: PackCheck::Checked { frozen, tasks },
        }
    }

    fn inputs(cells: Vec<CellResult>) -> ReportInputs {
        ReportInputs {
            started_at: 0,
            cells,
            audits: vec![],
            metadata: vec![metadata(Some(evicted(2)))],
            packs: vec![],
        }
    }

    #[mononoke::test]
    fn verdicts_never_promote_partial_or_failed_results() {
        let plan = test_plan(1);
        assert_eq!(
            build_report(&plan, inputs(vec![absent_cell()])).verdict,
            Verdict::CompleteForCoveredScope,
            "an explicit repository list is a partial scope"
        );

        let mut all = test_plan(1);
        all.request.repos = RepoSelection::AllConfigured;
        assert_eq!(
            build_report(&all, inputs(vec![absent_cell()])).verdict,
            Verdict::Complete
        );

        let mut with_s3 = all.clone();
        with_s3.components[0].capability = Capability::Excluded {
            reason: "s3".to_string(),
        };
        let report = build_report(&with_s3, inputs(vec![absent_cell()]));
        assert_eq!(report.verdict, Verdict::CompleteForCoveredScope);
        assert!(
            report
                .not_covered
                .iter()
                .any(|item| item.starts_with("EXCLUDED"))
        );

        for state in [
            CellState::Present,
            CellState::Unknown {
                error: "e".to_string(),
            },
        ] {
            let cell = CellResult {
                state,
                ..absent_cell()
            };
            assert_eq!(
                build_report(&all, inputs(vec![cell])).verdict,
                Verdict::Incomplete
            );
        }

        let mut uninvalidated = inputs(vec![absent_cell()]);
        uninvalidated.metadata = vec![metadata(Some(FinalPass {
            batches: BTreeMap::from([(CacheState::Evicted, 1), (CacheState::NoSharedCache, 1)]),
            entries_evicted: 1,
        }))];
        assert_eq!(
            build_report(&all, uninvalidated).verdict,
            Verdict::Incomplete
        );

        let mut nothing_deleted = inputs(vec![absent_cell()]);
        nothing_deleted.metadata = vec![metadata(Some(FinalPass::default()))];
        assert_eq!(
            build_report(&all, nothing_deleted).verdict,
            Verdict::Complete,
            "a final pass with nothing deleted leaves nothing to evict"
        );

        let mut no_final_pass = inputs(vec![absent_cell()]);
        no_final_pass.metadata = vec![metadata(None)];
        assert_eq!(
            build_report(&all, no_final_pass).verdict,
            Verdict::Incomplete,
            "evictions before the blobs were gone prove nothing"
        );

        let mut unreadable_metadata = inputs(vec![absent_cell()]);
        unreadable_metadata.metadata = vec![RepoMetadataResult {
            repo_id: RepositoryId::new(1),
            check: MetadataCheck::Failed {
                error: "connection limit".to_string(),
            },
        }];
        assert_eq!(
            build_report(&all, unreadable_metadata).verdict,
            Verdict::Incomplete
        );

        let mut unknown_format = inputs(vec![absent_cell()]);
        let mut unknown = Sampled::default();
        unknown.note("repo0001.historymf.v9.x".to_string());
        unknown_format.audits.push(FamilyAudit {
            component: "ctest".to_string(),
            repo_id: RepositoryId::new(1),
            family: "historymf.".to_string(),
            registered: 0,
            unknown,
            error: None,
        });
        assert_eq!(
            build_report(&all, unknown_format).verdict,
            Verdict::Incomplete
        );

        assert_eq!(
            build_report(&all, inputs(vec![])).verdict,
            Verdict::Incomplete,
            "no checked cells cannot be complete"
        );

        let frozen = FreezeSummary {
            tasks: 2,
            packed_aliases: 3,
        };
        let one_task = TaskTotals {
            complete: 1,
            ..Default::default()
        };
        let mut unfinished = inputs(vec![absent_cell()]);
        unfinished
            .packs
            .push(packs(Some(frozen.clone()), one_task.clone()));
        assert_eq!(build_report(&all, unfinished).verdict, Verdict::Incomplete);

        let mut finished = inputs(vec![absent_cell()]);
        let mut both = one_task.clone();
        both.add(&one_task);
        finished
            .packs
            .push(packs(Some(frozen.clone()), both.clone()));
        assert_eq!(build_report(&all, finished).verdict, Verdict::Complete);

        let mut source_missing = inputs(vec![absent_cell()]);
        let mut missing = both;
        missing.source_missing.note("multiblob-x.pack".to_string());
        source_missing.packs.push(packs(Some(frozen), missing));
        assert_eq!(
            build_report(&all, source_missing).verdict,
            Verdict::Complete,
            "a task whose pack was already gone is complete"
        );

        let mut never_frozen = inputs(vec![absent_cell()]);
        never_frozen.packs.push(packs(None, TaskTotals::default()));
        assert_eq!(
            build_report(&all, never_frozen).verdict,
            Verdict::Incomplete,
            "packed aliases are unhandled until apply freezes their tasks"
        );

        let mut unreadable_packs = inputs(vec![absent_cell()]);
        unreadable_packs.packs.push(PackResult {
            component: "ctest".to_string(),
            repo_id: RepositoryId::new(1),
            check: PackCheck::Failed {
                error: "corrupt journal".to_string(),
            },
        });
        assert_eq!(
            build_report(&all, unreadable_packs).verdict,
            Verdict::Incomplete
        );
    }
}
