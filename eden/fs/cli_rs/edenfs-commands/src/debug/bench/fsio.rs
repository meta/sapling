/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Filesystem I/O benchmarking

mod comparison;
mod daemon;

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::Command;
#[cfg(target_os = "linux")]
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
pub(crate) use comparison::load_result;
pub(crate) use daemon::DaemonSampler;
use edenfs_client::instance::EdenFsInstance;
#[cfg(target_os = "linux")]
use edenfs_utils::trace_env;
use hdrhistogram::Histogram;
use indicatif::ProgressBar;
use indicatif::ProgressStyle;
use serde::Deserialize;
use serde::Serialize;
use virtual_repo::generate_workload;
use virtual_repo::text_gen::generate_paragraphs;

use super::r#gen::TestDir;
use super::types;

const CONTENT_BUFFER_SIZE: usize = 4 * types::BYTES_IN_MEGABYTE;
const FILE_SIZE_STEP_UPPER_BOUNDS: &[u64] = &[
    511, 4_095, 16_383, 65_535, 262_143, 524_287, 1_048_575, 5_242_879, 20_971_520,
];
const MAX_TRACKED_LATENCY_NANOSECONDS: u64 = 60 * 60 * 1_000_000_000;
/// Files a worker takes from the shared cursor at a time: enough to keep
/// neighboring files on one thread, few enough that the threads finish
/// together.
const WORK_BATCH: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KernelCacheDropMode {
    Disabled,
    PageCache,
    DentriesInodes,
    PageCacheDentriesInodes,
}

impl KernelCacheDropMode {
    /// The numeric mode written to /proc/sys/vm/drop_caches and recorded in
    /// benchmark results.
    fn value(self) -> u8 {
        match self {
            Self::Disabled => 0,
            Self::PageCache => 1,
            Self::DentriesInodes => 2,
            Self::PageCacheDentriesInodes => 3,
        }
    }

    fn drops_page_cache(self) -> bool {
        matches!(self, Self::PageCache | Self::PageCacheDentriesInodes)
    }

    fn description(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::PageCache => "Linux drop_caches mode 1: page cache",
            Self::DentriesInodes => "Linux drop_caches mode 2: dentries and inodes",
            Self::PageCacheDentriesInodes => {
                "Linux drop_caches mode 3: page cache, dentries, and inodes"
            }
        }
    }
}

pub(crate) fn parse_kernel_cache_drop_mode(value: &str) -> Result<KernelCacheDropMode> {
    let mode = value
        .parse::<u8>()
        .map_err(|error| anyhow!("invalid kernel-cache mode {value:?}: {error}"))?;
    match mode {
        0 => Ok(KernelCacheDropMode::Disabled),
        1 => Ok(KernelCacheDropMode::PageCache),
        2 => Ok(KernelCacheDropMode::DentriesInodes),
        3 => Ok(KernelCacheDropMode::PageCacheDentriesInodes),
        _ => Err(anyhow!("kernel-cache mode must be between 0 and 3")),
    }
}

pub(crate) fn resolve_kernel_cache_drop_mode(
    requested: Option<KernelCacheDropMode>,
    default: KernelCacheDropMode,
) -> Result<KernelCacheDropMode> {
    resolve_kernel_cache_drop_mode_for_platform(requested, default, cfg!(target_os = "linux"))
}

fn resolve_kernel_cache_drop_mode_for_platform(
    requested: Option<KernelCacheDropMode>,
    default: KernelCacheDropMode,
    supported: bool,
) -> Result<KernelCacheDropMode> {
    if supported {
        return Ok(requested.unwrap_or(default));
    }
    match requested {
        None | Some(KernelCacheDropMode::Disabled) => Ok(KernelCacheDropMode::Disabled),
        Some(_) => Err(anyhow!("--drop-kernel-caches is supported only on Linux")),
    }
}

