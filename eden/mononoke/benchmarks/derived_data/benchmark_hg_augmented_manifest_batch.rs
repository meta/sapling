/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Benchmark for measuring RootHgAugmentedManifestId derivation performance
//! on linear stacks of commits.
//!
//! This benchmark measures how the derived data system handles deriving
//! RootHgAugmentedManifestId for a linear stack of commits - the same pattern
//! used in production batch derivation.
//!
//! **How it works:**
//! By deriving only the stack tip, DerivedDataManager calls `derive_batch()`
//! with all underived ancestors at once.
//!
//! **Comparing before/after optimization:**
//! - Before optimization: `MappedHgChangesetId` is derived as a dependency (sequentially),
//!   then `RootHgAugmentedManifestId` is derived with default sequential `derive_batch()`
//! - After optimization: Custom `derive_batch()` batch-derives HgChangesets inline
//!   and shares unchanged directory structure across the linear stack
//!
//! The benchmark does NOT pre-derive `MappedHgChangesetId` to ensure a fair comparison.
//! This way, the benchmark captures the full cost of deriving both data types, allowing
//! you to see the benefit of batch-deriving HgChangesets inline.
//!
//! **Benchmark Modes:**
//! - `shared` (default): Simulates realistic monorepo workloads where commits modify
//!   files in shared directory trees. This is typical of production patterns where
//!   developers work on the same projects and directories. Batch optimization should
//!   show improvement here due to manifest reuse.
//! - `isolated`: Each commit creates files in completely separate directory trees.
//!   This is a worst-case scenario where no manifest sharing is possible - useful
//!   for measuring the overhead of batch processing.
//!
//! Configuration:
//! - Configurable stack size (default: 10 commits)
//! - Configurable files per commit (default: 500)
//! - Realistic I/O latency (10ms GET, 15ms PUT)
//! - Blobstore operation counting
//!
//! Usage:
//! ```text
//!   buck2 run @mode/opt //eden/mononoke/benchmarks/derived_data:benchmark_hg_augmented_manifest_batch
//! ```
//!
//! Options:
//! ```text
//!   --mode <MODE>        Benchmark mode: 'shared' or 'isolated' (default: shared)
//!   --stack-size <N>     Number of commits in the stack (default: 10)
//!   --files <N>          Files per commit (default: 500)
//!   --no-delay           Disable I/O latency simulation
//! ```

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use benchmark_utils::BlobstoreCounters;
use benchmark_utils::GET_LATENCY_MS;
use benchmark_utils::PUT_LATENCY_MS;
use benchmark_utils::create_repo;
use benchmark_utils::gen_filename;
use clap::Parser;
use clap::ValueEnum;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use fbinit::FacebookInit;
use mercurial_derivation::RootHgAugmentedManifestId;
use mononoke_types::ChangesetId;
use rand::Rng;
use rand::RngExt as _;
use repo_derived_data::RepoDerivedDataRef;
use tests_utils::CreateCommitContext;

/// Benchmark mode determines how file paths are generated across commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum BenchmarkMode {
    /// Realistic monorepo: commits share directory trees
    Shared,
    /// Worst case: each commit has separate directories
    Isolated,
}

impl std::fmt::Display for BenchmarkMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BenchmarkMode::Shared => write!(f, "shared"),
            BenchmarkMode::Isolated => write!(f, "isolated"),
        }
    }
}

/// Benchmark for measuring RootHgAugmentedManifestId batch derivation
#[derive(Parser)]
#[clap(
    about = "Benchmark for measuring RootHgAugmentedManifestId derivation performance on linear stacks of commits."
)]
struct BenchmarkArgs {
    /// Benchmark mode
    #[clap(long, value_enum, default_value_t = BenchmarkMode::Shared)]
    mode: BenchmarkMode,

    /// Number of commits in the stack
    #[clap(long, default_value_t = 10)]
    stack_size: usize,

    /// Files per commit
    #[clap(long, default_value_t = 500)]
    files: usize,

    /// Disable I/O latency simulation
    #[clap(long)]
    no_delay: bool,
}

