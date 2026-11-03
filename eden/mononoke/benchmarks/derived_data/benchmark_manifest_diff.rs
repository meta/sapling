/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Benchmark manifest diffs and path lookups over content manifests.
//!
//! Builds a repo with one large flat directory and several medium directories,
//! then measures result counts, blobstore reads, bytes, and wall-clock time.
//! Scenarios cover small edits, subtree replacements, additions/removals,
//! pagination, and individual versus batched path lookups.
//!
//! Diff operations prune identical content-manifest sub-shards without loading
//! them. Added and removed entries still need enumeration, while path lookups
//! perform their own trie descents.
//!
//! Run with optional positional args:
//! ```text
//!   <total_files> <modify_count> <wide_dirs> <wide_files> <limit>
//!   buck2 run //eden/mononoke/benchmarks/derived_data:benchmark_manifest_diff
//!   buck2 run //eden/mononoke/benchmarks/derived_data:benchmark_manifest_diff -- 200000 5
//! ```

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use blobstore::BlobstoreBytes;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::KeyedBlobstore;
use blobstore::StoreLoadable;
use bonsai_hg_mapping::BonsaiHgMapping;
use bookmarks::Bookmarks;
use commit_graph::CommitGraph;
use commit_graph::CommitGraphWriter;
use content_manifest_derivation::RootContentManifestId;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use fbinit::FacebookInit;
use filestore::FilestoreConfig;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use futures::stream::BoxStream;
use futures_stats::TimedFutureExt;
use manifest::Entry;
use manifest::ManifestOps;
use manifest::ManifestOrderedOps;
use mononoke_types::ChangesetId;
use mononoke_types::ContentManifestId;
use mononoke_types::path::MPath;
use rand::Rng;
use rand::RngExt as _;
use rand::distr::Alphanumeric;
use rand::distr::Uniform;
use repo_blobstore::RepoBlobstore;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedData;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentity;
use tests_utils::CreateCommitContext;

/// Matches the fan-out `metadata_diff` callers use when resolving a batch of
/// paths, so the per-path scenario is not artificially serialized.
const LOOKUP_CONCURRENCY: usize = 100;

/// The big flat directory that makes the sharding visible.
const BIG_DIR: &str = "large_directory";
/// A subdirectory of [`BIG_DIR`], used as the site of a manifest replacement.
const BIG_DIR_SUBDIR: &str = "large_directory/subdir";
/// A small directory used as the payload of a manifest replacement.
const REPLACEMENT_SOURCE: &str = "copy_source";

#[facet::container]
#[derive(Clone)]
struct Repo {
    #[facet]
    repo_identity: RepoIdentity,

    #[facet]
    repo_blobstore: RepoBlobstore,

    #[facet]
    repo_derived_data: RepoDerivedData,

    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    bookmarks: dyn Bookmarks,

    #[facet]
    commit_graph: CommitGraph,

    #[facet]
    commit_graph_writer: dyn CommitGraphWriter,

    #[facet]
    filestore_config: FilestoreConfig,
}

/// A `Blobstore` wrapper that counts `get` calls and bytes returned. Every
/// clone shares the same counters via `Arc`, so wrapping in `Arc<_>` (which is
/// what the diff entry points need: `Store: Clone + Send + Sync + 'static`)
/// keeps a single shared tally.
struct CountingBlobstore {
    inner: RepoBlobstore,
    gets: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
}

impl fmt::Debug for CountingBlobstore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CountingBlobstore({:?})", self.inner)
    }
}

impl fmt::Display for CountingBlobstore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CountingBlobstore({})", self.inner)
    }
}

#[async_trait]
impl KeyedBlobstore for CountingBlobstore {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        let res = self.inner.get(ctx, key).await?;
        self.gets.fetch_add(1, Ordering::Relaxed);
        if let Some(data) = &res {
            self.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        }
        Ok(res)
    }

    async fn put<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<()> {
        self.inner.put(ctx, key, value).await
    }

    async fn is_present<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent> {
        self.inner.is_present(ctx, key).await
    }

    async fn copy<'a>(
        &'a self,
        ctx: &'a CoreContext,
        old_key: &'a str,
        new_key: String,
    ) -> Result<()> {
        self.inner.copy(ctx, old_key, new_key).await
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        self.inner.unlink(ctx, key).await
    }
}