/// Where the files of a run come from.
#[derive(Clone, Copy)]
pub(crate) enum FsIoSource<'a> {
    /// Files the run writes under the benchmark directory, reads back, and
    /// removes.
    Generated(&'a TestDir),
    /// The regular files already under a directory, in listing order.
    Tree(&'a TestDir),
    /// The paths listed one per line in a file, in list order.
    FileList(&'a Path),
}

impl FsIoSource<'_> {
    fn read_only(self) -> bool {
        !matches!(self, Self::Generated(_))
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FsIoOptions {
    /// Cap on the files in the run. `None` is the default count for
    /// generated and tree runs and the whole list for a file list.
    pub(crate) number_of_files: Option<NonZeroUsize>,
    pub(crate) files_per_dir: Option<NonZeroUsize>,
    pub(crate) file_size: Option<usize>,
    pub(crate) read_size: NonZeroUsize,
    pub(crate) write_size: NonZeroUsize,
    pub(crate) jobs: NonZeroUsize,
    pub(crate) drop_kernel_caches: KernelCacheDropMode,
    pub(crate) show_progress: bool,
}

impl FsIoOptions {
    fn default_file_count(self) -> usize {
        self.number_of_files
            .map_or(types::DEFAULT_NUMBER_OF_FILES, NonZeroUsize::get)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct FsIoResult {
    config: FsIoConfig,
    // Absent for a read-only run.
    #[serde(default)]
    write: Option<PhaseResult>,
    cache_preparation: CachePreparationResult,
    read: PhaseResult,
    // Optional so baselines recorded before the remove phase existed can
    // still be loaded for --diff.
    #[serde(default)]
    remove: Option<RemoveResult>,
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    diff: Option<comparison::FsIoDiff>,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct FsIoConfig {
    #[serde(default)]
    read_only: bool,
    /// The files came from a list rather than a directory tree.
    #[serde(default)]
    file_list: bool,
    /// Listed paths that were not regular files when the run was planned.
    #[serde(default)]
    skipped_files: u64,
    number_of_files: usize,
    files_per_dir: Option<usize>,
    virtual_repo_factor: u8,
    read_size_bytes: usize,
    write_size_bytes: usize,
    jobs: usize,
    drop_kernel_caches: u8,
    total_bytes: u64,
    file_sizes: Vec<FileSizeConfig>,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct FileSizeConfig {
    size_bytes: usize,
    files: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct CachePreparationResult {
    drop_kernel_caches: u8,
    wall_seconds: f64,
    file_sync: OperationResult,
    cache_drop_method: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct PhaseResult {
    wall_seconds: f64,
    throughput_mib_per_second: f64,
    /// Time inside the measured filesystem calls, summed across jobs. The
    /// wall time also holds the benchmark's own work between calls, which
    /// is the same for every filesystem and hides differences between them.
    #[serde(default)]
    syscall_seconds: f64,
    #[serde(default)]
    syscall_nanoseconds_per_file: f64,
    files: u64,
    directories: u64,
    bytes: u64,
    directory_create: OperationResult,
    directory_open: OperationResult,
    directory_read: OperationResult,
    directory_close: OperationResult,
    file_open: OperationResult,
    file_io: OperationResult,
    file_close: OperationResult,
    per_file_size: Vec<FileSizeResult>,
    /// What the EdenFS daemon did during the phase; absent when the files
    /// are not on EdenFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    eden: Option<daemon::DaemonPhaseResult>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FileSizeResult {
    size_bytes: usize,
    files: u64,
    bytes: u64,
    sum_seconds: f64,
    average_nanoseconds_per_file: f64,
    p50_nanoseconds_per_file: u64,
    p90_nanoseconds_per_file: u64,
    p99_nanoseconds_per_file: u64,
    file_open: OperationResult,
    file_io: OperationResult,
    file_close: OperationResult,
}

#[derive(Debug, Deserialize, Serialize)]
struct RemoveResult {
    wall_seconds: f64,
    files_per_second: f64,
    #[serde(default)]
    syscall_seconds: f64,
    #[serde(default)]
    syscall_nanoseconds_per_file: f64,
    files: u64,
    directories: u64,
    file_unlink: OperationResult,
    directory_remove: OperationResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    eden: Option<daemon::DaemonPhaseResult>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct OperationResult {
    calls: u64,
    sum_seconds: f64,
    average_nanoseconds: f64,
    p50_nanoseconds: u64,
    p90_nanoseconds: u64,
    p99_nanoseconds: u64,
}

struct Workload {
    files: Vec<FileSpec>,
    directories: Vec<PathBuf>,
    file_sizes: Vec<usize>,
    total_bytes: u64,
    virtual_repo_factor: u8,
    /// Listed paths that were not regular files when the run was planned.
    skipped_files: u64,
}

struct FileSpec {
    path: PathBuf,
    size: usize,
    size_index: usize,
}

struct OperationAccumulator {
    calls: u64,
    elapsed: Duration,
    latencies_nanoseconds: Histogram<u64>,
}

/// The directory-level operations of one benchmark phase. Grouped so phase
/// construction can't silently swap two accumulators of the same type.
#[derive(Default)]
struct DirectoryOperations {
    create: OperationAccumulator,
    open: OperationAccumulator,
    read: OperationAccumulator,
    close: OperationAccumulator,
}

/// Timings of one file's open/IO/close sequence, recorded into the phase
/// accumulators by `record_file`.
struct FileTiming {
    open: Duration,
    io: OperationAccumulator,
    close: Duration,
}

struct FileSizeAccumulator {
    size: usize,
    files: u64,
    bytes: u64,
    file_total: OperationAccumulator,
    file_open: OperationAccumulator,
    file_io: OperationAccumulator,
    file_close: OperationAccumulator,
}

struct FileAccumulator {
    files: u64,
    bytes: u64,
    file_open: OperationAccumulator,
    file_io: OperationAccumulator,
    file_close: OperationAccumulator,
    per_file_size: Vec<FileSizeAccumulator>,
}

impl OperationAccumulator {
    fn new() -> Self {
        Self {
            calls: 0,
            elapsed: Duration::default(),
            latencies_nanoseconds: Histogram::new_with_bounds(
                1,
                MAX_TRACKED_LATENCY_NANOSECONDS,
                3,
            )
            .expect("latency histogram bounds are valid"),
        }
    }

    fn record(&mut self, elapsed: Duration) {
        self.calls += 1;
        self.elapsed += elapsed;
        let latency = u64::try_from(elapsed.as_nanos())
            .unwrap_or(MAX_TRACKED_LATENCY_NANOSECONDS)
            .clamp(1, MAX_TRACKED_LATENCY_NANOSECONDS);
        self.latencies_nanoseconds
            .record(latency)
            .expect("clamped latency fits the histogram");
    }

    fn merge(&mut self, other: &Self) {
        self.calls += other.calls;
        self.elapsed += other.elapsed;
        self.latencies_nanoseconds
            .add(&other.latencies_nanoseconds)
            .expect("latency histograms use identical bounds");
    }

    fn result(self) -> OperationResult {
        OperationResult {
            calls: self.calls,
            sum_seconds: self.elapsed.as_secs_f64(),
            average_nanoseconds: if self.calls == 0 {
                0.0
            } else {
                self.elapsed.as_secs_f64() * 1_000_000_000.0 / self.calls as f64
            },
            p50_nanoseconds: self.latencies_nanoseconds.value_at_quantile(0.50),
            p90_nanoseconds: self.latencies_nanoseconds.value_at_quantile(0.90),
            p99_nanoseconds: self.latencies_nanoseconds.value_at_quantile(0.99),
        }
    }
}

impl Default for OperationAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSizeAccumulator {
    fn new(size: usize) -> Self {
        Self {
            size,
            files: 0,
            bytes: 0,
            file_total: OperationAccumulator::default(),
            file_open: OperationAccumulator::default(),
            file_io: OperationAccumulator::default(),
            file_close: OperationAccumulator::default(),
        }
    }

    fn merge(&mut self, other: &Self) {
        self.files += other.files;
        self.bytes += other.bytes;
        self.file_total.merge(&other.file_total);
        self.file_open.merge(&other.file_open);
        self.file_io.merge(&other.file_io);
        self.file_close.merge(&other.file_close);
    }

    fn result(self) -> FileSizeResult {
        let file_total = self.file_total.result();
        FileSizeResult {
            size_bytes: self.size,
            files: self.files,
            bytes: self.bytes,
            sum_seconds: file_total.sum_seconds,
            average_nanoseconds_per_file: file_total.average_nanoseconds,
            p50_nanoseconds_per_file: file_total.p50_nanoseconds,
            p90_nanoseconds_per_file: file_total.p90_nanoseconds,
            p99_nanoseconds_per_file: file_total.p99_nanoseconds,
            file_open: self.file_open.result(),
            file_io: self.file_io.result(),
            file_close: self.file_close.result(),
        }
    }
}

impl FileAccumulator {
    fn new(file_sizes: &[usize]) -> Self {
        Self {
            files: 0,
            bytes: 0,
            file_open: OperationAccumulator::default(),
            file_io: OperationAccumulator::default(),
            file_close: OperationAccumulator::default(),
            per_file_size: file_sizes
                .iter()
                .copied()
                .map(FileSizeAccumulator::new)
                .collect(),
        }
    }

    fn merge(&mut self, other: Self) {
        self.files += other.files;
        self.bytes += other.bytes;
        self.file_open.merge(&other.file_open);
        self.file_io.merge(&other.file_io);
        self.file_close.merge(&other.file_close);
        for (size, other_size) in self.per_file_size.iter_mut().zip(&other.per_file_size) {
            size.merge(other_size);
        }
    }
}

/// Runs the benchmark. With an `instance`, the EdenFS daemon serving the
/// files is sampled around each phase when the files are on EdenFS.
pub(crate) async fn bench_fs_io(
    source: FsIoSource<'_>,
    options: FsIoOptions,
    baseline: Option<&FsIoResult>,
    instance: Option<&EdenFsInstance>,
) -> Result<FsIoResult> {
    let read_only = source.read_only();
    let workload = match source {
        FsIoSource::Generated(test_dir) => Workload::new(test_dir, options)?,
        FsIoSource::Tree(test_dir) => Workload::from_tree(&test_dir.path, options)?,
        FsIoSource::FileList(list) => Workload::from_file_list(list, options)?,
    };
    let config = fs_io_config(source, options, &workload);
    if let Some(baseline) = baseline {
        // Fail before the benchmark runs rather than after, so a baseline
        // recorded with a different workload doesn't waste a full run.
        comparison::ensure_config_matches(&config, &baseline.config)?;
    }
    let mut sampler = match instance {
        Some(instance) => {
            // Generated files do not exist yet; their directory does.
            let path = match source {
                FsIoSource::Generated(test_dir) | FsIoSource::Tree(test_dir) => &test_dir.path,
                FsIoSource::FileList(_) => workload.files[0]
                    .path
                    .parent()
                    .unwrap_or(&workload.files[0].path),
            };
            DaemonSampler::for_path(instance, path).await?
        }
        None => None,
    };

    let write = if read_only {
        None
    } else {
        // Extend the buffer by one write so every chunk is a contiguous slice
        // even when the cyclic offset is near the end of the buffer.
        let content =
            generate_paragraphs(CONTENT_BUFFER_SIZE + options.write_size.get(), 0).into_bytes();
        let write_progress =
            file_progress_bar("Write", workload.files.len(), options.show_progress);
        let write = daemon::measure_phase(sampler.as_mut(), || {
            bench_write(&workload, &content, options, &write_progress)
        })
        .await;
        write_progress.finish_and_clear();
        let (mut write, eden) = write?;
        write.eden = eden;
        Some(write)
    };

    // Only files this run wrote can have dirty pages to sync away.
    let files_to_sync: &[FileSpec] = if read_only { &[] } else { &workload.files };
    let cache_progress = cache_drop_spinner(
        options.drop_kernel_caches,
        options.show_progress,
        files_to_sync.len(),
    );
    let cache_preparation =
        prepare_read_cache(files_to_sync, options.drop_kernel_caches, &cache_progress);
    cache_progress.finish_and_clear();
    let cache_preparation = cache_preparation?;

    let read_progress = file_progress_bar("Read", workload.files.len(), options.show_progress);
    let read = daemon::measure_phase(sampler.as_mut(), || {
        bench_read(source, &workload, options, &read_progress)
    })
    .await;
    read_progress.finish_and_clear();
    let (mut read, eden) = read?;
    read.eden = eden;

    let remove = if read_only {
        None
    } else {
        let remove_progress =
            file_progress_bar("Remove", workload.files.len(), options.show_progress);
        let remove = daemon::measure_phase(sampler.as_mut(), || {
            bench_remove(&workload, options, &remove_progress)
        })
        .await;
        remove_progress.finish_and_clear();
        let (mut remove, eden) = remove?;
        remove.eden = eden;
        Some(remove)
    };
    Ok(FsIoResult {
        config,
        write,
        cache_preparation,
        read,
        remove,
        diff: None,
    })
}

/// The run's configuration, derived from the planned workload before any
/// I/O happens so a `--diff` baseline mismatch can be rejected up front.
fn fs_io_config(source: FsIoSource<'_>, options: FsIoOptions, workload: &Workload) -> FsIoConfig {
    let mut files_per_band = vec![0u64; workload.file_sizes.len()];
    for file in &workload.files {
        files_per_band[file.size_index] += 1;
    }
    FsIoConfig {
        read_only: source.read_only(),
        file_list: matches!(source, FsIoSource::FileList(_)),
        skipped_files: workload.skipped_files,
        number_of_files: workload.files.len(),
        files_per_dir: options.files_per_dir.map(NonZeroUsize::get),
        virtual_repo_factor: workload.virtual_repo_factor,
        read_size_bytes: options.read_size.get(),
        write_size_bytes: options.write_size.get(),
        jobs: options.jobs.get(),
        drop_kernel_caches: options.drop_kernel_caches.value(),
        total_bytes: workload.total_bytes,
        file_sizes: workload
            .file_sizes
            .iter()
            .zip(files_per_band)
            .map(|(&size_bytes, files)| FileSizeConfig { size_bytes, files })
            .collect(),
    }
}

fn bench_remove(
    workload: &Workload,
    options: FsIoOptions,
    progress: &ProgressBar,
) -> Result<RemoveResult> {
    let wall_start = Instant::now();
    let file_unlink = run_unlink_workers(&workload.files, options.jobs, progress)?;

    // Directories were recorded parents-before-children while generating the
    // workload, so removing in reverse order empties children first. Removal
    // stays sequential, unlike file unlinks: parallel workers would need
    // cross-worker ordering to keep every child ahead of its parent, and
    // directories are few relative to files.
    let mut directory_remove = OperationAccumulator::default();
    for path in workload.directories.iter().rev() {
        let start = Instant::now();
        fs::remove_dir(path).with_context(|| format!("failed to remove {}", path.display()))?;
        directory_remove.record(start.elapsed());
    }

    let wall_seconds = wall_start.elapsed().as_secs_f64();
    let syscall_seconds = (file_unlink.elapsed + directory_remove.elapsed).as_secs_f64();
    Ok(RemoveResult {
        wall_seconds,
        files_per_second: if wall_seconds > 0.0 {
            workload.files.len() as f64 / wall_seconds
        } else {
            0.0
        },
        syscall_seconds,
        syscall_nanoseconds_per_file: nanoseconds_per_file(
            syscall_seconds,
            workload.files.len() as u64,
        ),
        files: workload.files.len() as u64,
        directories: workload.directories.len() as u64,
        file_unlink: file_unlink.result(),
        directory_remove: directory_remove.result(),
        eden: None,
    })
}

fn run_unlink_workers(
    files: &[FileSpec],
    jobs: NonZeroUsize,
    progress: &ProgressBar,
) -> Result<OperationAccumulator> {
    run_batched_workers(
        files,
        jobs,
        OperationAccumulator::default,
        |batch, accumulator| {
            for file in batch {
                let start = Instant::now();
                fs::remove_file(&file.path)
                    .with_context(|| format!("failed to unlink {}", file.path.display()))?;
                accumulator.record(start.elapsed());
            }
            progress.inc(batch.len() as u64);
            Ok(())
        },
        |workers| {
            let mut result = OperationAccumulator::default();
            for worker in &workers {
                result.merge(worker);
            }
            Ok(result)
        },
    )
}

fn file_progress_bar(name: &str, files: usize, show_progress: bool) -> ProgressBar {
    if !show_progress {
        return ProgressBar::hidden();
    }

    let progress = ProgressBar::new(files as u64);
    progress.set_style(
        ProgressStyle::default_bar()
            .template(
                "[{elapsed_precise}] {msg:<5} [{bar:40.cyan/blue}] {pos:>8}/{len:8} files {per_sec:>12} ETA {eta_precise}",
            )
            .expect("progress bar template is valid")
            .progress_chars("=>-"),
    );
    progress.set_message(name.to_owned());
    progress
}

fn cache_drop_spinner(mode: KernelCacheDropMode, show_progress: bool, files: usize) -> ProgressBar {
    if !show_progress || mode == KernelCacheDropMode::Disabled {
        return ProgressBar::hidden();
    }

    let progress = ProgressBar::new_spinner();
    progress.set_style(
        ProgressStyle::default_spinner()
            .template("[{elapsed_precise}] {spinner} {msg}")
            .expect("cache-drop spinner template is valid"),
    );
    progress.set_message(if mode.drops_page_cache() && files > 0 {
        format!("Syncing {files} files before dropping kernel caches")
    } else {
        format!("Dropping kernel caches (mode {})", mode.value())
    });
    progress.enable_steady_tick(Duration::from_millis(100));
    progress
}

impl Workload {
    fn new(test_dir: &TestDir, options: FsIoOptions) -> Result<Self> {
        let generated = generate_workload(options.default_file_count())?;
        let sizes = generated
            .files
            .iter()
            .map(|file| file.size)
            .collect::<Vec<_>>();
        let (file_sizes, size_indexes) = file_size_steps(&sizes, options.file_size)?;
        let mut files = Vec::with_capacity(generated.files.len());
        let data_root = test_dir.path.join("data");
        let mut directories = vec![data_root.clone()];
        let mut seen_directories = HashSet::from([data_root.clone()]);

        for (ordinal, (generated_file, size_index)) in
            generated.files.iter().zip(size_indexes).enumerate()
        {
            let relative_path = options.files_per_dir.map_or_else(
                || PathBuf::from(&generated_file.path).with_file_name(benchmark_file_name(ordinal)),
                |files_per_dir| manual_relative_path(ordinal, files_per_dir.get()),
            );
            add_parent_directories(
                &data_root,
                &relative_path,
                &mut seen_directories,
                &mut directories,
            );
            files.push(FileSpec {
                path: data_root.join(relative_path),
                size: file_sizes[size_index],
                size_index,
            });
        }
        let total_bytes = files.iter().map(|file| file.size as u64).sum();

        Ok(Self {
            files,
            directories,
            file_sizes,
            total_bytes,
            virtual_repo_factor: generated.factor_bits,
            skipped_files: 0,
        })
    }

    /// The regular files already under `root`, in directory-listing order and
    /// capped at `options.number_of_files`. Sizes come from an untimed stat so
    /// the read phase can verify and bucket each file like a generated one.
    fn from_tree(root: &Path, options: FsIoOptions) -> Result<Self> {
        let listing = list_files(root, options.default_file_count())?;
        if listing.files.is_empty() {
            return Err(anyhow!("no regular files found under {}", root.display()));
        }
        Self::from_listed_files(listing.files, listing.directories, 0)
    }

    /// The paths listed one per line in `list`, in list order and capped at
    /// `options.number_of_files`. Paths that are not regular files are
    /// skipped and counted, since a list recorded from a build names files
    /// that come and go.
    fn from_file_list(list: &Path, options: FsIoOptions) -> Result<Self> {
        let contents = fs::read_to_string(list)
            .with_context(|| format!("failed to read file list {}", list.display()))?;
        let max_files = options
            .number_of_files
            .map_or(usize::MAX, NonZeroUsize::get);
        let mut files = Vec::new();
        let mut skipped_files = 0;
        for line in contents.lines().filter(|line| !line.is_empty()) {
            if files.len() == max_files {
                break;
            }
            let path = PathBuf::from(line);
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => files.push(ListedFile {
                    path,
                    size: usize::try_from(metadata.len())
                        .context("file size does not fit usize")?,
                }),
                Ok(_) | Err(_) => skipped_files += 1,
            }
        }
        if files.is_empty() {
            return Err(anyhow!("no regular files listed in {}", list.display()));
        }
        Self::from_listed_files(files, Vec::new(), skipped_files)
    }

    fn from_listed_files(
        listed: Vec<ListedFile>,
        directories: Vec<PathBuf>,
        skipped_files: u64,
    ) -> Result<Self> {
        let sizes = listed
            .iter()
            .map(|file| file.size as u64)
            .collect::<Vec<_>>();
        let (file_sizes, size_indexes) = file_size_steps(&sizes, None)?;
        let files = listed
            .into_iter()
            .zip(size_indexes)
            .map(|(file, size_index)| FileSpec {
                path: file.path,
                size: file.size,
                size_index,
            })
            .collect::<Vec<_>>();
        let total_bytes = files.iter().map(|file| file.size as u64).sum();

        Ok(Self {
            files,
            directories,
            file_sizes,
            total_bytes,
            virtual_repo_factor: 0,
            skipped_files,
        })
    }
}

fn file_size_steps(
    sizes: &[u64],
    override_size: Option<usize>,
) -> Result<(Vec<usize>, Vec<usize>)> {
    if let Some(size) = override_size {
        return Ok((vec![size], vec![0; sizes.len()]));
    }

    let bucket_count = FILE_SIZE_STEP_UPPER_BOUNDS.len() + 1;
    let mut counts = vec![0_u64; bucket_count];
    let mut sums = vec![0_u128; bucket_count];
    let buckets = sizes
        .iter()
        .map(|size| {
            let bucket = FILE_SIZE_STEP_UPPER_BOUNDS.partition_point(|upper| upper < size);
            counts[bucket] += 1;
            sums[bucket] += *size as u128;
            bucket
        })
        .collect::<Vec<_>>();

    let mut bucket_to_size_index = vec![None; bucket_count];
    let mut file_sizes = Vec::new();
    for bucket in 0..bucket_count {
        if counts[bucket] == 0 {
            continue;
        }
        let average = (sums[bucket] + (counts[bucket] / 2) as u128) / counts[bucket] as u128;
        let size = usize::try_from(average).context("generated file size does not fit usize")?;
        bucket_to_size_index[bucket] = Some(file_sizes.len());
        file_sizes.push(size);
    }

    let size_indexes = buckets
        .into_iter()
        .map(|bucket| bucket_to_size_index[bucket].expect("occupied bucket has a size step"))
        .collect();
    Ok((file_sizes, size_indexes))
}

fn manual_relative_path(ordinal: usize, files_per_dir: usize) -> PathBuf {
    let leaf = ordinal / files_per_dir;
    PathBuf::from(format!("{:02x}", leaf >> 16))
        .join(format!("{:02x}", (leaf >> 8) & 0xff))
        .join(format!("{:02x}", leaf & 0xff))
        .join(benchmark_file_name(ordinal))
}

fn benchmark_file_name(ordinal: usize) -> String {
    format!("file-{ordinal:08}.txt")
}

fn add_parent_directories(
    data_root: &Path,
    relative_file: &Path,
    seen: &mut HashSet<PathBuf>,
    directories: &mut Vec<PathBuf>,
) {
    let Some(parent) = relative_file.parent() else {
        return;
    };
    let mut directory = data_root.to_path_buf();
    for component in parent.components() {
        directory.push(component);
        if seen.insert(directory.clone()) {
            directories.push(directory.clone());
        }
    }
}

fn bench_write(
    workload: &Workload,
    content: &[u8],
    options: FsIoOptions,
    progress: &ProgressBar,
) -> Result<PhaseResult> {
    let wall_start = Instant::now();
    let mut directory_create = OperationAccumulator::default();
    for path in &workload.directories {
        let start = Instant::now();
        fs::create_dir(path).with_context(|| format!("failed to create {}", path.display()))?;
        directory_create.record(start.elapsed());
    }
    let files = run_file_workers(
        workload,
        options.jobs,
        |file, accumulator| write_file(file, content, options.write_size.get(), accumulator),
        progress,
    )?;
    let wall_seconds = wall_start.elapsed().as_secs_f64();

    Ok(phase_result(
        wall_seconds,
        workload.directories.len() as u64,
        DirectoryOperations {
            create: directory_create,
            ..Default::default()
        },
        files,
    ))
}

fn write_file(
    file: &FileSpec,
    content: &[u8],
    write_size: usize,
    accumulator: &mut FileAccumulator,
) -> Result<()> {
    let cycle = content.len() - write_size;
    debug_assert!(
        cycle > 0,
        "content must extend at least one write past the cycle"
    );
    let start = Instant::now();
    let mut output = File::create(&file.path)
        .with_context(|| format!("failed to create {}", file.path.display()))?;
    let open_elapsed = start.elapsed();

    let mut remaining = file.size;
    let mut file_offset = 0;
    let mut file_io = OperationAccumulator::default();
    while remaining > 0 {
        let content_offset = file_offset % cycle;
        let bytes = remaining.min(write_size);
        let mut written = 0;
        while written < bytes {
            let start = Instant::now();
            let result = output.write(&content[content_offset + written..content_offset + bytes]);
            file_io.record(start.elapsed());
            match result {
                Ok(0) => {
                    return Err(anyhow!("write returned zero for {}", file.path.display()));
                }
                Ok(result) => written += result,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        file_offset += written;
        remaining -= written;
    }

    let start = Instant::now();
    drop(output);
    let close_elapsed = start.elapsed();
    record_file(
        accumulator,
        file,
        file.size as u64,
        FileTiming {
            open: open_elapsed,
            io: file_io,
            close: close_elapsed,
        },
    );
    Ok(())
}

fn prepare_read_cache(
    files: &[FileSpec],
    drop_kernel_caches: KernelCacheDropMode,
    progress: &ProgressBar,
) -> Result<CachePreparationResult> {
    let wall_start = Instant::now();
    let mut file_sync = OperationAccumulator::default();
    if drop_kernel_caches.drops_page_cache() {
        for file in files {
            let input = File::open(&file.path)
                .with_context(|| format!("failed to open {} for syncing", file.path.display()))?;
            let start = Instant::now();
            input
                .sync_data()
                .with_context(|| format!("failed to sync {}", file.path.display()))?;
            file_sync.record(start.elapsed());
        }
    }
    if drop_kernel_caches != KernelCacheDropMode::Disabled {
        progress.set_message(format!(
            "Dropping kernel caches (mode {})",
            drop_kernel_caches.value()
        ));
        drop_kernel_caches_on_linux(drop_kernel_caches)?;
    }

    Ok(CachePreparationResult {
        drop_kernel_caches: drop_kernel_caches.value(),
        wall_seconds: wall_start.elapsed().as_secs_f64(),
        file_sync: file_sync.result(),
        cache_drop_method: drop_kernel_caches.description().to_owned(),
    })
}

fn bench_read(
    source: FsIoSource<'_>,
    workload: &Workload,
    options: FsIoOptions,
    progress: &ProgressBar,
) -> Result<PhaseResult> {
    let wall_start = Instant::now();
    let (file_indexes, directory_operations) = match source {
        FsIoSource::Generated(test_dir) => enumerate_files(&test_dir.path, workload, false)?,
        FsIoSource::Tree(test_dir) => enumerate_files(&test_dir.path, workload, true)?,
        // A list is read in its own order, with no directory listing.
        FsIoSource::FileList(_) => (
            (0..workload.files.len()).collect(),
            DirectoryOperations::default(),
        ),
    };
    let files = run_read_workers(
        ReadPlan {
            files: &workload.files,
            file_sizes: &workload.file_sizes,
            order: &file_indexes,
        },
        options.read_size,
        options.jobs,
        progress,
    )?;
    let wall_seconds = wall_start.elapsed().as_secs_f64();

    Ok(phase_result(
        wall_seconds,
        // Report the workload's directory count, matching the write phase.
        // The open accumulator additionally counts the pre-existing benchmark
        // root opened by enumerate_files.
        workload.directories.len() as u64,
        directory_operations,
        files,
    ))
}

/// Lists the workload's files in directory order, timing the directory
/// calls. A file that is not part of the workload is an error for a generated
/// workload; a read-only run reads a prefix of a tree, so there it is skipped.
fn enumerate_files(
    root: &Path,
    workload: &Workload,
    allow_unknown_files: bool,
) -> Result<(Vec<usize>, DirectoryOperations)> {
    let index_by_path = workload
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path.as_path(), index))
        .collect::<HashMap<_, _>>();
    let mut file_indexes = Vec::with_capacity(workload.files.len());
    let (_, operations) = walk_directories(root, |entry, file_type| {
        if !file_type.is_file() {
            return Ok(Walk::Continue);
        }
        let path = entry.path();
        match index_by_path.get(path.as_path()) {
            Some(index) => file_indexes.push(*index),
            None if allow_unknown_files => {}
            None => return Err(anyhow!("unexpected benchmark file {}", path.display())),
        }
        Ok(if file_indexes.len() < workload.files.len() {
            Walk::Continue
        } else {
            Walk::Stop
        })
    })?;
    if file_indexes.len() != workload.files.len() {
        return Err(anyhow!(
            "enumerated {} files but expected {}",
            file_indexes.len(),
            workload.files.len()
        ));
    }

    Ok((file_indexes, operations))
}

struct ListedFile {
    path: PathBuf,
    size: usize,
}

struct Listing {
    files: Vec<ListedFile>,
    directories: Vec<PathBuf>,
}

/// Lists up to `max_files` regular files under `root` with their sizes.
fn list_files(root: &Path, max_files: usize) -> Result<Listing> {
    let mut files = Vec::new();
    let (directories, _) = walk_directories(root, |entry, file_type| {
        if !file_type.is_file() {
            return Ok(Walk::Continue);
        }
        let metadata = entry
            .metadata()
            .with_context(|| format!("failed to stat {}", entry.path().display()))?;
        files.push(ListedFile {
            path: entry.path(),
            size: usize::try_from(metadata.len()).context("file size does not fit usize")?,
        });
        Ok(if files.len() < max_files {
            Walk::Continue
        } else {
            Walk::Stop
        })
    })?;

    Ok(Listing { files, directories })
}

enum Walk {
    Continue,
    Stop,
}

/// Walks a directory tree depth first, timing each directory's open, read,
/// and close calls, and hands every entry that is not a directory to `visit`.
/// Returns the directories it opened, in order.
fn walk_directories<F>(root: &Path, mut visit: F) -> Result<(Vec<PathBuf>, DirectoryOperations)>
where
    F: FnMut(fs::DirEntry, fs::FileType) -> Result<Walk>,
{
    let mut operations = DirectoryOperations::default();
    let mut pending = vec![root.to_path_buf()];
    let mut visited = Vec::new();
    while let Some(directory) = pending.pop() {
        let start = Instant::now();
        let mut entries = fs::read_dir(&directory)
            .with_context(|| format!("failed to open directory {}", directory.display()))?;
        operations.open.record(start.elapsed());

        let mut read_elapsed = Duration::default();
        let mut walk = Walk::Continue;
        loop {
            let start = Instant::now();
            let entry = entries.next();
            read_elapsed += start.elapsed();
            let Some(entry) = entry else {
                break;
            };
            let entry = entry
                .with_context(|| format!("failed to read directory {}", directory.display()))?;
            let file_type = entry.file_type().with_context(|| {
                format!("failed to read file type for {}", entry.path().display())
            })?;
            if file_type.is_dir() {
                pending.push(entry.path());
                continue;
            }
            walk = visit(entry, file_type)?;
            if matches!(walk, Walk::Stop) {
                break;
            }
        }
        operations.read.record(read_elapsed);

        let start = Instant::now();
        drop(entries);
        operations.close.record(start.elapsed());
        visited.push(directory);
        if matches!(walk, Walk::Stop) {
            break;
        }
    }

    Ok((visited, operations))
}

fn read_file(file: &FileSpec, buffer: &mut [u8], accumulator: &mut FileAccumulator) -> Result<()> {
    let start = Instant::now();
    let mut input = File::open(&file.path)
        .with_context(|| format!("failed to open {}", file.path.display()))?;
    let open_elapsed = start.elapsed();

    let mut bytes_read = 0;
    let mut file_io = OperationAccumulator::default();
    loop {
        let start = Instant::now();
        let bytes = input.read(buffer);
        file_io.record(start.elapsed());
        let bytes = bytes?;
        if bytes == 0 {
            break;
        }
        bytes_read += bytes;
    }
    if bytes_read != file.size {
        return Err(anyhow!(
            "read {} bytes from {}, expected {}",
            bytes_read,
            file.path.display(),
            file.size
        ));
    }

    let start = Instant::now();
    drop(input);
    let close_elapsed = start.elapsed();
    record_file(
        accumulator,
        file,
        bytes_read as u64,
        FileTiming {
            open: open_elapsed,
            io: file_io,
            close: close_elapsed,
        },
    );
    Ok(())
}

fn record_file(accumulator: &mut FileAccumulator, file: &FileSpec, bytes: u64, timing: FileTiming) {
    accumulator.files += 1;
    accumulator.bytes += bytes;
    accumulator.file_open.record(timing.open);
    accumulator.file_io.merge(&timing.io);
    accumulator.file_close.record(timing.close);

    let size = &mut accumulator.per_file_size[file.size_index];
    size.files += 1;
    size.bytes += bytes;
    size.file_total
        .record(timing.open + timing.io.elapsed + timing.close);
    size.file_open.record(timing.open);
    size.file_io.merge(&timing.io);
    size.file_close.record(timing.close);
}

fn run_file_workers<F>(
    workload: &Workload,
    jobs: NonZeroUsize,
    operation: F,
    progress: &ProgressBar,
) -> Result<FileAccumulator>
where
    F: Fn(&FileSpec, &mut FileAccumulator) -> Result<()> + Sync,
{
    let file_sizes = &workload.file_sizes;
    run_batched_workers(
        &workload.files,
        jobs,
        || FileAccumulator::new(file_sizes),
        |batch, accumulator| {
            for file in batch {
                operation(file, accumulator)?;
            }
            progress.inc(batch.len() as u64);
            Ok(())
        },
        |workers| merge_file_accumulators(workers, file_sizes),
    )
}

/// The files one read pass covers and the order it reads them in.
struct ReadPlan<'a> {
    files: &'a [FileSpec],
    file_sizes: &'a [usize],
    order: &'a [usize],
}

fn run_read_workers(
    plan: ReadPlan<'_>,
    read_size: NonZeroUsize,
    jobs: NonZeroUsize,
    progress: &ProgressBar,
) -> Result<FileAccumulator> {
    let ReadPlan {
        files,
        file_sizes,
        order,
    } = plan;
    run_batched_workers(
        order,
        jobs,
        || (FileAccumulator::new(file_sizes), vec![0; read_size.get()]),
        |batch, (accumulator, buffer)| {
            for index in batch {
                read_file(&files[*index], buffer, accumulator)?;
            }
            progress.inc(batch.len() as u64);
            Ok(())
        },
        |workers| {
            merge_file_accumulators(
                workers
                    .into_iter()
                    .map(|(accumulator, _)| accumulator)
                    .collect(),
                file_sizes,
            )
        },
    )
}

fn merge_file_accumulators(
    workers: Vec<FileAccumulator>,
    file_sizes: &[usize],
) -> Result<FileAccumulator> {
    let mut result = FileAccumulator::new(file_sizes);
    for worker in workers {
        result.merge(worker);
    }
    Ok(result)
}

/// Hand `items` to up to `jobs` scoped threads in batches of `WORK_BATCH`
/// taken from a shared cursor, so a slow stretch of files does not leave
/// one thread working alone at the end. Each thread folds its batches into
/// a state from `new_state`; `merge` combines the threads' states.
fn run_batched_workers<T, S, R, N, P, M>(
    items: &[T],
    jobs: NonZeroUsize,
    new_state: N,
    process_batch: P,
    merge: M,
) -> Result<R>
where
    T: Sync,
    S: Send,
    N: Fn() -> S + Sync,
    P: Fn(&[T], &mut S) -> Result<()> + Sync,
    M: FnOnce(Vec<S>) -> Result<R>,
{
    let cursor = AtomicUsize::new(0);
    let work = || -> Result<S> {
        let mut state = new_state();
        loop {
            let begin = cursor.fetch_add(WORK_BATCH, Ordering::Relaxed);
            if begin >= items.len() {
                return Ok(state);
            }
            let end = (begin + WORK_BATCH).min(items.len());
            process_batch(&items[begin..end], &mut state)?;
        }
    };

    let workers = jobs.get().min(items.len().div_ceil(WORK_BATCH)).max(1);
    if workers == 1 {
        return merge(vec![work()?]);
    }
    std::thread::scope(|scope| {
        let handles = (0..workers).map(|_| scope.spawn(work)).collect::<Vec<_>>();
        let mut states = Vec::with_capacity(handles.len());
        for handle in handles {
            states.push(
                handle
                    .join()
                    .map_err(|_| anyhow!("filesystem benchmark worker panicked"))??,
            );
        }
        merge(states)
    })
}

fn nanoseconds_per_file(seconds: f64, files: u64) -> f64 {
    if files == 0 {
        0.0
    } else {
        seconds * 1_000_000_000.0 / files as f64
    }
}

fn phase_result(
    wall_seconds: f64,
    directories: u64,
    directory_operations: DirectoryOperations,
    files: FileAccumulator,
) -> PhaseResult {
    let syscall_seconds = (directory_operations.create.elapsed
        + directory_operations.open.elapsed
        + directory_operations.read.elapsed
        + directory_operations.close.elapsed
        + files.file_open.elapsed
        + files.file_io.elapsed
        + files.file_close.elapsed)
        .as_secs_f64();
    PhaseResult {
        wall_seconds,
        throughput_mib_per_second: files.bytes as f64
            / types::BYTES_IN_MEGABYTE as f64
            / wall_seconds,
        syscall_seconds,
        syscall_nanoseconds_per_file: nanoseconds_per_file(syscall_seconds, files.files),
        files: files.files,
        directories,
        bytes: files.bytes,
        directory_create: directory_operations.create.result(),
        directory_open: directory_operations.open.result(),
        directory_read: directory_operations.read.result(),
        directory_close: directory_operations.close.result(),
        file_open: files.file_open.result(),
        file_io: files.file_io.result(),
        file_close: files.file_close.result(),
        per_file_size: files
            .per_file_size
            .into_iter()
            .map(FileSizeAccumulator::result)
            .collect(),
        eden: None,
    }
}

#[cfg(target_os = "linux")]
fn drop_kernel_caches_on_linux(mode: KernelCacheDropMode) -> Result<()> {
    const DROP_CACHES: &str = "/proc/sys/vm/drop_caches";
    let value = format!("{}\n", mode.value());
    match fs::write(DROP_CACHES, value.as_bytes()) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(error) => return Err(error).context("failed to drop Linux kernel caches"),
    }

    let mut child = Command::new("sudo")
        .envs(trace_env("sudo"))
        .args(["-n", "tee", DROP_CACHES])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context("failed to run sudo to drop Linux kernel caches")?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("sudo did not provide a standard input pipe"))?
        .write_all(value.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        return Err(anyhow!(
            "sudo could not drop Linux kernel caches using mode {} (status {status})",
            mode.value()
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn drop_kernel_caches_on_linux(_mode: KernelCacheDropMode) -> Result<()> {
    Err(anyhow!(
        "dropping kernel caches is currently supported only on Linux"
    ))
}

fn format_byte_size(bytes: u64) -> String {
    let units = [
        (types::BYTES_IN_GIGABYTE as u64, "GiB"),
        (types::BYTES_IN_MEGABYTE as u64, "MiB"),
        (types::BYTES_IN_KILOBYTE as u64, "KiB"),
    ];
    for (unit_size, unit) in units {
        let value = bytes as f64 / unit_size as f64;
        // Anything that would print as 1023.5 or more in the unit below
        // rounds to 1 here instead.
        if value >= 0.9995 {
            if (value - value.round()).abs() < 0.05 {
                return format!("{value:.0} {unit}");
            }
            return format!("{value:.1} {unit}");
        }
    }
    format!("{bytes} B")
}

fn format_decimal_with_unit(value: f64, decimal_places: usize, unit: &str) -> String {
    let mut formatted = format!("{value:.decimal_places$}");
    if formatted.contains('.') {
        while formatted.ends_with('0') {
            formatted.pop();
        }
        if formatted.ends_with('.') {
            formatted.pop();
        }
    }
    formatted.push_str(unit);
    formatted
}

fn format_duration_seconds(seconds: f64) -> String {
    if seconds == 0.0 {
        return "0s".to_owned();
    }
    if seconds >= 1.0 {
        return format_decimal_with_unit(seconds, 3, "s");
    }
    if seconds >= 0.001 {
        return format_decimal_with_unit(seconds * 1_000.0, 2, "ms");
    }
    if seconds >= 0.000_001 {
        return format_decimal_with_unit(seconds * 1_000_000.0, 1, "us");
    }
    format_decimal_with_unit(seconds * 1_000_000_000.0, 0, "ns")
}

fn format_duration_nanoseconds(nanoseconds: f64) -> String {
    format_duration_seconds(nanoseconds / 1_000_000_000.0)
}

impl fmt::Display for FsIoResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "Filesystem I/O benchmark")?;
        let directory_layout = if self.config.file_list {
            format!(
                "from a file list ({} paths skipped), read only",
                self.config.skipped_files
            )
        } else if self.config.read_only {
            "existing files, read only".to_owned()
        } else {
            self.config.files_per_dir.map_or_else(
                || {
                    format!(
                        "Virtual Repo factor {} layout",
                        self.config.virtual_repo_factor
                    )
                },
                |files_per_dir| format!("{files_per_dir} files/directory"),
            )
        };
        writeln!(
            formatter,
            "{} files, {}, {}, {} job(s)",
            self.config.number_of_files,
            directory_layout,
            format_byte_size(self.config.total_bytes),
            self.config.jobs
        )?;
        writeln!(
            formatter,
            "I/O request caps: read {}, write {}; drop_kernel_caches={}",
            format_byte_size(self.config.read_size_bytes as u64),
            format_byte_size(self.config.write_size_bytes as u64),
            self.config.drop_kernel_caches
        )?;
        writeln!(
            formatter,
            "Sums add measured durations across jobs; latency values are per call or file."
        )?;
        writeln!(
            formatter,
            "Time in filesystem calls excludes the benchmark's own work, which wall time includes."
        )?;
        if let Some(write) = &self.write {
            write_phase(formatter, "Write", "file create", write)?;
        }
        writeln!(
            formatter,
            "\nRead-cache preparation: {} — {}",
            format_duration_seconds(self.cache_preparation.wall_seconds),
            self.cache_preparation.cache_drop_method
        )?;
        write_operation(formatter, "file sync", &self.cache_preparation.file_sync)?;
        write_phase(formatter, "Read", "file open", &self.read)?;
        if let Some(remove) = &self.remove {
            write_remove_phase(formatter, remove)?;
        }
        if let Some(diff) = &self.diff {
            comparison::write_diff(formatter, diff)?;
        }
        Ok(())
    }
}

fn write_remove_phase(formatter: &mut fmt::Formatter<'_>, result: &RemoveResult) -> fmt::Result {
    writeln!(
        formatter,
        "\nRemove: {} wall, {} in filesystem calls ({}/file), {:.0} files/s, {} files, {} directories",
        format_duration_seconds(result.wall_seconds),
        format_duration_seconds(result.syscall_seconds),
        format_duration_nanoseconds(result.syscall_nanoseconds_per_file),
        result.files_per_second,
        result.files,
        result.directories
    )?;
    writeln!(
        formatter,
        "{:<20} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "operation", "calls", "sum", "avg", "p50", "p90", "p99"
    )?;
    write_operation(formatter, "file unlink", &result.file_unlink)?;
    write_operation(formatter, "directory remove", &result.directory_remove)?;
    if let Some(eden) = &result.eden {
        daemon::write_daemon(formatter, eden)?;
    }
    Ok(())
}

fn write_phase(
    formatter: &mut fmt::Formatter<'_>,
    name: &str,
    file_open_label: &str,
    result: &PhaseResult,
) -> fmt::Result {
    writeln!(
        formatter,
        "\n{name}: {} wall, {} in filesystem calls ({}/file), {:.2} MiB/s, {} files, {} directories",
        format_duration_seconds(result.wall_seconds),
        format_duration_seconds(result.syscall_seconds),
        format_duration_nanoseconds(result.syscall_nanoseconds_per_file),
        result.throughput_mib_per_second,
        result.files,
        result.directories
    )?;
    writeln!(
        formatter,
        "{:<20} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "operation", "calls", "sum", "avg", "p50", "p90", "p99"
    )?;
    write_operation(formatter, "directory create", &result.directory_create)?;
    write_operation(formatter, "directory open", &result.directory_open)?;
    write_operation(formatter, "directory read", &result.directory_read)?;
    write_operation(formatter, "directory close", &result.directory_close)?;
    write_operation(formatter, file_open_label, &result.file_open)?;
    write_operation(formatter, "file I/O", &result.file_io)?;
    write_operation(formatter, "file close", &result.file_close)?;

    writeln!(
        formatter,
        "\n{:>10} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "size", "files", "sum", "avg", "p50", "p90", "p99", "open sum", "I/O sum", "close sum"
    )?;
    for size in &result.per_file_size {
        writeln!(
            formatter,
            "{:>10} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
            format_byte_size(size.size_bytes as u64),
            size.files,
            format_duration_seconds(size.sum_seconds),
            format_duration_nanoseconds(size.average_nanoseconds_per_file),
            format_duration_nanoseconds(size.p50_nanoseconds_per_file as f64),
            format_duration_nanoseconds(size.p90_nanoseconds_per_file as f64),
            format_duration_nanoseconds(size.p99_nanoseconds_per_file as f64),
            format_duration_seconds(size.file_open.sum_seconds),
            format_duration_seconds(size.file_io.sum_seconds),
            format_duration_seconds(size.file_close.sum_seconds),
        )?;
    }
    if let Some(eden) = &result.eden {
        writeln!(formatter)?;
        daemon::write_daemon(formatter, eden)?;
    }
    Ok(())
}

fn write_operation(
    formatter: &mut fmt::Formatter<'_>,
    name: &str,
    result: &OperationResult,
) -> fmt::Result {
    if result.calls > 0 {
        writeln!(
            formatter,
            "{name:<20} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12}",
            result.calls,
            format_duration_seconds(result.sum_seconds),
            format_duration_nanoseconds(result.average_nanoseconds),
            format_duration_nanoseconds(result.p50_nanoseconds as f64),
            format_duration_nanoseconds(result.p90_nanoseconds as f64),
            format_duration_nanoseconds(result.p99_nanoseconds as f64),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn formats_byte_sizes_with_readable_units() {
        assert_eq!(format_byte_size(256), "256 B");
        assert_eq!(format_byte_size(1_536), "1.5 KiB");
        assert_eq!(format_byte_size(56 * 1024), "56 KiB");
        assert_eq!(format_byte_size(5 * 1024 * 1024), "5 MiB");
    }

    #[test]
    fn formats_durations_with_adaptive_units() {
        assert_eq!(format_duration_seconds(12.245), "12.245s");
        assert_eq!(format_duration_seconds(0.003), "3ms");
        assert_eq!(format_duration_seconds(0.000_250_4), "250.4us");
        assert_eq!(format_duration_seconds(0.000_000_025), "25ns");
    }

    #[test]
    fn operation_accumulator_reports_latency_percentiles() {
        let mut accumulator = OperationAccumulator::default();
        for latency in [10, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
            accumulator.record(Duration::from_nanos(latency));
        }

        let result = accumulator.result();
        assert_eq!(result.calls, 10);
        assert_eq!(result.average_nanoseconds, 55.0);
        assert_eq!(result.p50_nanoseconds, 50);
        assert_eq!(result.p90_nanoseconds, 90);
        assert_eq!(result.p99_nanoseconds, 100);
    }

    #[test]
    fn batched_workers_process_every_item_once() {
        let items = (0..1_000_u64).collect::<Vec<_>>();

        let (sum, batches) = run_batched_workers(
            &items,
            NonZeroUsize::new(4).expect("test job count is nonzero"),
            || (0_u64, 0_u64),
            |batch, (sum, batches)| {
                assert!(batch.len() <= WORK_BATCH);
                *sum += batch.iter().sum::<u64>();
                *batches += 1;
                Ok(())
            },
            |workers| {
                Ok(workers
                    .iter()
                    .fold((0, 0), |(sum, batches), (worker_sum, worker_batches)| {
                        (sum + worker_sum, batches + worker_batches)
                    }))
            },
        )
        .expect("workers should succeed");

        assert_eq!(
            sum,
            999 * 1_000 / 2,
            "every item is handed out exactly once"
        );
        assert_eq!(batches, 16, "1,000 items make 16 batches of up to 64");
    }

    #[test]
    fn kernel_cache_drop_mode_accepts_zero_through_three() {
        for mode in 0..=3 {
            assert_eq!(
                parse_kernel_cache_drop_mode(&mode.to_string())
                    .expect("supported cache-drop mode should parse")
                    .value(),
                mode
            );
        }
        assert!(
            parse_kernel_cache_drop_mode("4").is_err(),
            "cache-drop modes above three should fail"
        );
    }

    #[test]
    fn kernel_cache_drop_defaults_depend_on_platform_support() {
        let default = KernelCacheDropMode::PageCache;
        assert_eq!(
            resolve_kernel_cache_drop_mode_for_platform(None, default, true)
                .expect("Linux default should resolve"),
            KernelCacheDropMode::PageCache
        );
        assert_eq!(
            resolve_kernel_cache_drop_mode_for_platform(
                Some(KernelCacheDropMode::PageCacheDentriesInodes),
                default,
                true
            )
            .expect("explicit Linux mode should resolve"),
            KernelCacheDropMode::PageCacheDentriesInodes
        );
        assert_eq!(
            resolve_kernel_cache_drop_mode_for_platform(None, default, false)
                .expect("unsupported platforms should disable cache dropping by default"),
            KernelCacheDropMode::Disabled
        );
        assert_eq!(
            resolve_kernel_cache_drop_mode_for_platform(
                Some(KernelCacheDropMode::Disabled),
                default,
                false
            )
            .expect("explicitly disabled cache dropping should be portable"),
            KernelCacheDropMode::Disabled
        );
        assert!(
            resolve_kernel_cache_drop_mode_for_platform(
                Some(KernelCacheDropMode::PageCache),
                default,
                false
            )
            .is_err(),
            "enabled cache dropping should fail on unsupported platforms"
        );
    }

    #[test]
    fn workload_uses_requested_leaf_directory_occupancy() {
        let parent = TempDir::new().expect("temporary directory should be created");
        let test_dir = TestDir::validate(
            parent
                .path()
                .to_str()
                .expect("temporary path should be UTF-8"),
        )
        .expect("benchmark directory should be created");
        let options = FsIoOptions {
            number_of_files: Some(NonZeroUsize::new(5).expect("test count is nonzero")),
            files_per_dir: Some(NonZeroUsize::new(2).expect("test occupancy is nonzero")),
            file_size: Some(10),
            read_size: NonZeroUsize::new(4).expect("test read size is nonzero"),
            write_size: NonZeroUsize::new(4).expect("test write size is nonzero"),
            jobs: NonZeroUsize::new(1).expect("test job count is nonzero"),
            drop_kernel_caches: KernelCacheDropMode::Disabled,
            show_progress: false,
        };

        let workload = Workload::new(&test_dir, options).expect("workload should be generated");

        assert_eq!(workload.files.len(), 5, "all requested files should exist");
        assert_eq!(
            workload.directories.len(),
            6,
            "three leaf directories are needed"
        );
        assert_eq!(
            workload.total_bytes, 50,
            "uniform file sizes should be used"
        );
        assert_eq!(
            workload.files[0].path.parent(),
            workload.files[1].path.parent(),
            "the first leaf should contain two files"
        );
        assert_ne!(
            workload.files[1].path.parent(),
            workload.files[2].path.parent(),
            "the next file should start a new leaf"
        );
    }

    #[test]
    fn workload_uses_virtual_repo_layout_and_sizes_by_default() {
        let parent = TempDir::new().expect("temporary directory should be created");
        let test_dir = TestDir::validate(
            parent
                .path()
                .to_str()
                .expect("temporary path should be UTF-8"),
        )
        .expect("benchmark directory should be created");
        let options = FsIoOptions {
            number_of_files: Some(NonZeroUsize::new(1_000).expect("test count is nonzero")),
            files_per_dir: None,
            file_size: None,
            read_size: NonZeroUsize::new(4).expect("test read size is nonzero"),
            write_size: NonZeroUsize::new(4).expect("test write size is nonzero"),
            jobs: NonZeroUsize::new(1).expect("test job count is nonzero"),
            drop_kernel_caches: KernelCacheDropMode::Disabled,
            show_progress: false,
        };

        let workload = Workload::new(&test_dir, options).expect("workload should be generated");

        assert_eq!(workload.files.len(), 1_000);
        assert!(workload.directories.len() > 1);
        assert!(workload.file_sizes.len() > 1);
        assert!(workload.file_sizes.len() <= FILE_SIZE_STEP_UPPER_BOUNDS.len() + 1);
        assert!(workload.virtual_repo_factor <= virtual_repo::MAX_FACTOR_BITS as u8);
        for (ordinal, file) in workload.files.iter().enumerate() {
            assert_eq!(
                file.path.file_name().and_then(|name| name.to_str()),
                Some(benchmark_file_name(ordinal).as_str())
            );
        }
    }

    #[test]
    fn virtual_repo_layout_can_be_written_and_enumerated() {
        let parent = TempDir::new().expect("temporary directory should be created");
        let test_dir = TestDir::validate(
            parent
                .path()
                .to_str()
                .expect("temporary path should be UTF-8"),
        )
        .expect("benchmark directory should be created");
        let options = FsIoOptions {
            number_of_files: Some(NonZeroUsize::new(20).expect("test count is nonzero")),
            files_per_dir: None,
            file_size: Some(10),
            read_size: NonZeroUsize::new(4).expect("test read size is nonzero"),
            write_size: NonZeroUsize::new(4).expect("test write size is nonzero"),
            jobs: NonZeroUsize::new(1).expect("test job count is nonzero"),
            drop_kernel_caches: KernelCacheDropMode::Disabled,
            show_progress: false,
        };
        let workload = Workload::new(&test_dir, options).expect("workload should be generated");
        let progress = ProgressBar::hidden();

        let write = bench_write(&workload, b"natural text buffer", options, &progress)
            .expect("write benchmark should succeed");
        let read = bench_read(
            FsIoSource::Generated(&test_dir),
            &workload,
            options,
            &progress,
        )
        .expect("read benchmark should succeed");

        assert_eq!(write.files, 20);
        assert_eq!(read.files, 20);
    }

    #[test]
    fn file_size_steps_use_requested_boundaries_and_omit_empty_buckets() {
        let sizes = [
            0,
            511,
            512,
            256 * 1024,
            512 * 1024,
            1024 * 1024,
            5 * 1024 * 1024,
            20 * 1024 * 1024,
            20 * 1024 * 1024 + 1,
        ];

        let (file_sizes, size_indexes) =
            file_size_steps(&sizes, None).expect("size steps should be generated");

        assert_eq!(
            file_sizes,
            vec![
                256,
                512,
                256 * 1024,
                512 * 1024,
                1024 * 1024,
                25 * 1024 * 1024 / 2,
                20 * 1024 * 1024 + 1,
            ],
            "only occupied buckets should produce output sizes"
        );
        assert_eq!(size_indexes, vec![0, 0, 1, 2, 3, 4, 5, 5, 6]);
    }

    #[tokio::test]
    async fn read_only_run_reads_existing_files_and_skips_write_and_remove() {
        let root = TempDir::new().expect("temporary directory should be created");
        fs::create_dir(root.path().join("nested")).expect("nested directory should be created");
        fs::write(root.path().join("a.txt"), b"12345").expect("file should be written");
        fs::write(root.path().join("nested/b.txt"), vec![0_u8; 3000])
            .expect("file should be written");
        fs::write(root.path().join("nested/c.txt"), b"").expect("file should be written");
        let test_dir = TestDir::existing(
            root.path()
                .to_str()
                .expect("temporary path should be UTF-8"),
        )
        .expect("an existing directory should be accepted");
        let options = FsIoOptions {
            number_of_files: Some(NonZeroUsize::new(16).expect("test cap is nonzero")),
            files_per_dir: None,
            file_size: None,
            read_size: NonZeroUsize::new(1024).expect("test read size is nonzero"),
            write_size: NonZeroUsize::new(1024).expect("test write size is nonzero"),
            jobs: NonZeroUsize::new(2).expect("test job count is nonzero"),
            drop_kernel_caches: KernelCacheDropMode::Disabled,
            show_progress: false,
        };

        let result = bench_fs_io(FsIoSource::Tree(&test_dir), options, None, None)
            .await
            .expect("read-only run should succeed");

        assert!(result.write.is_none());
        assert!(result.remove.is_none());
        assert!(result.config.read_only);
        assert_eq!(result.config.number_of_files, 3);
        assert_eq!(result.read.files, 3);
        assert_eq!(result.read.bytes, 3005);
        assert_eq!(result.read.directories, 2);
        assert_eq!(
            result.read.file_io.calls, 7,
            "every file reads until a zero-length read: 5 bytes take 2 calls, 3000 bytes take 4, an empty file takes 1"
        );
        assert_eq!(
            result.read.per_file_size.len(),
            2,
            "0 and 5 bytes share the smallest bucket; 3000 bytes falls in the next"
        );
        assert!(
            root.path().join("nested/b.txt").exists(),
            "a read-only run must leave the tree alone"
        );

        let capped = bench_fs_io(
            FsIoSource::Tree(&test_dir),
            FsIoOptions {
                number_of_files: Some(NonZeroUsize::new(2).expect("test cap is nonzero")),
                ..options
            },
            None,
            None,
        )
        .await
        .expect("capped read-only run should succeed");

        assert_eq!(capped.read.files, 2);
        assert_eq!(capped.config.number_of_files, 2);
    }

    #[tokio::test]
    async fn file_list_run_reads_listed_files_in_order_and_skips_missing_paths() {
        let root = TempDir::new().expect("temporary directory should be created");
        fs::create_dir(root.path().join("nested")).expect("nested directory should be created");
        fs::write(root.path().join("a.txt"), b"12345").expect("file should be written");
        fs::write(root.path().join("nested/b.txt"), vec![0_u8; 3000])
            .expect("file should be written");
        let list = root.path().join("files.txt");
        fs::write(
            &list,
            format!(
                "{}\n{}\n\n{}\n{}\n",
                root.path().join("nested/b.txt").display(),
                root.path().join("a.txt").display(),
                root.path().join("missing.txt").display(),
                root.path().join("nested").display(),
            ),
        )
        .expect("file list should be written");
        let options = FsIoOptions {
            number_of_files: None,
            files_per_dir: None,
            file_size: None,
            read_size: NonZeroUsize::new(1024).expect("test read size is nonzero"),
            write_size: NonZeroUsize::new(1024).expect("test write size is nonzero"),
            jobs: NonZeroUsize::new(1).expect("test job count is nonzero"),
            drop_kernel_caches: KernelCacheDropMode::Disabled,
            show_progress: false,
        };

        let result = bench_fs_io(FsIoSource::FileList(&list), options, None, None)
            .await
            .expect("file-list run should succeed");

        assert!(result.config.read_only);
        assert!(result.config.file_list);
        assert_eq!(
            result.config.skipped_files, 2,
            "the missing path and the directory are skipped"
        );
        assert_eq!(result.config.number_of_files, 2);
        assert_eq!(result.read.files, 2);
        assert_eq!(result.read.bytes, 3005);
        assert_eq!(result.read.directories, 0);
        assert_eq!(
            result.read.directory_open.calls, 0,
            "a file list is read without listing directories"
        );
        assert_eq!(
            result.read.file_io.calls, 6,
            "3000 bytes take 4 calls in list order, then 5 bytes take 2"
        );

        let capped = bench_fs_io(
            FsIoSource::FileList(&list),
            FsIoOptions {
                number_of_files: Some(NonZeroUsize::new(1).expect("test cap is nonzero")),
                ..options
            },
            None,
            None,
        )
        .await
        .expect("capped file-list run should succeed");

        assert_eq!(capped.read.files, 1);
        assert_eq!(
            capped.read.bytes, 3000,
            "the cap keeps the first listed file"
        );
    }

    #[test]
    fn workload_reports_file_and_directory_operations() {
        let parent = TempDir::new().expect("temporary directory should be created");
        let test_dir = TestDir::validate(
            parent
                .path()
                .to_str()
                .expect("temporary path should be UTF-8"),
        )
        .expect("benchmark directory should be created");
        let options = FsIoOptions {
            number_of_files: Some(NonZeroUsize::new(5).expect("test count is nonzero")),
            files_per_dir: Some(NonZeroUsize::new(2).expect("test occupancy is nonzero")),
            file_size: Some(10),
            read_size: NonZeroUsize::new(4).expect("test read size is nonzero"),
            write_size: NonZeroUsize::new(4).expect("test write size is nonzero"),
            jobs: NonZeroUsize::new(2).expect("test job count is nonzero"),
            drop_kernel_caches: KernelCacheDropMode::Disabled,
            show_progress: false,
        };
        let workload = Workload::new(&test_dir, options).expect("workload should be generated");

        let progress = ProgressBar::hidden();
        let write = bench_write(&workload, b"natural text buffer", options, &progress)
            .expect("write benchmark should succeed");
        let read = bench_read(
            FsIoSource::Generated(&test_dir),
            &workload,
            options,
            &progress,
        )
        .expect("read benchmark should succeed");

        assert_eq!(
            write.directory_create.calls, 6,
            "the data root, two intermediate directories, and three leaves are created"
        );
        assert_eq!(write.file_open.calls, 5, "each file is created once");
        assert_eq!(
            write.file_io.calls, 15,
            "each 10-byte file requires three 4-byte writes"
        );
        assert_eq!(write.file_close.calls, 5, "each file is closed once");
        assert_eq!(
            read.directories, 6,
            "the read phase reports the workload's directory count like the write phase"
        );
        assert_eq!(read.file_open.calls, 5, "each file is opened once");
        assert_eq!(
            read.file_io.calls, 20,
            "each file requires three data reads and one EOF read"
        );
        assert_eq!(read.file_close.calls, 5, "each file is closed once");
    }
}
