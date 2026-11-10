/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Hash-partitioned enumeration of retired keys.
//!
//! Each (component, repo, recipe) cell is split into partitions by the first
//! hex digits of the hash after the full recipe stem.  A partition is
//! enumerated to the end of its pages; only then is its inventory committed.

use std::collections::HashSet;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use clap::Args;
use context::CoreContext;
use derive_more::Display;
use futures::Stream;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use mononoke_macros::mononoke;
use mononoke_types::RepositoryId;
use packblob::StoredValue;
use serde::Deserialize;
use serde::Serialize;

use crate::adapters::ComponentStore;
use crate::policy::KeyRecipe;
use crate::policy::PhysicalEncoding;
use crate::policy::parse_physical_key;
use crate::progress::Phase;
use crate::scope::Encodings;
use crate::scope::Plan;
use crate::scope::PlannedComponent;
use crate::state::StagedLines;
use crate::state::StateDir;

/// Examples kept by a `Sampled` count; beyond this they are only counted.
const MAX_EXAMPLES: usize = 20;

#[derive(Args, Clone, Debug)]
pub struct ScanOptions {
    /// Partitions inventoried at the same time, or cells checked by
    /// `scan --exists`
    #[clap(long, default_value_t = NonZeroUsize::new(8).expect("8 is non-zero"))]
    pub scan_concurrency: NonZeroUsize,
    /// Envelope reads at the same time within one partition
    #[clap(long, default_value_t = NonZeroUsize::new(16).expect("16 is non-zero"))]
    pub classify_concurrency: NonZeroUsize,
}

/// All partitions for `digits` hex digits, in order.  With no digits, the
/// single partition is the empty string.
pub fn partition_hexes(digits: u8) -> impl Iterator<Item = String> + Clone {
    let width = usize::from(digits);
    (0..16u32.pow(u32::from(digits))).map(move |n| match width {
        0 => String::new(),
        _ => format!("{n:0width$x}"),
    })
}

/// The name of a partition's file with `suffix`.
pub fn partition_file(hex: &str, suffix: &str) -> String {
    match hex {
        "" => format!("all{suffix}"),
        hex => format!("{hex}{suffix}"),
    }
}

/// One (component, repo, recipe) cell.
#[derive(Clone, Copy, Display)]
#[display("component {} repo {repo_id} recipe {}", component.id, recipe.id)]
pub struct Cell<'a> {
    pub component: &'a PlannedComponent,
    pub repo_id: RepositoryId,
    pub recipe: &'static KeyRecipe,
    /// How the component stores the repository's keys.
    pub encodings: Encodings,
}

/// What identifies a cell within a plan.
pub type CellKey<'a> = (&'a str, RepositoryId, &'static str);

impl<'a> Cell<'a> {
    pub fn key(&self) -> CellKey<'a> {
        (&self.component.id, self.repo_id, self.recipe.id)
    }

    fn dir(&self) -> PathBuf {
        PathBuf::from("inventory")
            .join(&self.component.id)
            .join(self.repo_id.id().to_string())
            .join(self.recipe.id)
    }

    pub fn inventory_path(&self, hex: &str) -> PathBuf {
        self.dir().join(partition_file(hex, ".jsonl"))
    }

    pub fn summary_path(&self, hex: &str) -> PathBuf {
        self.dir().join(partition_file(hex, ".summary.json"))
    }

    /// The prefix of every key in partition `hex`.
    pub fn prefix(&self, hex: &str) -> String {
        format!("{}{}{hex}", self.repo_id.prefix(), self.recipe.stem)
    }

    /// How `key` is stored, if it is a key of this cell.
    pub fn holds(&self, key: &str) -> Option<PhysicalEncoding> {
        let (parsed, encoding) = parse_physical_key(key)?;
        (parsed.repo_id == self.repo_id && parsed.recipe.id == self.recipe.id).then_some(encoding)
    }

    /// The summary of partition `hex`, if its inventory is committed.
    pub async fn committed(&self, state: &StateDir, hex: &str) -> Result<Option<PartitionSummary>> {
        state.read_json(&self.summary_path(hex)).await
    }
}

