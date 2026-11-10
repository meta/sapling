/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Rewriting packs that hold retired keys.
//!
//! All retired aliases linking to one original pack in a component form one
//! task, frozen from the complete inventory before anything is deleted.  One
//! of them, elected by a stable hash ranking, owns the task: the task runs
//! with its owner's partition, and every other alias waits for the task.
//!
//! A task decodes the pack's surviving entries while the retired entries
//! they may use as dictionaries are still readable, publishes a replacement
//! pack holding only the survivors, and atomically relinks each surviving
//! alias that still links to the original pack.  Only after the survivors
//! are verified does it unlink the retired aliases, the owner last, so an
//! interrupted task can always be resumed from the owner.  Replacement is
//! atomic per alias, not across the pack.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::iter;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use blobstore::Blobstore;
use clap::Args;
use futures::future;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use metaconfig_types::PackFormat;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreBytes;
use mononoke_types::RepositoryId;
use mononoke_types::hash::Context as HashContext;
use packblob::EmptyPack;
use packblob::Pack;
use packblob::PackBlob;
use packblob::PackContents;
use packblob::StoredValue;
use serde::Deserialize;
use serde::Serialize;
use tracing::warn;

use crate::apply::ApplyContext;
use crate::apply::FailedKey;
use crate::apply::unlink_keys;
use crate::policy::PhysicalEncoding;
use crate::policy::parse_logical_key;
use crate::scan::Cell;
use crate::scan::InventoryRecord;
use crate::scan::Sampled;
use crate::scan::StoredForm;
use crate::scan::partition_file;
use crate::scan::partition_hexes;
use crate::scope::PACK_OWNER_ELECTION;
use crate::state::Journal;
use crate::state::StagedLines;
use crate::state::StateDir;

/// Prefix of replacement pack keys; the same as the packer's.
const REPACK_PREFIX: &str = "multiblob-";

/// Packed records are spread over this many buckets while freezing, so one
/// bucket at a time is grouped in memory.
const FREEZE_BUCKETS: usize = 256;

#[derive(Args, Clone, Debug)]
pub struct PackOptions {
    /// zstd level for replacement packs
    #[clap(long, default_value_t = 19, value_parser = clap::value_parser!(i32).range(1..=22))]
    pub repack_zstd_level: i32,
    /// Pack rewrite tasks run at the same time within a partition
    #[clap(long, default_value_t = NonZeroUsize::new(4).expect("4 is non-zero"))]
    pub pack_concurrency: NonZeroUsize,
}

/// The rewrite of one original pack in one component.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackTask {
    pub task_id: String,
    pub pack_key: String,
    /// Logical keys of the retired aliases linking to the pack in the
    /// inventory, sorted.
    pub eligible: Vec<String>,
    pub owner: String,
}

impl PackTask {
    fn aliases(&self) -> impl Iterator<Item = String> + '_ {
        self.eligible
            .iter()
            .map(|key| PhysicalEncoding::PackEnvelope.physical_key(key))
    }

    fn owner_alias(&self) -> String {
        PhysicalEncoding::PackEnvelope.physical_key(&self.owner)
    }
}

fn length_prefixed(context: &mut HashContext, fields: &[&str]) {
    for field in fields {
        context.update((field.len() as u64).to_le_bytes());
        context.update(field.as_bytes());
    }
}

/// The rank of `logical_key` in the election for one original pack.
pub fn owner_score(component_id: &str, pack_key: &str, logical_key: &str) -> [u8; 32] {
    let mut context = HashContext::new(PACK_OWNER_ELECTION.as_bytes());
    length_prefixed(&mut context, &[component_id, pack_key, logical_key]);
    context.finish().into_inner()
}

/// The lowest-scoring key, ties broken by the key bytes.
fn elect_with<'a, S: Ord>(eligible: &'a [String], score: impl Fn(&str) -> S) -> Option<&'a String> {
    eligible
        .iter()
        .map(|key| (score(key.as_str()), key))
        .min()
        .map(|(_, key)| key)
}

pub fn elect_owner<'a>(
    component_id: &str,
    pack_key: &str,
    eligible: &'a [String],
) -> Option<&'a String> {
    elect_with(eligible, |key| owner_score(component_id, pack_key, key))
}

fn task_id(plan_digest: &str, component_id: &str, repo_id: RepositoryId, pack_key: &str) -> String {
    let mut context = HashContext::new(b"pack-task-v1");
    length_prefixed(
        &mut context,
        &[
            plan_digest,
            component_id,
            &repo_id.id().to_string(),
            pack_key,
        ],
    );
    context.finish().to_hex().to_string()
}

fn packs_dir(component_id: &str, repo_id: RepositoryId) -> PathBuf {
    PathBuf::from("packs")
        .join(component_id)
        .join(repo_id.id().to_string())
}

/// Tasks owned by a partition.
fn owned_tasks_path(cell: &Cell<'_>, hex: &str) -> PathBuf {
    packs_dir(&cell.component.id, cell.repo_id)
        .join("frozen")
        .join(cell.recipe.id)
        .join(partition_file(hex, ".jsonl"))
}

fn task_journals_dir(component_id: &str, repo_id: RepositoryId) -> PathBuf {
    packs_dir(component_id, repo_id).join("journal")
}

