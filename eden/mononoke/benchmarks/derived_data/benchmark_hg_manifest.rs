/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Benchmark for measuring RootHgAugmentedManifestId derivation performance
//! with production-realistic settings.
//!
//! This benchmark measures the time to derive RootHgAugmentedManifestId when
//! MappedHgChangesetId (its dependency) is not yet derived. The derivation
//! system will automatically derive the dependency first.
//!
//! Configuration:
//! - Deep directory structure (2-6 levels, ~8000 unique directories)
//! - 10,000 files (configurable via --files)
//! - Realistic I/O latency (10ms GET, 15ms PUT) to simulate production blobstore
//! - Blobstore operation counting to track GETs and PUTs
//!
//! Usage:
//! ```text
//!   buck2 run @mode/opt //eden/mononoke/benchmarks/derived_data:benchmark_hg_manifest
//! ```
//!
//! Options:
//! ```text
//!   --files <count>              Number of files to create (default: 10000)
//!   --no-delay                   Disable I/O latency simulation
//! ```

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Result;
use benchmark_utils::BlobstoreCounters;
use benchmark_utils::GET_LATENCY_MS;
use benchmark_utils::PUT_LATENCY_MS;
use benchmark_utils::create_repo;
use benchmark_utils::gen_filename;
use clap::Parser;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use fbinit::FacebookInit;
use futures_stats::TimedFutureExt;
use mercurial_derivation::RootHgAugmentedManifestId;
use mononoke_types::ChangesetId;
use rand::Rng;
use rand::RngExt as _;
use repo_derived_data::RepoDerivedDataRef;
use tests_utils::CreateCommitContext;

/// Benchmark for measuring RootHgAugmentedManifestId derivation performance
#[derive(Parser)]
#[clap(
    about = "Benchmark for measuring RootHgAugmentedManifestId derivation performance with production-realistic settings."
)]
struct BenchmarkArgs {
    /// Number of files to create
    #[clap(long, default_value_t = 10_000)]
    files: usize,

    /// Disable I/O latency simulation
    #[clap(long)]
    no_delay: bool,
}

/// Generate a realistic deep directory path with 2-6 levels of depth.
fn gen_realistic_path(rng: &mut impl Rng) -> String {
    const PREFIXES: &[&str] = &["src", "lib", "tests", "bin", "common", "features"];
    const EXTENSIONS: &[&str] = &["rs", "py", "js", "cpp", "h", "java", "go", "ts"];

    let depth = rng.random_range(2..=6);
    let mut components = Vec::with_capacity(depth);

    components.push(PREFIXES[rng.random_range(0..PREFIXES.len())].to_string());

    for _ in 1..depth - 1 {
        let len = rng.random_range(3..=12);
        components.push(gen_filename(rng, len));
    }

    let filename_len = rng.random_range(5..=20);
    let filename = gen_filename(rng, filename_len);
    let ext = EXTENSIONS[rng.random_range(0..EXTENSIONS.len())];
    components.push(format!("{filename}.{ext}"));

    components.join("/")
}

async fn create_test_commit(
    ctx: &CoreContext,
    repo: &benchmark_utils::Repo,
    file_count: usize,
) -> Result<(ChangesetId, BTreeSet<String>)> {
    let mut paths = BTreeSet::new();
    let mut rng = rand::rng();

    while paths.len() < file_count {
        paths.insert(gen_realistic_path(&mut rng));
    }

    let mut create = CreateCommitContext::new_root(ctx, repo);
    for path in paths.iter() {
        create = create.add_file(path.as_str(), format!("content of {path}"));
    }
    let csid = create.commit().await?;

    Ok((csid, paths))
}

#[fbinit::main]
async fn main(fb: FacebookInit) -> Result<()> {
    // Parse command-line arguments
    let args = BenchmarkArgs::parse();
    let use_delay = !args.no_delay;

    // Uncomment to test JKs locally
    // mononoke::override_just_knobs();

    println!("=== HgManifest Derivation Benchmark ===");
    println!(
        "Configuration: {} files, deep directory structure",
        args.files
    );
    if use_delay {
        println!("I/O latency: {GET_LATENCY_MS:.0}ms GET / {PUT_LATENCY_MS:.0}ms PUT");
    } else {
        println!("I/O latency: disabled (no-delay mode)");
    }
    println!();

    let ctx = CoreContext::test_mock(fb);

    // Create shared counters
    let counters = Arc::new(BlobstoreCounters::new());

    let repo = create_repo(fb, counters.clone(), use_delay).await?;

    // Reset counters before commit creation to exclude setup operations
    counters.reset();
    let (csid, paths) = create_test_commit(&ctx, &repo, args.files).await?;

    println!(
        "Test commit: {} files, {} directories",
        paths.len(),
        paths.iter().filter_map(|p| p.rfind('/')).count()
    );

    // Derive RootHgAugmentedManifestId - this will derive MappedHgChangesetId
    // as a dependency first, then derive the augmented manifest.
    counters.reset();

    let (stats, result) = repo
        .repo_derived_data()
        .derive::<RootHgAugmentedManifestId>(&ctx, csid, DerivationPriority::LOW)
        .timed()
        .await;
    result?;
    let snap = counters.snapshot();
    let (gets, puts) = (snap.gets, snap.puts);

    println!();
    println!(
        "Total derivation time: {:.3}s",
        stats.completion_time.as_secs_f64()
    );
    println!();
    println!("Blobstore operations: {gets} GETs, {puts} PUTs");

    Ok(())
}
