/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Seeding a run's inventory from key lists instead of enumerating stores.
//!
//! Keys come from files, such as Manifold's daily key dumps, or from another
//! component's committed inventory.  They are validated against the policy
//! and staged by repository, recipe and partition.  Each partition is then
//! committed as the inventory of every target component, so `apply` and
//! `verify` treat it like a scanned one.
//!
//! An imported inventory is only as complete as its source; keys the source
//! missed are left for a later scan or import in a new run.

use std::collections::BTreeSet;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use context::CoreContext;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use mononoke_types::RepositoryId;
use serde::Deserialize;
use serde::Serialize;
use tokio::fs;
use tokio::fs::File;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::BufWriter;

use crate::adapters::ComponentStore;
use crate::policy::KeyRecipe;
use crate::policy::PhysicalEncoding;
use crate::policy::parse_physical_key;
use crate::progress::Phase;
use crate::scan;
use crate::scan::Cell;
use crate::scan::InventoryContext;
use crate::scan::InventoryRecord;
use crate::scan::PartitionInventory;
use crate::scan::PartitionSummary;
use crate::scan::Sampled;
use crate::scan::StoredForm;
use crate::scan::partition_file;
use crate::scope::Plan;
use crate::state::StateDir;

const STAGING_DIR: &str = "import/staging";
const STAGED: &str = "import/staged.json";

/// Manifold key dumps list each key under this path prefix.
const MANIFOLD_PATH_PREFIX: &str = "flat/";

/// Key files in a keys directory end with this; partial files end with
/// `.keys.tmp`.
const KEYS_EXTENSION: &str = "keys";

/// Where imported keys come from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeySource {
    /// Files of keys, one per line, by canonical path in sorted order.
    Files { paths: Vec<PathBuf> },
    /// The committed inventory of another component of the plan.
    Inventory { component: String },
}

impl KeySource {
    /// Files of keys, identified by their canonical paths, so the same files
    /// named differently are the same source.
    pub async fn files(paths: &[PathBuf]) -> Result<Self> {
        let mut canonical = Vec::with_capacity(paths.len());
        for path in paths {
            canonical.push(
                fs::canonicalize(path)
                    .await
                    .with_context(|| format!("resolving {}", path.display()))?,
            );
        }
        canonical.sort();
        canonical.dedup();
        Ok(KeySource::Files { paths: canonical })
    }

    /// The `*.keys` files of `dir`.
    pub async fn keys_dir(dir: &Path) -> Result<Self> {
        let mut entries = fs::read_dir(dir)
            .await
            .with_context(|| format!("listing {}", dir.display()))?;
        let mut paths = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == KEYS_EXTENSION) {
                paths.push(path);
            }
        }
        if paths.is_empty() {
            bail!("{} holds no .{KEYS_EXTENSION} files", dir.display());
        }
        Self::files(&paths).await
    }
}

/// The outcome of staging.  Written once staging is complete, so a resumed
/// import does not stage again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staged {
    pub source: KeySource,
    pub lines: u64,
    pub staged: u64,
    /// Retired keys of repositories or types the plan does not select.
    pub outside_plan: u64,
    /// Lines that are not retired keys.
    pub rejected: Sampled<String>,
}

fn staged_partition(repo_id: RepositoryId, recipe: &KeyRecipe, hex: &str) -> PathBuf {
    PathBuf::from(STAGING_DIR)
        .join(repo_id.id().to_string())
        .join(recipe.id)
        .join(partition_file(hex, ".keys"))
}

/// Appends keys to staging files, keeping the latest file open, so input
/// sorted by key opens each file once.  Files are synced once, when staging
/// finishes: unfinished staging is discarded and redone.
struct Stager<'a> {
    state: &'a StateDir,
    open: Option<(PathBuf, BufWriter<File>)>,
    touched: HashSet<PathBuf>,
}