/// Journals are spread over directories by the first two digits of the
/// task id.
fn task_journal_path(component_id: &str, repo_id: RepositoryId, task_id: &str) -> PathBuf {
    task_journals_dir(component_id, repo_id)
        .join(&task_id[..2])
        .join(format!("{task_id}.jsonl"))
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreezeSummary {
    pub tasks: u64,
    pub packed_aliases: u64,
}

#[derive(Serialize, Deserialize)]
struct PackedAlias {
    pack_key: String,
    logical: String,
}

fn freeze_bucket(pack_key: &str) -> usize {
    let mut context = HashContext::new(b"freeze-bucket");
    context.update(pack_key.as_bytes());
    usize::from(context.finish().as_ref()[0]) % FREEZE_BUCKETS
}

/// Groups the packed aliases of one component and repo, whose cells are
/// `cells`, into pack tasks, once every partition of those cells is
/// inventoried.  Later calls return the frozen result.
pub async fn freeze(
    state: &StateDir,
    plan_digest: &str,
    digits: u8,
    cells: &[Cell<'_>],
) -> Result<FreezeSummary> {
    let Some(first) = cells.first() else {
        return Ok(FreezeSummary::default());
    };
    let (component_id, repo_id) = (&first.component.id, first.repo_id);
    let dir = packs_dir(component_id, repo_id);
    let frozen = dir.join("frozen");
    if let Some(summary) = state.read_json(&frozen.join("summary.json")).await? {
        return Ok(summary);
    }
    // Tasks only run after a freeze has published its summary, so a frozen
    // directory without one was never used.
    state.remove_dir_all(&frozen).await?;
    let staging = dir.join("freezing");
    state.remove_dir_all(&staging).await?;
    let bucket_path = |bucket: usize| staging.join("buckets").join(format!("{bucket:02x}.jsonl"));

    let mut buckets: HashMap<usize, StagedLines> = HashMap::new();
    for cell in cells {
        for hex in partition_hexes(digits) {
            if cell.committed(state, &hex).await?.is_none() {
                bail!("{cell} partition {hex} is not inventoried");
            }
            let records: Vec<InventoryRecord> =
                state.read_json_lines(&cell.inventory_path(&hex)).await?;
            for record in records {
                let StoredForm::Packed { pack_key } = record.form else {
                    continue;
                };
                let logical = record
                    .key
                    .strip_suffix(packblob::ENVELOPE_SUFFIX)
                    .with_context(|| format!("packed key {} has no envelope suffix", record.key))?
                    .to_string();
                let bucket = freeze_bucket(&pack_key);
                let staged = match buckets.entry(bucket) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        entry.insert(state.staged_lines(&bucket_path(bucket)).await?)
                    }
                };
                staged.push(&PackedAlias { pack_key, logical }).await?;
            }
        }
    }
    for staged in buckets.into_values() {
        staged.commit().await?;
    }

    let mut summary = FreezeSummary::default();
    for bucket in 0..FREEZE_BUCKETS {
        let aliases: Vec<PackedAlias> = state.read_json_lines(&bucket_path(bucket)).await?;
        let mut packs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for alias in aliases {
            packs.entry(alias.pack_key).or_default().push(alias.logical);
        }
        let mut by_owner: BTreeMap<PathBuf, Vec<PackTask>> = BTreeMap::new();
        for (pack_key, mut eligible) in packs {
            eligible.sort();
            eligible.dedup();
            let owner = elect_owner(component_id, &pack_key, &eligible)
                .context("a pack task has at least one alias")?
                .clone();
            let parsed = parse_logical_key(&owner)
                .with_context(|| format!("owner {owner} matches no recipe"))?;
            let owner_cell = cells
                .iter()
                .find(|cell| cell.recipe.id == parsed.recipe.id)
                .with_context(|| format!("owner {owner} is in no cell of this run"))?;
            summary.tasks += 1;
            summary.packed_aliases += eligible.len() as u64;
            by_owner
                .entry(
                    staging
                        .join("frozen")
                        .join(owner_cell.recipe.id)
                        .join(partition_file(parsed.partition(digits), ".jsonl")),
                )
                .or_default()
                .push(PackTask {
                    task_id: task_id(plan_digest, component_id, repo_id, &pack_key),
                    pack_key,
                    eligible,
                    owner,
                });
        }
        for (path, tasks) in by_owner {
            state.append_lines(&path, &tasks).await?;
        }
    }
    state
        .write_json(&staging.join("frozen").join("summary.json"), &summary)
        .await?;
    state.rename(&staging.join("frozen"), &frozen).await?;
    state.remove_dir_all(&staging).await?;
    Ok(summary)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub pack_key: String,
    pub retired_unlinked: u64,
    pub retired_already_absent: u64,
    pub survivors_relinked: u64,
    pub replacement_pack_key: Option<String>,
    /// No eligible alias still linked to the original pack, so it could not
    /// be read or rewritten.  Surviving links may still hold retired bytes.
    pub source_missing: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum PackEvent {
    /// A replacement holding `survivors` is about to be published.
    Planned {
        survivors: Vec<String>,
        replacement_pack_key: String,
    },
    /// Every survivor links to the replacement, or there are none, so the
    /// retired aliases can be unlinked.
    ReadyToUnlink {
        survivors_relinked: u64,
        replacement_pack_key: Option<String>,
    },
    Complete {
        outcome: TaskOutcome,
    },
}

/// How far a task got, replayed from its journal.
enum Stage {
    Fresh,
    Planned {
        survivors: Vec<String>,
        replacement: String,
    },
    ReadyToUnlink {
        survivors_relinked: u64,
        replacement: Option<String>,
    },
    Complete(TaskOutcome),
}

fn replay(events: Vec<PackEvent>) -> Stage {
    events
        .into_iter()
        .fold(Stage::Fresh, |stage, event| match (stage, event) {
            (Stage::Complete(outcome), _) | (_, PackEvent::Complete { outcome }) => {
                Stage::Complete(outcome)
            }
            (
                _,
                PackEvent::Planned {
                    survivors,
                    replacement_pack_key,
                },
            ) => Stage::Planned {
                survivors,
                replacement: replacement_pack_key,
            },
            (
                _,
                PackEvent::ReadyToUnlink {
                    survivors_relinked,
                    replacement_pack_key,
                },
            ) => Stage::ReadyToUnlink {
                survivors_relinked,
                replacement: replacement_pack_key,
            },
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskResult {
    Complete(TaskOutcome),
    /// The pack or its aliases changed in a way the task must not override.
    Conflict(String),
}

/// Reads the original pack through the first eligible alias, owner first,
/// that still links to it.
async fn find_source(actx: &ApplyContext<'_>, task: &PackTask) -> Result<Option<PackContents>> {
    let owner_alias = task.owner_alias();
    let candidates =
        iter::once(owner_alias.clone()).chain(task.aliases().filter(|alias| *alias != owner_alias));
    for alias in candidates {
        if let Some(contents) = linked_pack(actx, &alias).await?
            && contents.pack_key() == task.pack_key
        {
            return Ok(Some(contents));
        }
    }
    Ok(None)
}

/// The pack `alias` links to, if any.  An envelope that cannot be read is
/// an error, not a missing link.
async fn linked_pack(actx: &ApplyContext<'_>, alias: &str) -> Result<Option<PackContents>> {
    let Some(raw) = actx.store.get(actx.ctx, alias).await? else {
        return Ok(None);
    };
    match packblob::inspect_stored_value(raw)
        .with_context(|| format!("reading the envelope of {alias}"))?
    {
        StoredValue::Packed(contents) => Ok(Some(contents)),
        StoredValue::Single => Ok(None),
    }
}

/// The pack each of `aliases` currently links to, if any.
async fn linked_pack_keys(
    actx: &ApplyContext<'_>,
    aliases: Vec<String>,
) -> Result<Vec<(String, Option<String>)>> {
    stream::iter(aliases)
        .map(|alias| async move {
            let pack_key = linked_pack(actx, &alias)
                .await?
                .map(|contents| contents.pack_key().to_string());
            Ok::<_, anyhow::Error>((alias, pack_key))
        })
        .buffered(actx.unlink.concurrency.get())
        .try_collect()
        .await
}

fn alias_of(actx: &ApplyContext<'_>, entry: &str) -> String {
    PhysicalEncoding::PackEnvelope.physical_key(&format!("{}{entry}", actx.cell.repo_id.prefix()))
}

/// The pack's non-retired entries whose aliases still link to it.
async fn live_survivors(
    actx: &ApplyContext<'_>,
    task: &PackTask,
    contents: &PackContents,
) -> Result<Vec<String>> {
    let prefix = actx.cell.repo_id.prefix();
    let candidates: Vec<String> = contents
        .entry_keys()
        .filter(|entry| {
            !actx
                .selection
                .selects(actx.cell.repo_id, &format!("{prefix}{entry}"))
        })
        .map(String::from)
        .collect();
    let links =
        linked_pack_keys(actx, candidates.iter().map(|e| alias_of(actx, e)).collect()).await?;
    Ok(candidates
        .into_iter()
        .zip(links)
        .filter(|(_, (_, linked))| linked.as_deref() == Some(task.pack_key.as_str()))
        .map(|(entry, _)| entry)
        .collect())
}

fn build_pack(
    zstd_level: i32,
    survivors: &[String],
    values: &HashMap<String, BlobstoreBytes>,
) -> Result<Pack> {
    let value = |key: &String| {
        values
            .get(key)
            .cloned()
            .with_context(|| format!("no decoded value for survivor {key}"))
    };
    let (base, rest) = survivors.split_first().context("no survivors to pack")?;
    let mut pack = EmptyPack::new(zstd_level).add_base_blob(base.clone(), value(base)?)?;
    for key in rest {
        pack.add_delta_blob(base.clone(), key.clone(), value(key)?)?;
    }
    Ok(pack)
}

/// A replacement pack and the survivor values it was built from.
struct Replacement {
    pack: Pack,
    values: HashMap<String, BlobstoreBytes>,
}

/// Decodes `survivors` and builds their replacement pack on a blocking
/// thread, as both are CPU-bound.
async fn rebuild(
    contents: PackContents,
    survivors: Vec<String>,
    zstd_level: i32,
) -> Result<Replacement> {
    mononoke::spawn_blocking(move || {
        let refs: Vec<&str> = survivors.iter().map(String::as_str).collect();
        let values = contents.decode_entries(&refs)?;
        let pack = build_pack(zstd_level, &survivors, &values)?;
        anyhow::Ok(Replacement { pack, values })
    })
    .await?
}

/// Why the survivors cannot be relinked, if one of them no longer links to
/// the original or the replacement pack.
async fn survivor_conflict(
    actx: &ApplyContext<'_>,
    task: &PackTask,
    survivors: &[String],
    replacement: &str,
) -> Result<Option<String>> {
    let links =
        linked_pack_keys(actx, survivors.iter().map(|e| alias_of(actx, e)).collect()).await?;
    let expected = [task.pack_key.as_str(), replacement];
    Ok(links
        .into_iter()
        .find(|(_, linked)| !linked.as_deref().is_some_and(|key| expected.contains(&key)))
        .map(|(alias, _)| {
            format!("surviving alias {alias} no longer links to the original or replacement pack")
        }))
}

/// Publishes `replacement` under `replacement_key` and checks that every
/// survivor links to it and reads back its original value.
async fn publish(
    actx: &ApplyContext<'_>,
    survivors: &[String],
    replacement_key: &str,
    replacement: Replacement,
) -> Result<()> {
    let Replacement { pack, values } = replacement;
    let values = &values;
    let prefix = actx.cell.repo_id.prefix();
    let packblob = PackBlob::new(actx.store.blobstore(), PackFormat::Raw);
    let stored = packblob
        .put_packed(actx.ctx, pack, prefix.clone(), REPACK_PREFIX.to_string())
        .await?;
    if stored != replacement_key {
        bail!("replacement pack was stored as {stored}, expected {replacement_key}");
    }
    stream::iter(survivors)
        .map(Ok)
        .try_for_each_concurrent(actx.unlink.concurrency.get(), |entry| {
            let (packblob, prefix) = (&packblob, &prefix);
            async move {
                let read = packblob
                    .get(actx.ctx, &format!("{prefix}{entry}"))
                    .await?
                    .with_context(|| format!("survivor {entry} is missing after relinking"))?;
                if Some(&read.into_bytes()) != values.get(entry) {
                    bail!("survivor {entry} reads back a different value after relinking");
                }
                anyhow::Ok(())
            }
        })
        .await?;
    let relinked =
        linked_pack_keys(actx, survivors.iter().map(|e| alias_of(actx, e)).collect()).await?;
    if let Some((alias, _)) = relinked
        .iter()
        .find(|(_, linked)| linked.as_deref() != Some(replacement_key))
    {
        bail!("survivor {alias} does not link to the replacement pack");
    }
    Ok(())
}

/// Unlinks the task's retired aliases, every non-owner first and the owner
/// last.  Fails, keeping the owner, if any non-owner could not be unlinked.
async fn unlink_aliases(actx: &ApplyContext<'_>, task: &PackTask) -> Result<(u64, u64)> {
    let owner_alias = task.owner_alias();
    let others: Vec<String> = task
        .aliases()
        .filter(|alias| *alias != owner_alias)
        .collect();
    let others = unlink_keys(actx.ctx, actx.store, others, actx.unlink).await;
    fail_on(&others.failed)?;
    let owner = unlink_keys(actx.ctx, actx.store, vec![owner_alias], actx.unlink).await;
    fail_on(&owner.failed)?;
    Ok((
        others.unlinked + owner.unlinked,
        others.already_absent + owner.already_absent,
    ))
}

fn fail_on(failed: &[FailedKey]) -> Result<()> {
    match failed.first() {
        Some(first) => bail!(
            "{} aliases could not be unlinked, e.g. {}: {}",
            failed.len(),
            first.key,
            first.error
        ),
        None => Ok(()),
    }
}

/// Journals that the retired aliases can be unlinked, returning what the
/// task's outcome needs.
async fn ready_to_unlink(
    journal: &mut Journal,
    survivors_relinked: u64,
    replacement: Option<String>,
) -> Result<(u64, Option<String>, bool)> {
    journal
        .append(&PackEvent::ReadyToUnlink {
            survivors_relinked,
            replacement_pack_key: replacement.clone(),
        })
        .await?;
    Ok((survivors_relinked, replacement, false))
}

/// Runs, or resumes, one pack task.
pub async fn execute_task(actx: &ApplyContext<'_>, task: &PackTask) -> Result<TaskResult> {
    let journal_path = task_journal_path(&actx.cell.component.id, actx.cell.repo_id, &task.task_id);
    let stage = replay(actx.state.read_journal(&journal_path).await?);
    let mut journal = actx.state.journal(&journal_path);
    let level = actx.pack.repack_zstd_level;

    let (survivors_relinked, replacement, source_missing) = match stage {
        Stage::Complete(outcome) => return Ok(TaskResult::Complete(outcome)),
        Stage::ReadyToUnlink {
            survivors_relinked,
            replacement,
        } => (survivors_relinked, replacement, false),
        Stage::Planned {
            survivors,
            replacement,
        } => {
            let Some(contents) = find_source(actx, task).await? else {
                return Ok(TaskResult::Conflict(
                    "the original pack vanished before its replacement was published".to_string(),
                ));
            };
            if let Some(conflict) = survivor_conflict(actx, task, &survivors, &replacement).await? {
                return Ok(TaskResult::Conflict(conflict));
            }
            let rebuilt = rebuild(contents, survivors.clone(), level).await?;
            publish(actx, &survivors, &replacement, rebuilt).await?;
            ready_to_unlink(&mut journal, survivors.len() as u64, Some(replacement)).await?
        }
        Stage::Fresh => match find_source(actx, task).await? {
            // No alias links to the pack any more: the retired aliases must
            // go regardless of their values.
            None => (0, None, true),
            Some(contents) => {
                let survivors = live_survivors(actx, task, &contents).await?;
                if survivors.is_empty() {
                    ready_to_unlink(&mut journal, 0, None).await?
                } else {
                    let rebuilt = rebuild(contents, survivors.clone(), level).await?;
                    let replacement = rebuilt.pack.pack_key(REPACK_PREFIX);
                    if let Some(conflict) =
                        survivor_conflict(actx, task, &survivors, &replacement).await?
                    {
                        return Ok(TaskResult::Conflict(conflict));
                    }
                    journal
                        .append(&PackEvent::Planned {
                            survivors: survivors.clone(),
                            replacement_pack_key: replacement.clone(),
                        })
                        .await?;
                    publish(actx, &survivors, &replacement, rebuilt).await?;
                    ready_to_unlink(&mut journal, survivors.len() as u64, Some(replacement)).await?
                }
            }
        },
    };

    let (unlinked, absent) = unlink_aliases(actx, task).await?;
    let outcome = TaskOutcome {
        pack_key: task.pack_key.clone(),
        retired_unlinked: unlinked,
        retired_already_absent: absent,
        survivors_relinked,
        replacement_pack_key: replacement,
        source_missing,
    };
    journal
        .append(&PackEvent::Complete {
            outcome: outcome.clone(),
        })
        .await?;
    Ok(TaskResult::Complete(outcome))
}

/// The frozen tasks of a component and repo, if frozen.
pub async fn frozen_summary(
    state: &StateDir,
    component_id: &str,
    repo_id: RepositoryId,
) -> Result<Option<FreezeSummary>> {
    state
        .read_json(
            &packs_dir(component_id, repo_id)
                .join("frozen")
                .join("summary.json"),
        )
        .await
}

/// Task journals read at the same time.
const JOURNAL_READS: usize = 64;

/// Outcomes of the completed tasks of a component and repo, by pack key.
pub async fn completed_tasks(
    state: &StateDir,
    component_id: &str,
    repo_id: RepositoryId,
) -> Result<Vec<TaskOutcome>> {
    let dir = task_journals_dir(component_id, repo_id);
    let journals: Vec<PathBuf> = stream::iter(state.list_dir(&dir).await?)
        .map(|shard| {
            let shard_dir = dir.join(shard);
            async move {
                let names = state.list_dir(&shard_dir).await?;
                anyhow::Ok(
                    names
                        .into_iter()
                        .map(|name| shard_dir.join(name))
                        .collect::<Vec<_>>(),
                )
            }
        })
        .buffer_unordered(JOURNAL_READS)
        .try_concat()
        .await?;
    let mut outcomes: Vec<TaskOutcome> = stream::iter(journals)
        .map(|path| async move {
            let events: Vec<PackEvent> = state.read_journal(&path).await?;
            anyhow::Ok(match replay(events) {
                Stage::Complete(outcome) => Some(outcome),
                _ => None,
            })
        })
        .buffer_unordered(JOURNAL_READS)
        .try_filter_map(|outcome| future::ready(Ok(outcome)))
        .try_collect()
        .await?;
    outcomes.sort_by(|a, b| a.pack_key.cmp(&b.pack_key));
    Ok(outcomes)
}

/// Describes a task that found no alias still linking to its pack.
pub fn missing_pack_note(pack_key: &str) -> String {
    format!(
        "pack {pack_key} not found: its retired aliases were unlinked; any live entries it held were already unreadable"
    )
}

/// Totals over pack tasks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTotals {
    pub complete: u64,
    pub rewritten: u64,
    pub retired_unlinked: u64,
    pub retired_already_absent: u64,
    pub survivors_relinked: u64,
    /// Packs no alias linked to any more, so their tasks only unlinked the
    /// retired aliases.
    pub source_missing: Sampled<String>,
    pub conflicts: Sampled<String>,
    pub failed: Sampled<String>,
}

impl TaskTotals {
    pub fn record(&mut self, outcome: &TaskOutcome) {
        self.complete += 1;
        self.rewritten += u64::from(outcome.replacement_pack_key.is_some());
        self.retired_unlinked += outcome.retired_unlinked;
        self.retired_already_absent += outcome.retired_already_absent;
        self.survivors_relinked += outcome.survivors_relinked;
        if outcome.source_missing {
            self.source_missing.note(outcome.pack_key.clone());
        }
    }

    pub fn add(&mut self, other: &TaskTotals) {
        self.complete += other.complete;
        self.rewritten += other.rewritten;
        self.retired_unlinked += other.retired_unlinked;
        self.retired_already_absent += other.retired_already_absent;
        self.survivors_relinked += other.survivors_relinked;
        self.source_missing.merge(&other.source_missing);
        self.conflicts.merge(&other.conflicts);
        self.failed.merge(&other.failed);
    }

    /// Every task completed, so its retired aliases are gone.
    pub fn is_finished(&self) -> bool {
        self.conflicts.is_empty() && self.failed.is_empty()
    }
}

/// Runs the tasks owned by one partition.
pub async fn execute_owned_tasks(actx: &ApplyContext<'_>, hex: &str) -> Result<TaskTotals> {
    let tasks: Vec<PackTask> = actx
        .state
        .read_json_lines(&owned_tasks_path(&actx.cell, hex))
        .await?;
    let totals = stream::iter(tasks)
        .map(|task| async move {
            // A task resumes from its journal, so a failed one can run again.
            let result = actx
                .unlink
                .retry
                .run(
                    |_| execute_task(actx, &task),
                    |attempt, error| {
                        warn!(
                            "pack task {} failed on attempt {attempt}, retrying: {error:#}",
                            task.task_id
                        )
                    },
                )
                .await;
            (task, result)
        })
        .buffer_unordered(actx.pack.pack_concurrency.get())
        .fold(TaskTotals::default(), |mut totals, (task, result)| {
            let describe =
                |issue: String| format!("pack {} (task {}): {issue}", task.pack_key, task.task_id);
            match result {
                Ok(TaskResult::Complete(outcome)) => totals.record(&outcome),
                Ok(TaskResult::Conflict(reason)) => totals.conflicts.note(describe(reason)),
                Err(e) => totals.failed.note(describe(format!("{e:#}"))),
            }
            future::ready(totals)
        })
        .await;
    Ok(totals)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::num::NonZeroU32;
    use std::sync::Arc;

    use blobstore_factory::ReadOnlyStorage;
    use blobstore_factory::ThrottleOptions;
    use context::CoreContext;
    use fbinit::FacebookInit;
    use memblob::Memblob;

    use super::*;
    use crate::adapters::ComponentStore;
    use crate::apply::tests::TestApply;
    use crate::policy::RECIPES;
    use crate::scan;
    use crate::scan::InventoryContext;
    use crate::scan::inventory_partition;
    use crate::scan::tests::bytes;
    use crate::scan::tests::concurrency;
    use crate::scan::tests::hash;
    use crate::scan::tests::test_cell;
    use crate::scan::tests::test_plan;
    use crate::scan::tests::test_state;
    use crate::scope::Plan;

    fn keys(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("repo0001.historymf.file.blake2.{i:064x}"))
            .collect()
    }

    #[mononoke::test]
    fn election_is_stable_under_permutation() {
        let eligible = keys(20);
        let owner = elect_owner("c1", "multiblob-x.pack", &eligible).cloned();
        let mut reversed = eligible.clone();
        reversed.reverse();
        assert_eq!(
            elect_owner("c1", "multiblob-x.pack", &reversed).cloned(),
            owner
        );
        let mut rotated = eligible.clone();
        rotated.rotate_left(7);
        assert_eq!(
            elect_owner("c1", "multiblob-x.pack", &rotated).cloned(),
            owner
        );
        assert_ne!(
            owner.as_ref(),
            eligible.first(),
            "these inputs should not elect the lexicographically first key"
        );
        assert!(elect_owner("c1", "p", &[]).is_none());
    }

    #[mononoke::test]
    fn election_depends_on_component_and_pack() {
        let eligible = keys(64);
        let owners: BTreeSet<_> = (0..16)
            .map(|n| elect_owner(&format!("c{n}"), "multiblob-x.pack", &eligible).cloned())
            .chain(
                (0..16)
                    .map(|n| elect_owner("c1", &format!("multiblob-{n}.pack"), &eligible).cloned()),
            )
            .collect();
        assert!(
            owners.len() > 8,
            "owners should vary with the task identity"
        );
    }

    #[mononoke::test]
    fn ties_are_broken_by_key_bytes() {
        let eligible = vec!["b".to_string(), "a".to_string(), "c".to_string()];
        assert_eq!(
            elect_with(&eligible, |_| 0u8).map(String::as_str),
            Some("a")
        );
    }

    #[mononoke::test]
    fn ownership_is_not_biased_to_the_first_partition() {
        // 4096 packs, each with aliases spread over all 16 first-digit
        // partitions.
        let mut first_digit_counts = [0usize; 16];
        for pack in 0..4096u32 {
            let eligible: Vec<String> = (0..16u32)
                .map(|digit| {
                    format!(
                        "repo0001.historymf.file.blake2.{digit:x}{:063x}",
                        u64::from(pack) * 31 + u64::from(digit)
                    )
                })
                .collect();
            let owner =
                elect_owner("c1", &format!("multiblob-{pack}.pack"), &eligible).expect("non-empty");
            let digit = owner
                .chars()
                .nth("repo0001.historymf.file.blake2.".len())
                .and_then(|c| c.to_digit(16))
                .expect("hex digit");
            first_digit_counts[digit as usize] += 1;
        }
        let expected = 4096 / 16;
        for (digit, count) in first_digit_counts.iter().enumerate() {
            assert!(
                *count > expected / 2 && *count < expected * 3 / 2,
                "partition {digit:x} owns {count} of 4096 tasks, expected about {expected}"
            );
        }
    }

    struct Fixture {
        memblob: Arc<Memblob>,
        packblob: PackBlob<Arc<Memblob>>,
        retired: Vec<String>,
        survivor: String,
        dict_only: String,
        moved: String,
        pack_key: String,
    }

    /// A pack in repo 1 holding two retired history manifest entries, a
    /// surviving unode whose dictionary is a retired entry, a surviving
    /// entry whose alias has moved to another pack, and a non-retired entry
    /// with no alias at all.
    async fn mixed_pack(ctx: &CoreContext) -> Result<Fixture> {
        let memblob = Arc::new(Memblob::default());
        let packblob = PackBlob::new(memblob.clone(), PackFormat::Raw);
        let retired: Vec<String> = [0x01u8, 0x02]
            .iter()
            .map(|seed| format!("historymf.file.blake2.{}", hash(*seed)))
            .collect();
        let survivor = format!("fileunode.blake2.{}", hash(0x03));
        let dict_only = format!("fileunode.blake2.{}", hash(0x04));
        let moved = format!("fileunode.blake2.{}", hash(0x05));
        let mut pack =
            EmptyPack::new(3).add_base_blob(retired[0].clone(), bytes("retired base"))?;
        pack.add_delta_blob(retired[0].clone(), retired[1].clone(), bytes("retired two"))?;
        pack.add_delta_blob(
            retired[0].clone(),
            survivor.clone(),
            bytes("surviving unode"),
        )?;
        pack.add_delta_blob(
            retired[0].clone(),
            dict_only.clone(),
            bytes("dictionary only"),
        )?;
        pack.add_delta_blob(retired[0].clone(), moved.clone(), bytes("moved"))?;
        let pack_key = packblob
            .put_packed(ctx, pack, "repo0001.".to_string(), "multiblob-".to_string())
            .await?;
        // The dictionary-only entry has no alias, and the moved alias now
        // holds a value of its own.
        memblob
            .unlink(ctx, &format!("repo0001.{dict_only}.pack"))
            .await?;
        packblob
            .put_explicit(
                ctx,
                format!("repo0001.{moved}"),
                bytes("moved"),
                blobstore::PutBehaviour::Overwrite,
            )
            .await?;
        Ok(Fixture {
            memblob,
            packblob,
            retired,
            survivor,
            dict_only,
            moved,
            pack_key,
        })
    }

    /// Inventories every partition of the plan's cells and freezes their
    /// pack tasks.
    async fn freeze_all(
        ctx: &CoreContext,
        state: &StateDir,
        plan: &Plan,
        store: &ComponentStore,
    ) -> Result<FreezeSummary> {
        let cells: Vec<Cell<'_>> = scan::cells(plan).collect();
        for cell in &cells {
            for hex in partition_hexes(plan.request.partition_digits) {
                inventory_partition(
                    InventoryContext {
                        ctx,
                        state,
                        store,
                        concurrency: concurrency(2),
                    },
                    *cell,
                    &hex,
                )
                .await?;
            }
        }
        freeze(state, &plan.digest, plan.request.partition_digits, &cells).await
    }

    /// Every frozen task owned by a partition of `cell`.
    async fn owned_tasks(state: &StateDir, plan: &Plan, cell: Cell<'_>) -> Result<Vec<PackTask>> {
        let mut tasks = Vec::new();
        for hex in partition_hexes(plan.request.partition_digits) {
            tasks.extend(
                state
                    .read_json_lines::<PackTask>(&owned_tasks_path(&cell, &hex))
                    .await?,
            );
        }
        Ok(tasks)
    }

    async fn frozen_task(
        ctx: &CoreContext,
        state: &StateDir,
        plan: &Plan,
        store: &ComponentStore,
    ) -> Result<PackTask> {
        let stale = packs_dir(&plan.components[0].id, RepositoryId::new(1))
            .join("frozen")
            .join("stale.jsonl");
        state.write_atomic(&stale, b"{}\n").await?;
        let summary = freeze_all(ctx, state, plan, store).await?;
        assert_eq!(
            summary,
            FreezeSummary {
                tasks: 1,
                packed_aliases: 2
            }
        );
        assert!(
            !state.exists(&stale).await?,
            "an unpublished frozen directory is replaced"
        );
        let mut tasks = owned_tasks(state, plan, test_cell(plan, &RECIPES[0])).await?;
        assert_eq!(tasks.len(), 1, "the task is owned by exactly one partition");
        Ok(tasks.remove(0))
    }

    /// Drops a task's journal records from the first one matching `from`.
    async fn truncate_journal(
        state: &StateDir,
        actx: &ApplyContext<'_>,
        task: &PackTask,
        from: impl Fn(&PackEvent) -> bool,
    ) -> Result<()> {
        let path = task_journal_path(&actx.cell.component.id, actx.cell.repo_id, &task.task_id);
        let events: Vec<PackEvent> = state.read_journal(&path).await?;
        let kept: String = events
            .iter()
            .take_while(|event| !from(event))
            .map(|event| serde_json::to_string(event).expect("serialisable") + "\n")
            .collect();
        state.write_atomic(&path, kept.as_bytes()).await
    }

    #[mononoke::fbinit_test]
    async fn a_throttled_store_relinks_survivors(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        // The fixture's memblob does not overwrite on put.
        let fixture = mixed_pack(&ctx).await?;
        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        // A read rate limit alone makes `open` wrap the store in `ThrottledBlob`.
        let throttle = ThrottleOptions {
            read_qps: NonZeroU32::new(1_000_000),
            ..Default::default()
        };
        let store = ComponentStore::key_source(fixture.memblob.clone())
            .with_storage_options(ReadOnlyStorage(false), throttle);
        let task = frozen_task(&ctx, &state, &plan, &store).await?;

        let (test, batch_size) = TestApply::new(10);
        let actx = test.context(
            &ctx,
            &state,
            &store,
            test_cell(&plan, &RECIPES[0]),
            batch_size,
        );
        let TaskResult::Complete(outcome) = execute_task(&actx, &task).await? else {
            bail!("the task should complete");
        };
        assert_eq!(outcome.survivors_relinked, 1);
        let survivor = fixture
            .packblob
            .get(&ctx, &format!("repo0001.{}", fixture.survivor))
            .await?
            .context("survivor must remain")?;
        assert_eq!(survivor.into_bytes(), bytes("surviving unode"));
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn rewrite_keeps_live_survivors_and_removes_retired(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let fixture = mixed_pack(&ctx).await?;
        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        let store = ComponentStore::key_source(fixture.memblob.clone());
        let task = frozen_task(&ctx, &state, &plan, &store).await?;
        assert_eq!(task.pack_key, fixture.pack_key);
        assert_eq!(
            task.eligible,
            fixture
                .retired
                .iter()
                .map(|k| format!("repo0001.{k}"))
                .collect::<Vec<_>>()
        );

        let (test, batch_size) = TestApply::new(10);
        let actx = test.context(
            &ctx,
            &state,
            &store,
            test_cell(&plan, &RECIPES[0]),
            batch_size,
        );
        let TaskResult::Complete(outcome) = execute_task(&actx, &task).await? else {
            bail!("the task should complete");
        };
        assert_eq!(outcome.retired_unlinked, 2);
        assert_eq!(outcome.survivors_relinked, 1);
        assert!(!outcome.source_missing);

        for retired in &fixture.retired {
            assert!(
                fixture
                    .packblob
                    .get(&ctx, &format!("repo0001.{retired}"))
                    .await?
                    .is_none()
            );
        }
        let survivor = fixture
            .packblob
            .get(&ctx, &format!("repo0001.{}", fixture.survivor))
            .await?
            .context("survivor must remain")?;
        assert_eq!(survivor.into_bytes(), bytes("surviving unode"));
        let moved = fixture
            .packblob
            .get(&ctx, &format!("repo0001.{}", fixture.moved))
            .await?
            .context("moved alias must remain")?;
        assert_eq!(moved.into_bytes(), bytes("moved"));
        assert!(
            fixture
                .packblob
                .get(&ctx, &format!("repo0001.{}", fixture.dict_only))
                .await?
                .is_none(),
            "an entry without an alias must not be recreated"
        );

        let raw = fixture
            .memblob
            .get(&ctx, &format!("repo0001.{}.pack", fixture.survivor))
            .await?
            .context("survivor link")?;
        let StoredValue::Packed(contents) = packblob::inspect_stored_value(raw.into_bytes())?
        else {
            bail!("survivor should link to a pack");
        };
        assert_eq!(
            Some(contents.pack_key()),
            outcome.replacement_pack_key.as_deref()
        );
        assert_eq!(
            contents.entry_keys().collect::<Vec<_>>(),
            vec![fixture.survivor.as_str()],
            "the replacement holds only the live survivor"
        );
        let leftover = fixture
            .memblob
            .get(
                &ctx,
                outcome.replacement_pack_key.as_deref().unwrap_or_default(),
            )
            .await?;
        assert!(leftover.is_none(), "no temporary pack key remains");

        // A completed task is not run again.
        assert_eq!(
            execute_task(&actx, &task).await?,
            TaskResult::Complete(outcome)
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn interrupted_tasks_resume_from_their_journal(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let fixture = mixed_pack(&ctx).await?;
        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        let store = ComponentStore::key_source(fixture.memblob.clone());
        let task = frozen_task(&ctx, &state, &plan, &store).await?;
        let (test, batch_size) = TestApply::new(10);
        let actx = test.context(
            &ctx,
            &state,
            &store,
            test_cell(&plan, &RECIPES[0]),
            batch_size,
        );

        // Simulate a run that published the replacement and unlinked every
        // retired alias, owner included, but stopped before completing.
        let TaskResult::Complete(first) = execute_task(&actx, &task).await? else {
            bail!("the task should complete");
        };
        truncate_journal(&state, &actx, &task, |event| {
            matches!(event, PackEvent::Complete { .. })
        })
        .await?;

        let TaskResult::Complete(resumed) = execute_task(&actx, &task).await? else {
            bail!("the resumed task should complete");
        };
        assert_eq!(resumed.retired_already_absent, 2);
        assert_eq!(resumed.survivors_relinked, first.survivors_relinked);
        assert_eq!(resumed.replacement_pack_key, first.replacement_pack_key);
        assert!(
            !resumed.source_missing,
            "the journal proves the rewrite was published"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_planned_task_whose_pack_vanished_is_a_conflict(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let fixture = mixed_pack(&ctx).await?;
        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        let store = ComponentStore::key_source(fixture.memblob.clone());
        let task = frozen_task(&ctx, &state, &plan, &store).await?;
        let (test, batch_size) = TestApply::new(10);
        let actx = test.context(
            &ctx,
            &state,
            &store,
            test_cell(&plan, &RECIPES[0]),
            batch_size,
        );

        // A run that planned the replacement, then lost every retired alias
        // to something else before publishing it.
        execute_task(&actx, &task).await?;
        truncate_journal(&state, &actx, &task, |event| {
            matches!(event, PackEvent::ReadyToUnlink { .. })
        })
        .await?;
        assert!(
            matches!(execute_task(&actx, &task).await?, TaskResult::Conflict(_)),
            "the survivors may not have been relinked, so the task must not complete"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_moved_survivor_is_a_conflict(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let fixture = mixed_pack(&ctx).await?;
        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        let store = ComponentStore::key_source(fixture.memblob.clone());
        let task = frozen_task(&ctx, &state, &plan, &store).await?;
        let (test, batch_size) = TestApply::new(10);
        let actx = test.context(
            &ctx,
            &state,
            &store,
            test_cell(&plan, &RECIPES[0]),
            batch_size,
        );

        let contents = find_source(&actx, &task)
            .await?
            .context("the original pack is readable")?;
        let survivors = live_survivors(&actx, &task, &contents).await?;
        assert_eq!(survivors, vec![fixture.survivor.clone()]);
        let mut journal = state.journal(&task_journal_path(
            &actx.cell.component.id,
            actx.cell.repo_id,
            &task.task_id,
        ));
        journal
            .append(&PackEvent::Planned {
                survivors,
                replacement_pack_key: "multiblob-planned.pack".to_string(),
            })
            .await?;
        // The survivor's alias now links to a third pack.
        let mut third = EmptyPack::new(3).add_base_blob(fixture.survivor.clone(), bytes("x"))?;
        third.add_delta_blob(
            fixture.survivor.clone(),
            format!("content.blake2.{}", hash(0x09)),
            bytes("other"),
        )?;
        fixture
            .packblob
            .put_packed(
                &ctx,
                third,
                "repo0001.".to_string(),
                "multiblob-".to_string(),
            )
            .await?;

        let TaskResult::Conflict(reason) = execute_task(&actx, &task).await? else {
            bail!("relinking a moved survivor must be refused");
        };
        assert!(reason.contains(&fixture.survivor), "{reason}");
        for retired in &fixture.retired {
            assert!(
                fixture
                    .memblob
                    .get(&ctx, &format!("repo0001.{retired}.pack"))
                    .await?
                    .is_some(),
                "retired aliases are kept while the task conflicts"
            );
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_corrupt_alias_envelope_fails_the_task(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let fixture = mixed_pack(&ctx).await?;
        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        let store = ComponentStore::key_source(fixture.memblob.clone());
        let task = frozen_task(&ctx, &state, &plan, &store).await?;
        let (test, batch_size) = TestApply::new(10);
        let actx = test.context(
            &ctx,
            &state,
            &store,
            test_cell(&plan, &RECIPES[0]),
            batch_size,
        );

        fixture
            .memblob
            .put_explicit(
                &ctx,
                task.owner_alias(),
                bytes("not an envelope"),
                blobstore::PutBehaviour::Overwrite,
            )
            .await?;
        assert!(
            execute_task(&actx, &task).await.is_err(),
            "an unreadable envelope is not a missing pack"
        );
        for alias in task.aliases() {
            assert!(
                fixture.memblob.get(&ctx, &alias).await?.is_some(),
                "no retired alias is unlinked"
            );
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_task_without_its_pack_completes_as_source_missing(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let fixture = mixed_pack(&ctx).await?;
        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        let store = ComponentStore::key_source(fixture.memblob.clone());
        let task = frozen_task(&ctx, &state, &plan, &store).await?;
        let (test, batch_size) = TestApply::new(10);
        let actx = test.context(
            &ctx,
            &state,
            &store,
            test_cell(&plan, &RECIPES[0]),
            batch_size,
        );

        // Every retired alias now holds a value of its own.
        for retired in &fixture.retired {
            fixture
                .packblob
                .put_explicit(
                    &ctx,
                    format!("repo0001.{retired}"),
                    bytes("single"),
                    blobstore::PutBehaviour::Overwrite,
                )
                .await?;
        }
        let TaskResult::Complete(outcome) = execute_task(&actx, &task).await? else {
            bail!("the task should complete");
        };
        assert!(outcome.source_missing);
        assert_eq!(outcome.retired_unlinked, 2, "the retired aliases still go");
        assert_eq!(outcome.replacement_pack_key, None);
        let mut totals = TaskTotals::default();
        totals.record(&outcome);
        assert_eq!(totals.source_missing.examples, vec![fixture.pack_key]);
        assert!(totals.is_finished());
        Ok(())
    }

    async fn pack_without_survivors(
        ctx: &CoreContext,
    ) -> Result<(Arc<Memblob>, PackBlob<Arc<Memblob>>, Vec<String>)> {
        let memblob = Arc::new(Memblob::default());
        let packblob = PackBlob::new(memblob.clone(), PackFormat::Raw);
        let retired: Vec<String> = [0x06u8, 0x07]
            .iter()
            .map(|seed| format!("blame_v3.v2.historymf.file.blake2.{}", hash(*seed)))
            .collect();
        let mut pack = EmptyPack::new(3).add_base_blob(retired[0].clone(), bytes("one"))?;
        pack.add_delta_blob(retired[0].clone(), retired[1].clone(), bytes("two"))?;
        packblob
            .put_packed(ctx, pack, "repo0001.".to_string(), "multiblob-".to_string())
            .await?;
        Ok((memblob, packblob, retired))
    }

    #[mononoke::fbinit_test]
    async fn packs_without_survivors_are_unlinked_owner_last(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (memblob, packblob, retired) = pack_without_survivors(&ctx).await?;
        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        let store = ComponentStore::key_source(memblob.clone());
        freeze_all(&ctx, &state, &plan, &store).await?;
        let cell = scan::cells(&plan)
            .find(|cell| cell.recipe.id == "blame_v3.v2")
            .context("recipe")?;
        let (test, batch_size) = TestApply::new(10);
        let actx = test.context(&ctx, &state, &store, cell, batch_size);
        let mut totals = TaskTotals::default();
        for hex in partition_hexes(1) {
            totals.add(&execute_owned_tasks(&actx, &hex).await?);
        }
        assert_eq!(totals.complete, 1);
        assert_eq!(totals.rewritten, 0, "no replacement pack without survivors");
        assert_eq!(totals.retired_unlinked, 2);
        assert!(totals.source_missing.is_empty());
        for key in &retired {
            assert!(
                packblob
                    .get(&ctx, &format!("repo0001.{key}"))
                    .await?
                    .is_none()
            );
        }
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn an_interrupted_task_without_survivors_resumes(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (memblob, _, _) = pack_without_survivors(&ctx).await?;
        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        let store = ComponentStore::key_source(memblob.clone());
        freeze_all(&ctx, &state, &plan, &store).await?;
        let cell = scan::cells(&plan)
            .find(|cell| cell.recipe.id == "blame_v3.v2")
            .context("recipe")?;
        let task = owned_tasks(&state, &plan, cell)
            .await?
            .pop()
            .context("one task")?;
        let (test, batch_size) = TestApply::new(10);
        let actx = test.context(&ctx, &state, &store, cell, batch_size);

        // Every alias, owner included, is gone, but the task never recorded
        // completion.
        execute_task(&actx, &task).await?;
        truncate_journal(&state, &actx, &task, |event| {
            matches!(event, PackEvent::Complete { .. })
        })
        .await?;
        let TaskResult::Complete(resumed) = execute_task(&actx, &task).await? else {
            bail!("a task with nothing left to keep must complete on resume");
        };
        assert_eq!(resumed.retired_already_absent, 2);
        assert!(!resumed.source_missing, "nothing needed the original pack");
        Ok(())
    }
}