type Store = Arc<CountingBlobstore>;

/// Shared handles onto the counting blobstore's tallies.
struct Counters {
    gets: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
}

impl Counters {
    fn reset(&self) {
        self.gets.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
    }

    fn gets(&self) -> u64 {
        self.gets.load(Ordering::Relaxed)
    }

    fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

struct Measurement {
    entries: u64,
    gets: u64,
    bytes: u64,
    time: Duration,
    /// Longest single uninterrupted `poll()`. This is what starves a tokio
    /// worker: a slow request only cascades if it holds the executor.
    max_poll: Duration,
    polls: u64,
}

/// Drain `stream`, counting entries, while measuring blob gets and bytes.
/// Counters are reset first, so setup done by the caller is not charged to the
/// scenario.
async fn measure(
    counters: &Counters,
    limit: Option<usize>,
    stream: BoxStream<'static, Result<()>>,
) -> Result<Measurement> {
    counters.reset();

    let (stats, entries) = async move {
        let mut stream = match limit {
            Some(limit) => stream.take(limit).boxed(),
            None => stream,
        };
        let mut entries = 0u64;
        while let Some(item) = stream.next().await {
            item?;
            entries += 1;
        }
        anyhow::Ok(entries)
    }
    .timed()
    .await;

    Ok(Measurement {
        entries: entries?,
        gets: counters.gets(),
        bytes: counters.bytes(),
        time: stats.completion_time,
        max_poll: stats.max_poll_time,
        polls: stats.poll_count,
    })
}

/// What the scenario compares, in terms of the fixture's commits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A handful of files changed inside the big directory.
    ChangedSmall,
    /// Same diff, plus a manifest replacement inside the big directory.
    ReplacementInBig,
    /// Same diff, but the replacement sits in a medium directory instead.
    ReplacementInSmall,
    /// The big directory appears wholesale.
    AddedSubtree,
    /// The big directory disappears wholesale.
    RemovedSubtree,
    /// One file changed in each of many medium directories.
    WideTree,
    /// `metadata_diff`-shaped: resolve each changed path independently on both
    /// sides via `find_entry`.
    MetadataLookupPerPath,
    /// The same resolution, batched into a single `find_entries` walk.
    MetadataLookupBatched,
}

struct Scenario {
    name: &'static str,
    kind: Kind,
    ordered: bool,
    limit: Option<usize>,
}

fn scenarios(limit: usize) -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    for (name, kind) in [
        ("changed-small", Kind::ChangedSmall),
        ("replacement-in-big", Kind::ReplacementInBig),
        ("replacement-in-small", Kind::ReplacementInSmall),
        ("added-subtree", Kind::AddedSubtree),
        ("removed-subtree", Kind::RemovedSubtree),
        ("wide-tree", Kind::WideTree),
    ] {
        for ordered in [false, true] {
            scenarios.push(Scenario {
                name,
                kind,
                ordered,
                limit: None,
            });
        }
    }
    // Truncating the consumer does not truncate the enumeration of an added
    // directory, so measure it explicitly.
    for ordered in [false, true] {
        scenarios.push(Scenario {
            name: "added-subtree-limited",
            kind: Kind::AddedSubtree,
            ordered,
            limit: Some(limit),
        });
    }
    scenarios.push(Scenario {
        name: "metadata-lookup-per-path",
        kind: Kind::MetadataLookupPerPath,
        ordered: false,
        limit: None,
    });
    scenarios.push(Scenario {
        name: "metadata-lookup-batched",
        kind: Kind::MetadataLookupBatched,
        ordered: false,
        limit: None,
    });
    scenarios
}

