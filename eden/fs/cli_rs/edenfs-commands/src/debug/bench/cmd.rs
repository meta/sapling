/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Command-line interface for benchmarking

use std::num::NonZeroUsize;
use std::path::PathBuf;

use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use clap::Parser;
use serde_json;

use super::dbio;
use super::facebook;
use super::fsio;
use super::r#gen;
use super::traversal;
use super::types;
use crate::ExitCode;

#[derive(Parser, Debug)]
#[clap(
    about = "Run benchmarks for EdenFS and OS-native file systems on Linux, macOS, and Windows",
    long_about = "Benchmark filesystem operations including traversal, I/O, and database performance"
)]
pub enum BenchCmd {
    #[clap(about = "Run a realistic filesystem I/O benchmark")]
    FsIo {
        /// Directory to use for testing
        #[clap(long, default_value_t = std::env::temp_dir().to_str().unwrap().to_string())]
        test_dir: String,

        /// Number of files to use for benchmarking. Defaults to 65,536 for
        /// generated files and for --read-only; with --file-list the whole
        /// list is read unless this caps it.
        #[clap(long)]
        number_of_files: Option<NonZeroUsize>,

        /// Override the Virtual Repo directory layout with fixed-size leaf directories
        #[clap(long)]
        files_per_dir: Option<NonZeroUsize>,

        /// Override the Virtual Repo file-size distribution with one size
        #[clap(long, value_parser = types::parse_byte_size)]
        file_size: Option<usize>,

        /// Maximum size of each read call
        #[clap(long, value_parser = types::parse_nonzero_byte_size, default_value_t = NonZeroUsize::new(types::DEFAULT_IO_SIZE).expect("default read size is nonzero"))]
        read_size: NonZeroUsize,

        /// Maximum size of each write call
        #[clap(long, value_parser = types::parse_nonzero_byte_size, default_value_t = NonZeroUsize::new(types::DEFAULT_IO_SIZE).expect("default write size is nonzero"))]
        write_size: NonZeroUsize,

        /// Number of worker threads for file reads and writes
        #[clap(long, default_value_t = NonZeroUsize::new(1).expect("default job count is nonzero"))]
        jobs: NonZeroUsize,

        /// Kernel caches to drop before reading: 0=none, 1=pages, 2=dentries/inodes, 3=both.
        /// Defaults to 1 on Linux; unsupported on other platforms.
        #[clap(
            long,
            value_parser = fsio::parse_kernel_cache_drop_mode,
            num_args = 0..=1,
            default_missing_value = "1"
        )]
        drop_kernel_caches: Option<fsio::KernelCacheDropMode>,

        /// Output results as JSON
        #[clap(long)]
        json: bool,

        /// Compare against JSON output from a previous fs-io run
        #[clap(long)]
        diff: Option<PathBuf>,

        /// Disable progress bars
        #[clap(long)]
        no_progress: bool,

        /// Only read the files already under --test-dir: skip the write and
        /// remove phases and leave the directory untouched. Kernel caches
        /// default to mode 3 so lookups reach the filesystem.
        #[clap(long)]
        read_only: bool,

        /// Read the paths listed one per line in this file, in list order,
        /// instead of a directory tree. Implies --read-only; --test-dir is
        /// not used. Paths that are not regular files are skipped.
        #[clap(long)]
        file_list: Option<PathBuf>,
    },

    #[clap(about = "Run database I/O benchmarks")]
    DbIo {
        /// Directory to use for testing
        #[clap(long, default_value_t = std::env::temp_dir().to_str().unwrap().to_string())]
        test_dir: String,

        /// Number of randomly generated files to use for benchmarking
        #[clap(long, default_value_t = types::DEFAULT_NUMBER_OF_FILES)]
        number_of_files: usize,

        /// Size of each chunk in bytes
        #[clap(long, default_value_t = types::DEFAULT_CHUNK_SIZE)]
        chunk_size: usize,
    },

    #[clap(
        about = "Run filesystem traversal benchmark",
        long_about = "Benchmark filesystem traversal performance including file reading, directory scanning, and I/O throughput. Supports multiple directories and various reading modes."
    )]
    Traversal {
        /// Directories to traverse (can be specified multiple times: --dir=/path1 --dir=/path2)
        #[clap(
            long,
            required = true,
            help = "Directory to traverse",
            long_help = "Directories to traverse. Can be specified multiple times to benchmark across multiple directory trees. Each directory will be traversed sequentially and results will be combined."
        )]
        dir: Vec<String>,

        /// Path to fbsource directory, required for thrift IO mode
        #[clap(
            long,
            help = "Use thrift I/O instead of filesystem calls",
            long_help = "Path to fbsource directory for thrift I/O mode. When specified, files will be read using EdenFS thrift calls instead of direct filesystem operations."
        )]
        thrift_io: Option<String>,

        /// Maximum number of files to process (default: unlimited)
        #[clap(
            long,
            help = "Limit number of files to process",
            long_help = "Maximum number of files to process during traversal. If not specified, all files in the directory tree will be processed."
        )]
        max_files: Option<usize>,

        /// Follow symbolic links during directory traversal
        #[clap(long, help = "Follow symbolic links")]
        follow_symlinks: bool,

        /// Disable progress bars and real-time updates
        #[clap(long, help = "Disable progress display")]
        no_progress: bool,

        /// Monitor CPU and memory usage during the benchmark
        #[clap(
            long,
            help = "Enable resource monitoring",
            long_help = "Enable CPU and memory usage monitoring during traversal. Shows additional metrics including memory consumption and CPU utilization."
        )]
        resource_usage: bool,

        /// Output results in JSON format for programmatic processing
        #[clap(long, help = "Output JSON format")]
        json: bool,

        /// Skip file reading and only measure directory traversal performance
        #[clap(
            long,
            help = "Skip file I/O, only traverse",
            long_help = "Skip the file reading benchmark and only measure directory traversal performance. Useful for testing pure filesystem traversal speed without I/O overhead."
        )]
        skip_read: bool,

        /// Show detailed read statistics including file size distribution and per-directory analysis
        #[clap(
            long,
            help = "Show detailed read performance statistics",
            long_help = "Enable detailed read statistics showing file size distribution, per-directory I/O performance breakdown, depth analysis, and file category overhead metrics. Only available when file reading is enabled (not compatible with --skip-read)."
        )]
        detailed_read_stats: bool,

        /// Show detailed directory listing statistics
        #[clap(
            long,
            help = "Show detailed directory listing statistics",
            long_help = "Enable detailed statistics about directory traversal including readdir() latency distribution, directory size analysis, scan rate variance, and slowest directories. Compatible with --skip-read for analyzing pure traversal performance."
        )]
        detailed_list_stats: bool,

        /// Include per-directory statistics (causes ~20% throughput reduction)
        #[clap(
            long,
            help = "Include per-directory stats (slower)",
            long_help = "Enable per-directory I/O statistics tracking. This feature causes significant overhead (~20% throughput reduction) due to CPU cache effects from HashMap operations. Only effective when used with --detailed-read-stats."
        )]
        include_dir_stats: bool,
    },
}