/// Generate a realistic deep directory path with 2-6 levels of depth.
///
/// In `Shared` mode: Uses shared top-level prefixes across all commits.
/// This simulates a monorepo where most commits modify files in existing
/// directories (e.g., different developers working on the same project).
/// The batch optimization benefits here because unchanged manifest subtrees
/// can be reused.
///
/// In `Isolated` mode: Uses commit-specific directory prefixes.
/// Each commit creates files in completely separate directory trees, so no
/// manifest nodes can be reused. This measures the overhead of batch
/// processing when there's no sharing benefit (worst case).
fn gen_realistic_path(rng: &mut impl Rng, commit_index: usize, mode: BenchmarkMode) -> String {
    // Shared top-level prefixes - simulate a monorepo structure
    const TOP_LEVEL_DIRS: &[&str] = &[
        "fbcode/project_alpha",
        "fbcode/project_beta",
        "fbcode/common/lib",
        "fbcode/services/backend",
        "xplat/mobile",
    ];
    const SUBDIRS: &[&str] = &["src", "lib", "tests", "bin", "common", "features", "utils"];
    const EXTENSIONS: &[&str] = &["rs", "py", "js", "cpp", "h", "java", "go", "ts"];

    let depth = rng.random_range(2..=5);
    let mut components = Vec::with_capacity(depth + 2);

    match mode {
        BenchmarkMode::Shared => {
            // Use shared top-level directories across all commits
            // This simulates realistic monorepo patterns where commits
            // touch files in the same project directories
            let top_level = TOP_LEVEL_DIRS[rng.random_range(0..TOP_LEVEL_DIRS.len())];
            components.push(top_level.to_string());
            components.push(SUBDIRS[rng.random_range(0..SUBDIRS.len())].to_string());
        }
        BenchmarkMode::Isolated => {
            // Add commit-specific prefix to ensure no overlap between commits
            // This creates the worst-case scenario for batch derivation
            components.push(format!("commit_{commit_index:03}"));
            components.push(SUBDIRS[rng.random_range(0..SUBDIRS.len())].to_string());
        }
    }

    // Add random subdirectories
    for _ in 0..depth {
        let len = rng.random_range(3..=12);
        components.push(gen_filename(rng, len));
    }

    // Add filename with extension
    let filename_len = rng.random_range(5..=20);
    let filename = gen_filename(rng, filename_len);
    let ext = EXTENSIONS[rng.random_range(0..EXTENSIONS.len())];
    components.push(format!("{filename}.{ext}"));

    components.join("/")
}

/// Create a linear stack of commits, each with its own set of files
async fn create_linear_commit_stack(
    ctx: &CoreContext,
    repo: &benchmark_utils::Repo,
    args: &BenchmarkArgs,
) -> Result<Vec<ChangesetId>> {
    let mut csids = Vec::with_capacity(args.stack_size);
    let mut parent_csid: Option<ChangesetId> = None;

    for commit_index in 0..args.stack_size {
        let mut paths = BTreeSet::new();
        let mut rng = rand::rng();

        while paths.len() < args.files {
            paths.insert(gen_realistic_path(&mut rng, commit_index, args.mode));
        }

        let mut create = if let Some(parent) = parent_csid {
            CreateCommitContext::new(ctx, repo, vec![parent])
        } else {
            CreateCommitContext::new_root(ctx, repo)
        };

        for path in paths.iter() {
            create = create.add_file(path.as_str(), format!("content of {path}"));
        }

        let csid = create.commit().await?;
        csids.push(csid);
        parent_csid = Some(csid);
    }

    Ok(csids)
}

#[fbinit::main]
async fn main(fb: FacebookInit) -> Result<()> {
    let args = BenchmarkArgs::parse();
    let use_delay = !args.no_delay;

    println!("=== HgAugmentedManifest Batch Derivation Benchmark ===");
    println!();
    println!("Configuration:");
    println!("  Mode: {}", args.mode);
    match args.mode {
        BenchmarkMode::Shared => {
            println!("         (Realistic monorepo: commits share directory trees)");
        }
        BenchmarkMode::Isolated => {
            println!("         (Worst case: each commit has separate directories)");
        }
    }
    println!("  Stack size: {} commits", args.stack_size);
    println!("  Files per commit: {}", args.files);
    if use_delay {
        println!("  I/O latency: {GET_LATENCY_MS:.0}ms GET / {PUT_LATENCY_MS:.0}ms PUT");
    } else {
        println!("  I/O latency: disabled");
    }
    println!();

    let ctx = CoreContext::test_mock(fb);
    let counters = Arc::new(BlobstoreCounters::new());
    let repo = create_repo(fb, counters.clone(), use_delay).await?;

    // Create a linear stack of commits
    println!("Creating linear commit stack...");
    counters.reset();
    let csids = create_linear_commit_stack(&ctx, &repo, &args).await?;
    println!("Created {} commits", csids.len());
    println!();

    // Derive RootHgAugmentedManifestId by deriving only the stack TIP.
    // This triggers derive_batch() with all underived ancestors at once.
    //
    // NOTE: We do NOT pre-derive MappedHgChangesetId. This is intentional:
    // - Before optimization: Derive manager derives MappedHgChangesetId as a dependency (sequential)
    // - After optimization: derive_batch() handles both HgChangesets and AugmentedManifests (batched)
    println!("Deriving RootHgAugmentedManifestId (including MappedHgChangesetId inline)...");
    counters.reset();
    let derive_start = Instant::now();

    let tip = csids.last().expect("stack should not be empty");
    repo.repo_derived_data()
        .derive::<RootHgAugmentedManifestId>(&ctx, *tip, DerivationPriority::LOW)
        .await?;

    let derive_time = derive_start.elapsed();
    let snap = counters.snapshot();
    let (total_gets, total_puts) = (snap.gets, snap.puts);

    // Print results
    println!();
    println!("=== RESULTS ===");
    println!(
        "Total time: {:.3}s for {} commits",
        derive_time.as_secs_f64(),
        csids.len()
    );
    println!(
        "Throughput: {:.2} commits/sec",
        csids.len() as f64 / derive_time.as_secs_f64()
    );
    println!("Blobstore: {total_gets} GETs, {total_puts} PUTs");
    println!(
        "Average per commit: {:.3}s, {:.0} GETs, {:.0} PUTs",
        derive_time.as_secs_f64() / csids.len() as f64,
        total_gets as f64 / csids.len() as f64,
        total_puts as f64 / csids.len() as f64
    );
    println!();

    Ok(())
}