/// Root content manifest IDs of the fixture's commits.
struct Roots {
    /// No `large_directory`.
    no_big: ContentManifestId,
    /// `large_directory` present; the base of every "changed" scenario.
    base: ContentManifestId,
    /// `base` with a few files in `large_directory` modified.
    small_change: ContentManifestId,
    /// `base` with a few files in `large_directory` modified AND one file in
    /// `wide/dir_0` modified, so that a replacement can be placed in either
    /// directory and the two compared.
    mixed_change: ContentManifestId,
    /// `base` with one file modified in each `wide/dir_*`.
    wide_change: ContentManifestId,
}

/// Run one scenario against the fixture content manifests.
async fn run_scenario(
    ctx: &CoreContext,
    store: &Store,
    roots: &Roots,
    lookup_paths: &[MPath],
    scenario: &Scenario,
    counters: &Counters,
) -> Result<Measurement> {
    // Path-lookup scenarios bypass the diff code entirely.
    match scenario.kind {
        Kind::MetadataLookupPerPath => {
            // `metadata_diff` resolves each path on each side as an independent
            // request, sharing nothing between paths.
            let mut work = Vec::with_capacity(lookup_paths.len() * 2);
            for path in lookup_paths {
                work.push((roots.base.clone(), path.clone()));
                work.push((roots.mixed_change.clone(), path.clone()));
            }
            let stream = {
                let ctx = ctx.clone();
                let store = store.clone();
                stream::iter(work)
                    .map(move |(root, path)| root.find_entry(ctx.clone(), store.clone(), path))
                    .buffered(LOOKUP_CONCURRENCY)
                    .map_ok(|_| ())
                    .boxed()
            };
            return measure(counters, scenario.limit, stream).await;
        }
        Kind::MetadataLookupBatched => {
            let old = roots
                .base
                .find_entries(ctx.clone(), store.clone(), lookup_paths.to_vec())
                .map_ok(|_| ());
            let new = roots
                .mixed_change
                .find_entries(ctx.clone(), store.clone(), lookup_paths.to_vec())
                .map_ok(|_| ());
            let stream = old.chain(new).boxed();
            return measure(counters, scenario.limit, stream).await;
        }
        _ => {}
    }

    // (old side, new side, Option<(replacement site, path to take the payload from)>)
    let (old, new, replacement) = match scenario.kind {
        Kind::ChangedSmall => (&roots.base, &roots.small_change, None),
        Kind::ReplacementInBig => (
            &roots.base,
            &roots.mixed_change,
            Some((MPath::new(BIG_DIR_SUBDIR)?, MPath::new(REPLACEMENT_SOURCE)?)),
        ),
        Kind::ReplacementInSmall => (
            &roots.base,
            &roots.mixed_change,
            Some((
                MPath::new("wide/dir_0/file_0")?,
                MPath::new("wide/dir_1/file_0")?,
            )),
        ),
        Kind::AddedSubtree => (&roots.no_big, &roots.base, None),
        Kind::RemovedSubtree => (&roots.base, &roots.no_big, None),
        Kind::WideTree => (&roots.base, &roots.wide_change, None),
        Kind::MetadataLookupPerPath | Kind::MetadataLookupBatched => unreachable!(),
    };

    // Resolving the replacement payload costs a few gets of its own; do it
    // before `measure` resets the counters, so it isn't charged to the diff.
    let replacement = match replacement {
        None => None,
        Some((at, from)) => {
            let entry = roots
                .base
                .find_entry(ctx.clone(), store.clone(), from.clone())
                .await?
                .ok_or_else(|| anyhow!("replacement source {from} not found"))?;
            Some((at, entry))
        }
    };

    let stream: BoxStream<'static, Result<()>> = if scenario.ordered {
        let mut replacements = HashMap::new();
        if let Some((at, entry)) = replacement {
            let entry = match entry {
                Entry::Tree(id) => {
                    let manifest = StoreLoadable::load(&id, ctx, store).await?;
                    let counts = manifest.subentries.rollup_data().descendant_counts;
                    let weight = (counts.files_count + counts.dirs_count) as usize;
                    Entry::Tree((weight, id))
                }
                Entry::Leaf(leaf) => Entry::Leaf(leaf),
            };
            replacements.insert(at, entry);
        }
        old.filtered_diff_ordered(
            ctx.clone(),
            store.clone(),
            new.clone(),
            store.clone(),
            None,
            |_| Some(()),
            |_| true,
            replacements,
        )
    } else {
        let mut replacements = HashMap::new();
        if let Some((at, entry)) = replacement {
            replacements.insert(at, entry);
        }
        old.filtered_diff(
            ctx.clone(),
            store.clone(),
            new.clone(),
            store.clone(),
            |_| Some(()),
            |_| true,
            replacements,
        )
    };

    measure(counters, scenario.limit, stream).await
}