impl Stager<'_> {
    async fn push(&mut self, relative: PathBuf, key: &str) -> Result<()> {
        let open = match self.open.take() {
            Some((path, writer)) if path == relative => (path, writer),
            previous => {
                if let Some((_, mut writer)) = previous {
                    writer.flush().await?;
                }
                let file = self.state.open_append(&relative).await?;
                self.touched.insert(relative.clone());
                (relative, BufWriter::new(file))
            }
        };
        let (_, writer) = self.open.insert(open);
        writer.write_all(key.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        Ok(())
    }

    async fn finish(mut self) -> Result<()> {
        if let Some((_, mut writer)) = self.open.take() {
            writer.flush().await?;
        }
        for relative in &self.touched {
            let path = self.state.path(relative);
            File::open(&path)
                .await
                .with_context(|| format!("opening {}", path.display()))?
                .sync_all()
                .await
                .with_context(|| format!("syncing {}", path.display()))?;
        }
        Ok(())
    }
}

struct Staging<'a> {
    stager: Stager<'a>,
    plan: &'a Plan,
    repos: HashSet<RepositoryId>,
    phase: &'a Phase,
    staged: Staged,
}

impl Staging<'_> {
    async fn add(&mut self, line: &str) -> Result<()> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(());
        }
        self.staged.lines += 1;
        let key = line.strip_prefix(MANIFOLD_PATH_PREFIX).unwrap_or(line);
        let Some((parsed, _)) = parse_physical_key(key) else {
            self.staged.rejected.note(line.to_string());
            self.phase.add("rejected", 1);
            return Ok(());
        };
        if !self.repos.contains(&parsed.repo_id)
            || !self
                .plan
                .request
                .selection
                .contains(parsed.recipe.blob_type)
        {
            self.staged.outside_plan += 1;
            self.phase.add("outside plan", 1);
            return Ok(());
        }
        let hex = parsed.partition(self.plan.request.partition_digits);
        let path = staged_partition(parsed.repo_id, parsed.recipe, hex);
        self.stager.push(path, &parsed.logical()).await?;
        self.staged.staged += 1;
        self.phase.add("staged", 1);
        Ok(())
    }
}

/// The cells of the plan in `component`.
fn component_cells<'a>(plan: &'a Plan, component: &'a str) -> impl Iterator<Item = Cell<'a>> {
    scan::cells(plan).filter(move |cell| cell.component.id == component)
}

/// Number of progress units `stage` reports for `source`.
pub fn staging_units(plan: &Plan, source: &KeySource) -> usize {
    match source {
        KeySource::Files { paths } => paths.len(),
        KeySource::Inventory { component } => {
            component_cells(plan, component).count()
                * scan::partition_count(plan.request.partition_digits)
        }
    }
}

/// Validates the source's keys and stages them by repository, recipe and
/// partition, or returns the outcome of staging completed earlier.
pub async fn stage(
    state: &StateDir,
    plan: &Plan,
    source: KeySource,
    phase: &Phase,
) -> Result<Staged> {
    if let Some(staged) = state.read_json::<Staged>(Path::new(STAGED)).await? {
        if staged.source != source {
            bail!(
                "this run already imported keys from {:?}; use a new state directory for another source",
                staged.source
            );
        }
        return Ok(staged);
    }

    // Staging interrupted before completion starts again.
    state.remove_dir_all(Path::new(STAGING_DIR)).await?;
    let mut staging = Staging {
        stager: Stager {
            state,
            open: None,
            touched: HashSet::new(),
        },
        plan,
        repos: plan.repos.iter().map(|repo| repo.id).collect(),
        phase,
        staged: Staged {
            source: source.clone(),
            lines: 0,
            staged: 0,
            outside_plan: 0,
            rejected: Sampled::default(),
        },
    };
    match &source {
        KeySource::Files { paths } => {
            for path in paths {
                let file = File::open(path)
                    .await
                    .with_context(|| format!("opening {}", path.display()))?;
                let mut lines = BufReader::new(file).lines();
                while let Some(line) = lines
                    .next_line()
                    .await
                    .with_context(|| format!("reading {}", path.display()))?
                {
                    staging.add(&line).await?;
                }
                phase.unit_done();
            }
        }
        KeySource::Inventory { component } => {
            for cell in component_cells(plan, component) {
                for hex in scan::partition_hexes(plan.request.partition_digits) {
                    if cell.committed(state, &hex).await?.is_none() {
                        bail!(
                            "partition {hex} of {cell} has no inventory; scan component {component} first"
                        );
                    }
                    let records = state
                        .read_json_lines::<InventoryRecord>(&cell.inventory_path(&hex))
                        .await?;
                    for record in records {
                        if record.form != StoredForm::Vanished {
                            staging.add(&record.key).await?;
                        }
                    }
                    phase.unit_done();
                }
            }
        }
    }
    staging.stager.finish().await?;
    state.write_json(Path::new(STAGED), &staging.staged).await?;
    Ok(staging.staged)
}