#[async_trait]
impl crate::Subcommand for BenchCmd {
    async fn run(&self) -> Result<ExitCode> {
        match self {
            Self::FsIo {
                test_dir,
                number_of_files,
                files_per_dir,
                file_size,
                read_size,
                write_size,
                jobs,
                drop_kernel_caches,
                json,
                diff,
                no_progress,
                read_only,
                file_list,
            } => {
                let read_only = *read_only || file_list.is_some();
                let drop_kernel_caches = fsio::resolve_kernel_cache_drop_mode(
                    *drop_kernel_caches,
                    if read_only {
                        fsio::KernelCacheDropMode::PageCacheDentriesInodes
                    } else {
                        fsio::KernelCacheDropMode::PageCache
                    },
                )?;
                let baseline = diff.as_deref().map(fsio::load_result).transpose()?;
                let bench_dir;
                let source = match file_list {
                    Some(list) => fsio::FsIoSource::FileList(list),
                    None if read_only => {
                        bench_dir = r#gen::TestDir::existing(test_dir)?;
                        fsio::FsIoSource::Tree(&bench_dir)
                    }
                    None => {
                        bench_dir = r#gen::TestDir::validate(test_dir)?;
                        fsio::FsIoSource::Generated(&bench_dir)
                    }
                };
                let result = fsio::bench_fs_io(
                    source,
                    fsio::FsIoOptions {
                        number_of_files: *number_of_files,
                        files_per_dir: *files_per_dir,
                        file_size: *file_size,
                        read_size: *read_size,
                        write_size: *write_size,
                        jobs: *jobs,
                        drop_kernel_caches,
                        show_progress: !*json && !*no_progress,
                    },
                    baseline.as_ref(),
                    Some(crate::get_edenfs_instance()),
                )
                .await;
                let cleanup_result = match source {
                    fsio::FsIoSource::Generated(bench_dir) => bench_dir.remove(),
                    fsio::FsIoSource::Tree(_) | fsio::FsIoSource::FileList(_) => Ok(()),
                };
                let mut result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        if let Err(cleanup_error) = cleanup_result {
                            return Err(error.context(format!(
                                "benchmark data cleanup also failed: {cleanup_error:#}"
                            )));
                        }
                        return Err(error);
                    }
                };
                // Print the measurements even when the baseline turns out
                // not to be comparable, so a long run is not thrown away.
                let diff_error = baseline
                    .as_ref()
                    .and_then(|baseline| result.add_diff(baseline).err());
                if *json {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    println!("{result}");
                }
                let diff_error = diff_error.map(|error| {
                    error.context("benchmark completed but could not be compared with the baseline")
                });
                match (diff_error, cleanup_result) {
                    (None, Ok(())) => {}
                    (Some(diff_error), Ok(())) => return Err(diff_error),
                    (None, Err(cleanup_error)) => {
                        return Err(cleanup_error
                            .context("benchmark completed but benchmark data cleanup failed"));
                    }
                    (Some(diff_error), Err(cleanup_error)) => {
                        return Err(anyhow!(
                            "{diff_error:#}; benchmark data cleanup also failed: {cleanup_error:#}"
                        ));
                    }
                }
            }
            Self::DbIo {
                test_dir,
                number_of_files,
                chunk_size,
            } => match r#gen::TestDir::validate(test_dir) {
                Ok(test_dir) => {
                    test_dir.prepare_hash_directories()?;
                    let random_data = r#gen::RandomData::new(*number_of_files, *chunk_size);
                    println!(
                        "The random data generated with {} chunks with {:.0} KiB each, with the total size of {:.2} GiB.",
                        random_data.number_of_files,
                        random_data.chunk_size as f64 / types::BYTES_IN_KILOBYTE as f64,
                        random_data.total_size() as f64 / types::BYTES_IN_GIGABYTE as f64
                    );
                    facebook::run_lmdb_benchmarks(&test_dir, &random_data)?;
                    println!(
                        "{}",
                        dbio::bench_sqlite_write_mfmd(&test_dir, &random_data)?
                    );
                    println!("{}", dbio::bench_sqlite_read_mfmd(&test_dir, &random_data)?);
                    test_dir.remove()?;
                }
                Err(e) => return Err(e),
            },
            Self::Traversal {
                dir,
                thrift_io,
                max_files,
                follow_symlinks,
                no_progress,
                resource_usage,
                json,
                skip_read,
                detailed_read_stats,
                detailed_list_stats,
                include_dir_stats,
            } => {
                // Validate flag compatibility
                if *skip_read && *detailed_read_stats {
                    return Err(anyhow::anyhow!(
                        "--skip-read and --detailed-read-stats are mutually exclusive.\n\
                        Detailed read statistics focus on file I/O performance metrics which are not \
                        collected when skipping file reads. Use --detailed-read-stats without --skip-read \
                        to see I/O performance analysis."
                    ));
                }

                if !*json {
                    if dir.len() == 1 {
                        println!(
                            "Running filesystem traversal benchmark on directory: {}",
                            dir[0]
                        );
                    } else {
                        println!(
                            "Running filesystem traversal benchmark on {} directories:",
                            dir.len()
                        );
                        for directory in dir {
                            println!("  - {directory}");
                        }
                    }
                }

                let effective_max_files = max_files.unwrap_or(usize::MAX);
                let benchmark_result = traversal::bench_traversal(
                    dir,
                    effective_max_files,
                    *follow_symlinks,
                    *no_progress,
                    *resource_usage,
                    *skip_read,
                    thrift_io.as_deref(),
                    *detailed_read_stats,
                    *detailed_list_stats,
                    *include_dir_stats,
                )
                .await?;

                if *json {
                    println!("{}", serde_json::to_string_pretty(&benchmark_result)?);
                } else {
                    println!("{benchmark_result}");
                }
            }
        }

        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_io_drop_kernel_caches_accepts_bare_flag() {
        let command = BenchCmd::try_parse_from(["bench", "fs-io", "--drop-kernel-caches"])
            .expect("bare cache-drop flag should use its default mode");

        let BenchCmd::FsIo {
            drop_kernel_caches, ..
        } = command
        else {
            panic!("expected fs-io command");
        };
        assert!(drop_kernel_caches.is_some());
    }

    #[test]
    fn fs_io_read_only_flag_parses() {
        let command =
            BenchCmd::try_parse_from(["bench", "fs-io", "--read-only", "--test-dir", "/tmp"])
                .expect("read-only flag should parse");

        let BenchCmd::FsIo {
            read_only,
            test_dir,
            ..
        } = command
        else {
            panic!("expected fs-io command");
        };
        assert!(read_only);
        assert_eq!(test_dir, "/tmp");
    }
}