fn print_header() {
    println!(
        "\n{:<32} {:<10} {:<8} {:<11} {:<12} {:<13} {:<7} time",
        "scenario", "ordering", "entries", "blob_gets", "bytes", "max_poll", "polls"
    );
    println!("{}", "-".repeat(130));
}

fn print_measurement(scenario: &Scenario, measurement: &Measurement) {
    let ordering = if scenario.ordered {
        "ordered"
    } else {
        "unordered"
    };
    let name = match scenario.limit {
        Some(limit) => format!("{} (take {limit})", scenario.name),
        None => scenario.name.to_string(),
    };
    println!(
        "{:<32} {:<10} {:<8} {:<11} {:<12} {:<13} {:<7} {:?}",
        name,
        ordering,
        measurement.entries,
        measurement.gets,
        measurement.bytes,
        format!("{:?}", measurement.max_poll),
        measurement.polls,
        measurement.time,
    );
}

fn gen_filename(rng: &mut impl Rng, len: usize) -> String {
    std::iter::repeat_with(|| rng.sample(Alphanumeric))
        .take(len)
        .map(char::from)
        .collect()
}

/// The commits every scenario is built from.
struct Fixture {
    no_big: ChangesetId,
    base: ChangesetId,
    small_change: ChangesetId,
    mixed_change: ChangesetId,
    wide_change: ChangesetId,
    /// The paths modified in [`BIG_DIR`], reused as the `metadata_diff` inputs.
    changed_paths: Vec<MPath>,
}

/// Build the fixture:
///
/// * `no_big`: `base_file`, `copy_source/{a,b,c}`, `wide/dir_i/file_j`
/// * `base`: adds `large_directory/<total_files random names>` and
///   `large_directory/subdir/{a,b,c}`
/// * `small_change`, `mixed_change`, `wide_change`: children of `base`
async fn build_fixture(
    ctx: &CoreContext,
    repo: &Repo,
    total_files: usize,
    modify_count: usize,
    wide_dirs: usize,
    wide_files: usize,
) -> Result<Fixture> {
    let mut rng = rand::rng();
    let len_distr = Uniform::new(5, 50).unwrap();
    let mut filenames = std::collections::BTreeSet::new();
    while filenames.len() < total_files {
        let len = rng.sample(len_distr);
        filenames.insert(gen_filename(&mut rng, len));
    }
    let big_paths: Vec<String> = filenames
        .into_iter()
        .map(|name| format!("{BIG_DIR}/{name}"))
        .collect();

    let mut create = CreateCommitContext::new_root(ctx, repo).add_file("base_file", "content");
    for name in ["a", "b", "c"] {
        create = create.add_file(
            format!("{REPLACEMENT_SOURCE}/{name}").as_str(),
            format!("content of {REPLACEMENT_SOURCE}/{name}"),
        );
    }
    for dir in 0..wide_dirs {
        for file in 0..wide_files {
            create = create.add_file(
                format!("wide/dir_{dir}/file_{file}").as_str(),
                format!("content of wide/dir_{dir}/file_{file}"),
            );
        }
    }
    let no_big = create.commit().await?;

    let mut create = CreateCommitContext::new(ctx, repo, vec![no_big]);
    for path in &big_paths {
        create = create.add_file(path.as_str(), format!("content of {path}"));
    }
    for name in ["a", "b", "c"] {
        create = create.add_file(
            format!("{BIG_DIR_SUBDIR}/{name}").as_str(),
            format!("content of {BIG_DIR_SUBDIR}/{name}"),
        );
    }
    let base = create.commit().await?;

    let modify_count = modify_count.min(big_paths.len());
    let modified = &big_paths[..modify_count];

    let mut create = CreateCommitContext::new(ctx, repo, vec![base]);
    for path in modified {
        create = create.add_file(path.as_str(), format!("modified content of {path}"));
    }
    let small_change = create.commit().await?;

    // Same as `small_change` plus one file in a medium directory, so that
    // `replacement-in-big` and `replacement-in-small` diff the same two commits
    // and their get counts are directly comparable.
    let mut create = CreateCommitContext::new(ctx, repo, vec![base]);
    for path in modified {
        create = create.add_file(path.as_str(), format!("modified content of {path}"));
    }
    let mixed_change = create
        .add_file("wide/dir_0/file_0", "modified content of wide/dir_0/file_0")
        .commit()
        .await?;

    let mut create = CreateCommitContext::new(ctx, repo, vec![base]);
    for dir in 0..wide_dirs {
        create = create.add_file(
            format!("wide/dir_{dir}/file_0").as_str(),
            format!("modified content of wide/dir_{dir}/file_0"),
        );
    }
    let wide_change = create.commit().await?;

    let changed_paths = modified
        .iter()
        .map(|path| MPath::new(path.as_str()))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Fixture {
        no_big,
        base,
        small_change,
        mixed_change,
        wide_change,
        changed_paths,
    })
}