async fn read_staged(state: &StateDir, relative: &Path) -> Result<Vec<String>> {
    let path = state.path(relative);
    match fs::read_to_string(&path).await {
        Ok(text) => Ok(text.lines().map(str::to_string).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

async fn check_seed(
    ctx: &CoreContext,
    store: &ComponentStore,
    key: &str,
    encoding: PhysicalEncoding,
) -> Result<InventoryRecord> {
    match encoding {
        // Not read: `apply` records a key that is already absent.
        PhysicalEncoding::Plain => Ok(InventoryRecord {
            key: key.to_string(),
            form: StoredForm::Plain,
        }),
        // Read, because a packed value can only be deleted with its pack.
        PhysicalEncoding::PackEnvelope => {
            scan::classify_spawned(ctx, store, encoding.physical_key(key), encoding).await
        }
    }
}

/// Commits one partition's staged keys as a component's inventory, or
/// returns the summary of an inventory committed earlier.
pub async fn import_partition(
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

    let keys = read_staged(state, &staged_partition(cell.repo_id, cell.recipe, hex)).await?;
    let listed = keys.len();
    let unique: BTreeSet<String> = keys.into_iter().collect();
    let mut inventory = PartitionInventory::start(state, &cell, hex).await?;
    inventory.summary.duplicates = (listed - unique.len()) as u64;
    inventory.summary.seeded = unique.len() as u64;
    let checks = unique
        .iter()
        .flat_map(|key| cell.encodings.iter().map(move |encoding| (key, encoding)));
    let mut records = stream::iter(checks)
        .map(|(key, encoding)| check_seed(ctx, store, key, encoding))
        .buffered(concurrency.get());
    while let Some(record) = records.try_next().await? {
        inventory.record(&record).await?;
    }
    drop(records);
    inventory.commit(state, &cell, hex).await
}

#[cfg(test)]
mod tests {
    use std::slice;
    use std::sync::Arc;

    use blobstore::Blobstore;
    use fbinit::FacebookInit;
    use memblob::Memblob;
    use mononoke_macros::mononoke;
    use packblob::PackBlob;

    use super::*;
    use crate::policy::RECIPES;
    use crate::scan::tests::bytes;
    use crate::scan::tests::concurrency;
    use crate::scan::tests::hash;
    use crate::scan::tests::test_cell;
    use crate::scan::tests::test_plan;
    use crate::scan::tests::test_state;

    fn quiet_phase() -> Phase {
        Phase::new("test", "units", 1, &["staged", "outside plan", "rejected"])
    }

    #[mononoke::fbinit_test]
    async fn staging_validates_and_partitions_keys(_fb: FacebookInit) -> Result<()> {
        let plan = test_plan(1);
        let (root, state) = test_state(&plan).await?;
        let first = format!("repo0001.historymf.file.blake2.{}", hash(0x01));
        let second = format!("repo0001.historymf.file.blake2.{}", hash(0x12));
        let other_repo = format!("repo0002.historymf.file.blake2.{}", hash(0x01));
        let input = root.path().join("keys.txt");
        tokio::fs::write(
            &input,
            format!(
                "flat/{first}\n\n{first}.pack\n{second}\n{other_repo}\nrepo0001.content.blake2.{}\n",
                hash(0x03)
            ),
        )
        .await?;

        let source = KeySource::files(slice::from_ref(&input)).await?;
        let staged = stage(&state, &plan, source.clone(), &quiet_phase()).await?;
        assert_eq!(
            (
                staged.lines,
                staged.staged,
                staged.outside_plan,
                staged.rejected.count
            ),
            (5, 3, 1, 1),
            "the blank line is skipped, the dump path and envelope key name the same key"
        );
        let zero = read_staged(
            &state,
            &staged_partition(RepositoryId::new(1), &RECIPES[0], "0"),
        )
        .await?;
        assert_eq!(zero, vec![first.clone(), first.clone()]);
        let one = read_staged(
            &state,
            &staged_partition(RepositoryId::new(1), &RECIPES[0], "1"),
        )
        .await?;
        assert_eq!(one, vec![second]);

        // A completed staging is reused, and only for the same source, which
        // the same files named differently still are.
        tokio::fs::write(&input, "").await?;
        let renamed = root.path().join(".").join("keys.txt");
        let same = KeySource::files(&[renamed, input.clone()]).await?;
        assert_eq!(same, source);
        assert_eq!(stage(&state, &plan, same, &quiet_phase()).await?, staged);
        let other = KeySource::Inventory {
            component: "ctest".to_string(),
        };
        assert!(stage(&state, &plan, other, &quiet_phase()).await.is_err());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_keys_directory_takes_its_finished_files_in_order(_fb: FacebookInit) -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        for name in ["part2.keys", "part1.keys", "part3.keys.tmp", "notes.txt"] {
            tokio::fs::write(dir.path().join(name), "").await?;
        }
        let KeySource::Files { paths } = KeySource::keys_dir(dir.path()).await? else {
            bail!("a keys directory is a list of files");
        };
        let names: Vec<_> = paths
            .iter()
            .filter_map(|path| path.file_name()?.to_str())
            .collect();
        assert_eq!(names, vec!["part1.keys", "part2.keys"]);
        let empty = tempfile::TempDir::new()?;
        assert!(KeySource::keys_dir(empty.path()).await.is_err());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn packed_seeds_are_read_and_plain_seeds_are_not(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let memblob = Arc::new(Memblob::default());
        let packblob = PackBlob::new(memblob.clone(), metaconfig_types::PackFormat::Raw);
        let stored = format!("repo0001.historymf.file.blake2.{}", hash(0x01));
        let missing = format!("repo0001.historymf.file.blake2.{}", hash(0x02));
        packblob.put(&ctx, stored.clone(), bytes("value")).await?;

        let mut plan = test_plan(1);
        let (root, state) = test_state(&plan).await?;
        let input = root.path().join("keys.txt");
        tokio::fs::write(&input, format!("{stored}\n{missing}\n{stored}\n")).await?;
        let source = KeySource::files(&[input]).await?;
        stage(&state, &plan, source, &quiet_phase()).await?;

        let store = ComponentStore::key_source(memblob.clone());
        let cell = test_cell(&plan, &RECIPES[0]);
        let summary = import_partition(
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
                summary.seeded,
                summary.duplicates,
                summary.single,
                summary.vanished
            ),
            (2, 1, 1, 1),
            "the stored key is a single value; the missing one is absent"
        );
        let records = state
            .read_json_lines::<InventoryRecord>(&cell.inventory_path("0"))
            .await?;
        assert_eq!(records[0].key, format!("{stored}.pack"));

        // A component referenced without packing records keys unread.
        plan.components[0].references[0].encoding = PhysicalEncoding::Plain;
        plan.components[0].id = "cplain".to_string();
        let empty = ComponentStore::key_source(Arc::new(Memblob::default()));
        let cell = test_cell(&plan, &RECIPES[0]);
        let plain = import_partition(
            InventoryContext {
                ctx: &ctx,
                state: &state,
                store: &empty,
                concurrency: concurrency(4),
            },
            cell,
            "0",
        )
        .await?;
        assert_eq!((plain.seeded, plain.plain), (2, 2));

        // A committed partition is not imported again.
        let again = import_partition(
            InventoryContext {
                ctx: &ctx,
                state: &state,
                store: &empty,
                concurrency: concurrency(4),
            },
            cell,
            "0",
        )
        .await?;
        assert_eq!(again, plain);
        Ok(())
    }
}