/// Every cell of the plan's supported components, in plan order.
pub fn cells(plan: &Plan) -> impl Iterator<Item = Cell<'_>> {
    plan.components
        .iter()
        .filter(|component| component.is_supported())
        .flat_map(move |component| {
            component
                .repo_encodings()
                .into_iter()
                .flat_map(move |(repo_id, encodings)| {
                    plan.request.selection.recipes().map(move |recipe| Cell {
                        component,
                        repo_id,
                        recipe,
                        encodings,
                    })
                })
        })
}

/// A count with the first few of the items counted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sampled<T> {
    pub count: u64,
    pub examples: Vec<T>,
}

impl<T> Default for Sampled<T> {
    fn default() -> Self {
        Self {
            count: 0,
            examples: Vec::new(),
        }
    }
}

impl<T: Clone> Sampled<T> {
    pub fn note(&mut self, item: T) {
        self.count += 1;
        if self.examples.len() < MAX_EXAMPLES {
            self.examples.push(item);
        }
    }

    pub fn merge(&mut self, other: &Sampled<T>) {
        self.count += other.count;
        let room = MAX_EXAMPLES.saturating_sub(self.examples.len());
        self.examples
            .extend(other.examples.iter().take(room).cloned());
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// How a listed physical key is stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "form", rename_all = "snake_case")]
pub enum StoredForm {
    /// Stored under the logical key, without a packblob envelope.
    Plain,
    /// A packblob envelope holding only this value.
    Single,
    /// A link to a pack that may hold other values too.
    Packed { pack_key: String },
    /// Listed, but absent when read.
    Vanished,
    /// The envelope could not be interpreted.  Never deleted.
    Unreadable { error: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryRecord {
    pub key: String,
    #[serde(flatten)]
    pub form: StoredForm,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionSummary {
    pub duplicates: u64,
    pub plain: u64,
    pub single: u64,
    pub packed: u64,
    pub vanished: u64,
    pub unreadable: u64,
    /// Keys under the partition prefix that match no recipe.
    pub unknown: Sampled<String>,
    /// Distinct keys taken from an import rather than enumeration.
    pub seeded: u64,
}

impl PartitionSummary {
    fn count(&mut self, form: &StoredForm) {
        match form {
            StoredForm::Plain => self.plain += 1,
            StoredForm::Single => self.single += 1,
            StoredForm::Packed { .. } => self.packed += 1,
            StoredForm::Vanished => self.vanished += 1,
            StoredForm::Unreadable { .. } => self.unreadable += 1,
        }
    }

    fn add(&mut self, other: &PartitionSummary) {
        self.duplicates += other.duplicates;
        self.plain += other.plain;
        self.single += other.single;
        self.packed += other.packed;
        self.vanished += other.vanished;
        self.unreadable += other.unreadable;
        self.unknown.merge(&other.unknown);
        self.seeded += other.seeded;
    }

    /// Keys of a retired recipe present in the partition.
    pub fn retired_keys(&self) -> u64 {
        self.plain + self.single + self.packed + self.unreadable
    }
}

/// A partition inventory being written.  Its summary is written last and
/// marks it complete.
pub struct PartitionInventory {
    lines: StagedLines,
    pub summary: PartitionSummary,
}

impl PartitionInventory {
    pub async fn start(state: &StateDir, cell: &Cell<'_>, hex: &str) -> Result<Self> {
        Ok(Self {
            lines: state.staged_lines(&cell.inventory_path(hex)).await?,
            summary: PartitionSummary::default(),
        })
    }

    pub async fn record(&mut self, record: &InventoryRecord) -> Result<()> {
        self.summary.count(&record.form);
        self.lines.push(record).await
    }

    pub async fn commit(
        self,
        state: &StateDir,
        cell: &Cell<'_>,
        hex: &str,
    ) -> Result<PartitionSummary> {
        self.lines.commit().await?;
        state
            .write_json(&cell.summary_path(hex), &self.summary)
            .await?;
        Ok(self.summary)
    }
}

async fn classify(
    ctx: &CoreContext,
    store: &ComponentStore,
    key: String,
    encoding: PhysicalEncoding,
) -> Result<InventoryRecord> {
    let form = match encoding {
        PhysicalEncoding::Plain => StoredForm::Plain,
        PhysicalEncoding::PackEnvelope => match store.get(ctx, &key).await? {
            None => StoredForm::Vanished,
            Some(raw) => match packblob::inspect_stored_value(raw) {
                Ok(StoredValue::Single) => StoredForm::Single,
                Ok(StoredValue::Packed(contents)) => StoredForm::Packed {
                    pack_key: contents.pack_key().to_string(),
                },
                Err(e) => StoredForm::Unreadable {
                    error: format!("{e:#}"),
                },
            },
        },
    };
    Ok(InventoryRecord { key, form })
}

/// `classify`, with any read in its own task, so it progresses while the
/// caller awaits other work.
pub(crate) async fn classify_spawned(
    ctx: &CoreContext,
    store: &ComponentStore,
    key: String,
    encoding: PhysicalEncoding,
) -> Result<InventoryRecord> {
    match encoding {
        PhysicalEncoding::Plain => classify(ctx, store, key, encoding).await,
        PhysicalEncoding::PackEnvelope => {
            let (ctx, store) = (ctx.clone(), store.clone());
            mononoke::spawn_task(async move { classify(&ctx, &store, key, encoding).await }).await?
        }
    }
}

/// What recording a partition's inventory needs.
#[derive(Clone, Copy)]
pub struct InventoryContext<'a> {
    pub ctx: &'a CoreContext,
    pub state: &'a StateDir,
    pub store: &'a ComponentStore,
    /// Envelope reads at the same time.
    pub concurrency: NonZeroUsize,
}

/// Enumerates one partition to the end and commits its inventory, or
/// returns the summary of an inventory committed earlier.
pub async fn inventory_partition(
    ictx: InventoryContext<'_>,
    cell: Cell<'_>,
    hex: &str,
) -> Result<PartitionSummary> {
    let InventoryContext {
        ctx,
        state,
        store,
        concurrency,
    } = ictx;
    if let Some(summary) = cell.committed(state, hex).await? {
        return Ok(summary);
    }

    let prefix = cell.prefix(hex);
    let mut inventory = PartitionInventory::start(state, &cell, hex).await?;
    let mut seen = HashSet::new();
    let mut pages = store.pages(ctx, &prefix);
    while let Some(keys) = pages
        .try_next()
        .await
        .with_context(|| format!("enumerating {prefix}"))?
    {
        let mut fresh = Vec::new();
        for key in keys {
            if !seen.insert(key.clone()) {
                inventory.summary.duplicates += 1;
                continue;
            }
            match cell.holds(&key) {
                Some(encoding) => fresh.push((key, encoding)),
                None => inventory.summary.unknown.note(key),
            }
        }
        let records: Vec<InventoryRecord> = stream::iter(fresh)
            .map(|(key, encoding)| classify(ctx, store, key, encoding))
            .buffered(concurrency.get())
            .try_collect()
            .await?;
        for record in &records {
            inventory.record(record).await?;
        }
    }
    drop(pages);
    inventory.commit(state, &cell, hex).await
}

/// Whether any key of the cell's recipe is present.  Absence is only
/// reported after the cell's whole prefix has been enumerated.
pub async fn cell_has_keys(
    ctx: &CoreContext,
    store: &ComponentStore,
    cell: Cell<'_>,
) -> Result<bool> {
    let prefix = cell.prefix("");
    let mut pages = store.pages(ctx, &prefix);
    while let Some(keys) = pages
        .try_next()
        .await
        .with_context(|| format!("enumerating {prefix}"))?
    {
        if keys.iter().any(|key| cell.holds(key).is_some()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Number of partitions of each cell.
pub fn partition_count(digits: u8) -> usize {
    partition_hexes(digits).count()
}

/// Runs `run` on every partition of `cells`, `concurrency` at a time,
/// recording each in `phase` and yielding its result as it finishes.
pub fn run_partitions<'r, 'p, T, F, Fut>(
    cells: &'r [Cell<'p>],
    digits: u8,
    concurrency: NonZeroUsize,
    phase: &'r Phase,
    run: F,
) -> impl Stream<Item = (Cell<'p>, String, Result<T>)> + 'r
where
    'p: 'r,
    F: Fn(Cell<'p>, String) -> Fut + 'r,
    Fut: Future<Output = Result<T>> + 'r,
    T: 'r,
{
    let partitions = cells
        .iter()
        .flat_map(move |cell| partition_hexes(digits).map(move |hex| (*cell, hex)));
    stream::iter(partitions)
        .map(move |(cell, hex)| {
            let partition = run(cell, hex.clone());
            async move {
                let result = partition.await;
                phase.record(&result);
                (cell, hex, result)
            }
        })
        .buffer_unordered(concurrency.get())
}

/// Totals of a cell's committed partitions.
#[derive(Debug, Default)]
struct CellTotals {
    partitions_total: usize,
    partitions_done: usize,
    summary: PartitionSummary,
}

async fn cell_totals(state: &StateDir, cell: &Cell<'_>, digits: u8) -> Result<CellTotals> {
    let mut totals = CellTotals::default();
    for hex in partition_hexes(digits) {
        totals.partitions_total += 1;
        if let Some(summary) = cell.committed(state, &hex).await? {
            totals.partitions_done += 1;
            totals.summary.add(&summary);
        }
    }
    Ok(totals)
}

/// Prints the committed inventory of `cells`, one line per cell that is
/// incomplete or holds keys.
pub async fn print_inventory(state: &StateDir, cells: &[Cell<'_>], digits: u8) -> Result<()> {
    let mut empty_cells = 0;
    for cell in cells {
        let totals = cell_totals(state, cell, digits).await?;
        let s = &totals.summary;
        if totals.partitions_done == totals.partitions_total
            && s.retired_keys() == 0
            && s.unknown.is_empty()
            && s.vanished == 0
        {
            empty_cells += 1;
            continue;
        }
        let seeded = if s.seeded > 0 {
            format!(", seeded {}", s.seeded)
        } else {
            String::new()
        };
        println!(
            "{cell}: {}/{} partitions, {} keys (plain {}, single {}, packed {}, unreadable {}), vanished {}, unknown {}, duplicates {}{seeded}",
            totals.partitions_done,
            totals.partitions_total,
            s.retired_keys(),
            s.plain,
            s.single,
            s.packed,
            s.unreadable,
            s.vanished,
            s.unknown.count,
            s.duplicates
        );
        for example in &s.unknown.examples {
            println!("  unknown key {example}");
        }
    }
    println!("{empty_cells} cells hold no retired keys");
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use blobstore::Blobstore;
    use bytes::Bytes;
    use fbinit::FacebookInit;
    use memblob::Memblob;
    use mononoke_types::BlobstoreBytes;
    use packblob::EmptyPack;
    use packblob::PackBlob;
    use tempfile::TempDir;
    use tokio::fs;

    use super::*;
    use crate::policy::RECIPES;
    use crate::policy::RetiredBlobType;
    use crate::policy::Selection;
    use crate::scope::Capability;
    use crate::scope::ComponentReference;
    use crate::scope::PhysicalStore;
    use crate::scope::PlanRequest;
    use crate::scope::PlannedRepo;
    use crate::scope::RepoSelection;
    use crate::scope::StorageRef;

    pub(crate) fn hash(seed: u8) -> String {
        format!("{seed:02x}").repeat(32)
    }

    pub(crate) fn bytes(value: &str) -> BlobstoreBytes {
        BlobstoreBytes::from_bytes(Bytes::copy_from_slice(value.as_bytes()))
    }

    pub(crate) fn concurrency(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).expect("test concurrency is non-zero")
    }

    pub(crate) fn test_plan(partition_digits: u8) -> Plan {
        let component = PlannedComponent {
            id: "ctest".to_string(),
            store: PhysicalStore::Sqlite {
                path: "/test".to_string(),
            },
            capability: Capability::Supported,
            references: vec![ComponentReference {
                repo_id: RepositoryId::new(1),
                storage: StorageRef::Main,
                path: "pack/".to_string(),
                encoding: PhysicalEncoding::PackEnvelope,
            }],
        };
        let request = PlanRequest::new(
            RepoSelection::Explicit(vec![RepositoryId::new(1)]),
            Selection::new(RetiredBlobType::all()).expect("valid selection"),
            vec![],
            partition_digits,
        )
        .expect("valid request");
        Plan {
            format: crate::scope::PLAN_FORMAT.to_string(),
            policy_version: crate::policy::POLICY_VERSION.to_string(),
            pack_owner_election: crate::scope::PACK_OWNER_ELECTION.to_string(),
            request,
            recipes: RECIPES.iter().map(|r| r.id.to_string()).collect(),
            families: vec![],
            repos: vec![PlannedRepo {
                id: RepositoryId::new(1),
                name: "repo".to_string(),
                metadata_db: "sqlite:/test".to_string(),
                excluded_storage: vec![],
            }],
            components: vec![component],
            digest: "test-digest".to_string(),
        }
    }

    /// A state directory holding `plan`, removed when the guard is dropped.
    pub(crate) async fn test_state(plan: &Plan) -> Result<(TempDir, StateDir)> {
        let root = TempDir::new()?;
        let state = StateDir::create(root.path(), plan).await?;
        Ok((root, state))
    }

    /// The cell of the plan's first component, repo 1 and `recipe`.
    pub(crate) fn test_cell<'a>(plan: &'a Plan, recipe: &'static KeyRecipe) -> Cell<'a> {
        cells(plan)
            .find(|cell| cell.recipe.id == recipe.id)
            .expect("the test plan has a cell for every recipe")
    }

    #[mononoke::test]
    fn partitions_cover_every_prefix_once() {
        assert_eq!(partition_hexes(0).collect::<Vec<_>>(), vec![String::new()]);
        assert_eq!(partition_file("", ".jsonl"), "all.jsonl");
        assert_eq!(partition_file("0a", ".jsonl"), "0a.jsonl");
        let one: Vec<_> = partition_hexes(1).collect();
        assert_eq!(one.len(), 16);
        assert_eq!(one.first().map(String::as_str), Some("0"));
        assert_eq!(one.last().map(String::as_str), Some("f"));
        let two: Vec<_> = partition_hexes(2).collect();
        assert_eq!(two.len(), 256);
        assert_eq!(two[0x0a], "0a");
        let distinct: HashSet<_> = two.iter().collect();
        assert_eq!(distinct.len(), 256);
    }

    #[mononoke::test]
    fn cells_know_their_prefixes_and_keys() {
        let plan = test_plan(1);
        let cell = test_cell(&plan, &RECIPES[0]);
        assert_eq!(cell.prefix("ff"), "repo0001.historymf.file.blake2.ff");
        assert_eq!(cell.prefix(""), "repo0001.historymf.file.blake2.");
        let key = format!("repo0001.historymf.file.blake2.{}", hash(1));
        assert_eq!(cell.holds(&key), Some(PhysicalEncoding::Plain));
        assert_eq!(
            cell.holds(&format!("{key}.pack")),
            Some(PhysicalEncoding::PackEnvelope)
        );
        for other in [
            format!("repo0002.historymf.file.blake2.{}", hash(1)),
            format!("repo0001.historymf.dir.blake2.{}", hash(1)),
            format!("{key}.bak"),
        ] {
            assert_eq!(cell.holds(&other), None, "{other} is not in the cell");
        }
        assert_eq!(
            cell.to_string(),
            "component ctest repo 1 recipe historymf.file"
        );
        assert_eq!(
            cells(&plan).count(),
            RECIPES.len(),
            "one cell per recipe of the plan's only repo"
        );
    }

    #[mononoke::test]
    fn samples_keep_counts_beyond_their_examples() {
        let mut sampled = Sampled::default();
        (0..30).for_each(|n| sampled.note(n));
        let mut merged = Sampled::default();
        merged.note(-1);
        merged.merge(&sampled);
        assert_eq!(merged.count, 31);
        assert_eq!(merged.examples.len(), MAX_EXAMPLES);
        assert_eq!(merged.examples[..2], [-1, 0]);
    }

    #[mononoke::fbinit_test]
    async fn inventory_classifies_and_resumes(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let memblob = Arc::new(Memblob::default());
        let packblob = PackBlob::new(memblob.clone(), metaconfig_types::PackFormat::Raw);
        let plain = format!("repo0001.historymf.file.blake2.{}", hash(0x01));
        let single = format!("repo0001.historymf.file.blake2.{}", hash(0x02));
        let packed = format!("historymf.file.blake2.{}", hash(0x03));
        let survivor = format!("content.blake2.{}", hash(0x04));
        memblob.put(&ctx, plain.clone(), bytes("plain")).await?;
        packblob.put(&ctx, single.clone(), bytes("single")).await?;
        let mut pack = EmptyPack::new(0).add_base_blob(survivor, bytes("survivor"))?;
        pack.add_delta_blob(
            format!("content.blake2.{}", hash(0x04)),
            packed.clone(),
            bytes("packed"),
        )?;
        let pack_key = packblob
            .put_packed(
                &ctx,
                pack,
                "repo0001.".to_string(),
                "multiblob-".to_string(),
            )
            .await?;
        // Unrecognised and corrupt keys in the same partition.
        let corrupt = format!("repo0001.historymf.file.blake2.{}.pack", hash(0x05));
        memblob.put(&ctx, corrupt.clone(), bytes("xx")).await?;
        let malformed = format!("repo0001.historymf.file.blake2.{}.bak", hash(0x06));
        memblob.put(&ctx, malformed.clone(), bytes("bak")).await?;

        let plan = test_plan(1);
        let (_root, state) = test_state(&plan).await?;
        let cell = test_cell(&plan, &RECIPES[0]);
        let store = ComponentStore::key_source(memblob.clone());
        let summary = inventory_partition(
            InventoryContext {
                ctx: &ctx,
                state: &state,
                store: &store,
                concurrency: concurrency(4),
            },
            cell,
            "0",
        )
        .await?;
        assert_eq!(
            (
                summary.plain,
                summary.single,
                summary.packed,
                summary.unreadable
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(summary.unknown.count, 1);
        assert_eq!(summary.unknown.examples, vec![malformed]);

        let text = tokio::fs::read_to_string(state.path(&cell.inventory_path("0"))).await?;
        let forms: BTreeMap<_, _> = text
            .lines()
            .map(|line| serde_json::from_str::<InventoryRecord>(line).map(|r| (r.key, r.form)))
            .collect::<Result<_, _>>()?;
        assert_eq!(forms[&plain], StoredForm::Plain);
        assert_eq!(forms[&format!("{single}.pack")], StoredForm::Single);
        assert_eq!(
            forms[&format!("repo0001.{packed}.pack")],
            StoredForm::Packed { pack_key }
        );
        assert!(matches!(forms[&corrupt], StoredForm::Unreadable { .. }));

        // A committed partition is not enumerated again.
        memblob.unlink(&ctx, &plain).await?;
        let again = inventory_partition(
            InventoryContext {
                ctx: &ctx,
                state: &state,
                store: &store,
                concurrency: concurrency(4),
            },
            cell,
            "0",
        )
        .await?;
        assert_eq!(again, summary);

        assert!(cell_has_keys(&ctx, &store, cell).await?);
        let empty = ComponentStore::key_source(Arc::new(Memblob::default()));
        assert!(!cell_has_keys(&ctx, &empty, cell).await?);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_single_partition_covers_the_whole_cell(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let memblob = Arc::new(Memblob::default());
        for seed in [0x01, 0xa2, 0xf3] {
            let key = format!("repo0001.historymf.file.blake2.{}", hash(seed));
            memblob.put(&ctx, key, bytes("v")).await?;
        }
        let plan = test_plan(0);
        let (_root, state) = test_state(&plan).await?;
        let cell = test_cell(&plan, &RECIPES[0]);
        let store = ComponentStore::key_source(memblob);
        let hexes: Vec<String> = partition_hexes(0).collect();
        let summary = inventory_partition(
            InventoryContext {
                ctx: &ctx,
                state: &state,
                store: &store,
                concurrency: concurrency(2),
            },
            cell,
            &hexes[0],
        )
        .await?;
        assert_eq!(
            summary.plain, 3,
            "every key of the cell is in the partition"
        );
        assert!(fs::try_exists(state.path(&cell.inventory_path(""))).await?);
        assert!(
            cell.inventory_path("")
                .ends_with("historymf.file/all.jsonl"),
            "the single partition's files are named all"
        );
        Ok(())
    }
}