async fn derive_content_manifest(
    ctx: &CoreContext,
    repo: &Repo,
    csid: ChangesetId,
) -> Result<ContentManifestId> {
    Ok(repo
        .repo_derived_data()
        .derive::<RootContentManifestId>(ctx, csid, DerivationPriority::LOW)
        .await?
        .into_content_manifest_id())
}

#[fbinit::main]
async fn main(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);

    let mut args = std::env::args().skip(1);
    let mut next = |default: usize| -> usize {
        args.next()
            .and_then(|arg| arg.parse().ok())
            .unwrap_or(default)
    };
    let total_files = next(100_000);
    let modify_count = next(10);
    let wide_dirs = next(8);
    let wide_files = next(2_000);
    let limit = next(100);

    println!(
        "Building repo: {total_files} files in {BIG_DIR}, {wide_dirs} x {wide_files} files under \
         wide/, modifying {modify_count}"
    );
    let repo: Repo = test_repo_factory::build_empty(ctx.fb).await?;
    let fixture = build_fixture(
        &ctx,
        &repo,
        total_files,
        modify_count,
        wide_dirs,
        wide_files,
    )
    .await?;

    let commits = [
        fixture.no_big,
        fixture.base,
        fixture.small_change,
        fixture.mixed_change,
        fixture.wide_change,
    ];
    let content_manifests = future::try_join_all(
        commits
            .iter()
            .map(|csid| derive_content_manifest(&ctx, &repo, *csid)),
    )
    .await?;

    let [no_big, base, small_change, mixed_change, wide_change]: [ContentManifestId; 5] =
        content_manifests
            .try_into()
            .map_err(|_| anyhow!("expected one content manifest per fixture commit"))?;
    let content_roots = Roots {
        no_big,
        base,
        small_change,
        mixed_change,
        wide_change,
    };

    let gets = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let store: Store = Arc::new(CountingBlobstore {
        inner: repo.repo_blobstore().clone(),
        gets: gets.clone(),
        bytes: bytes.clone(),
    });
    let counters = Counters { gets, bytes };

    print_header();
    for scenario in scenarios(limit) {
        let content = run_scenario(
            &ctx,
            &store,
            &content_roots,
            &fixture.changed_paths,
            &scenario,
            &counters,
        )
        .await?;
        print_measurement(&scenario, &content);
    }

    Ok(())
}
