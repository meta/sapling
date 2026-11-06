/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Compare an fs-io run against a baseline recorded with `--json`.

use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde::Serialize;

use super::CachePreparationResult;
use super::FileSizeResult;
use super::FsIoResult;
use super::OperationResult;
use super::PhaseResult;
use super::RemoveResult;
use super::format_byte_size;

#[derive(Debug, Serialize)]
pub(super) struct FsIoDiff {
    // Absent for read-only runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    write: Option<PhaseDiff>,
    // Absent when kernel caches were not dropped: the preparation phase does
    // nothing then and its wall time is noise.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_preparation: Option<CachePreparationDiff>,
    read: PhaseDiff,
    // Absent when the baseline predates the remove phase.
    #[serde(skip_serializing_if = "Option::is_none")]
    remove: Option<RemoveDiff>,
}

#[derive(Debug, Serialize)]
struct RemoveDiff {
    wall_time_percent: Option<f64>,
    syscall_time_percent: Option<f64>,
    files_per_second_percent: Option<f64>,
    file_unlink: OperationDiff,
    directory_remove: OperationDiff,
}

#[derive(Debug, Serialize)]
struct CachePreparationDiff {
    wall_time_percent: Option<f64>,
    file_sync: OperationDiff,
}

#[derive(Debug, Serialize)]
struct PhaseDiff {
    wall_time_percent: Option<f64>,
    syscall_time_percent: Option<f64>,
    throughput_percent: Option<f64>,
    directory_create: OperationDiff,
    directory_open: OperationDiff,
    directory_read: OperationDiff,
    directory_close: OperationDiff,
    file_open: OperationDiff,
    file_io: OperationDiff,
    file_close: OperationDiff,
    per_file_size: Vec<FileSizeDiff>,
}

#[derive(Debug, Serialize)]
struct FileSizeDiff {
    size_bytes: usize,
    sum_time_percent: Option<f64>,
    p50_latency_percent: Option<f64>,
    p90_latency_percent: Option<f64>,
    p99_latency_percent: Option<f64>,
    open_sum_time_percent: Option<f64>,
    io_sum_time_percent: Option<f64>,
    close_sum_time_percent: Option<f64>,
}

#[derive(Debug, Serialize)]
struct OperationDiff {
    calls: u64,
    sum_time_percent: Option<f64>,
    p50_latency_percent: Option<f64>,
    p90_latency_percent: Option<f64>,
    p99_latency_percent: Option<f64>,
}

pub(crate) fn load_result(path: &Path) -> Result<FsIoResult> {
    let file = File::open(path)
        .with_context(|| format!("failed to open fs-io baseline {}", path.display()))?;
    serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("failed to parse fs-io baseline {}", path.display()))
}

impl FsIoResult {
    pub(crate) fn add_diff(&mut self, baseline: &Self) -> Result<()> {
        let diff = FsIoDiff::new(self, baseline)?;
        self.diff = Some(diff);
        Ok(())
    }
}

/// Reject a baseline whose workload configuration differs from the
/// current run's. Called before the benchmark runs, and again from
/// `FsIoDiff::new` as a backstop.
pub(super) fn ensure_config_matches(
    current: &super::FsIoConfig,
    baseline: &super::FsIoConfig,
) -> Result<()> {
    ensure!(
        current == baseline,
        "fs-io configuration does not match baseline\nbaseline: {baseline:?}\ncurrent: {current:?}"
    );
    Ok(())
}

impl FsIoDiff {
    fn new(current: &FsIoResult, baseline: &FsIoResult) -> Result<Self> {
        ensure_config_matches(&current.config, &baseline.config)?;
        validate_cache_preparation(&current.cache_preparation, &baseline.cache_preparation)?;
        let write = match (&current.write, &baseline.write) {
            (Some(current_write), Some(baseline_write)) => {
                validate_phase("write", current_write, baseline_write)?;
                Some(PhaseDiff::new(current_write, baseline_write))
            }
            // ensure_config_matches rejects a read-only run against a
            // read-write baseline before this point.
            _ => None,
        };
        validate_phase("read", &current.read, &baseline.read)?;

        let remove = match (&current.remove, &baseline.remove) {
            (Some(current_remove), Some(baseline_remove)) => {
                ensure!(
                    current_remove.files == baseline_remove.files
                        && current_remove.directories == baseline_remove.directories,
                    "remove phase counts do not match baseline: baseline {}/{}, current {}/{}",
                    baseline_remove.files,
                    baseline_remove.directories,
                    current_remove.files,
                    current_remove.directories
                );
                Some(RemoveDiff::new(current_remove, baseline_remove))
            }
            _ => None,
        };

        Ok(Self {
            write,
            cache_preparation: (current.cache_preparation.drop_kernel_caches != 0).then(|| {
                CachePreparationDiff::new(&current.cache_preparation, &baseline.cache_preparation)
            }),
            read: PhaseDiff::new(&current.read, &baseline.read),
            remove,
        })
    }
}

impl RemoveDiff {
    fn new(current: &RemoveResult, baseline: &RemoveResult) -> Self {
        Self {
            wall_time_percent: percent_change(current.wall_seconds, baseline.wall_seconds),
            syscall_time_percent: percent_change(current.syscall_seconds, baseline.syscall_seconds),
            files_per_second_percent: percent_change(
                current.files_per_second,
                baseline.files_per_second,
            ),
            file_unlink: OperationDiff::new(&current.file_unlink, &baseline.file_unlink),
            directory_remove: OperationDiff::new(
                &current.directory_remove,
                &baseline.directory_remove,
            ),
        }
    }
}

impl CachePreparationDiff {
    fn new(current: &CachePreparationResult, baseline: &CachePreparationResult) -> Self {
        Self {
            wall_time_percent: percent_change(current.wall_seconds, baseline.wall_seconds),
            file_sync: OperationDiff::new(&current.file_sync, &baseline.file_sync),
        }
    }
}

impl PhaseDiff {
    fn new(current: &PhaseResult, baseline: &PhaseResult) -> Self {
        Self {
            wall_time_percent: percent_change(current.wall_seconds, baseline.wall_seconds),
            syscall_time_percent: percent_change(current.syscall_seconds, baseline.syscall_seconds),
            throughput_percent: percent_change(
                current.throughput_mib_per_second,
                baseline.throughput_mib_per_second,
            ),
            directory_create: OperationDiff::new(
                &current.directory_create,
                &baseline.directory_create,
            ),
            directory_open: OperationDiff::new(&current.directory_open, &baseline.directory_open),
            directory_read: OperationDiff::new(&current.directory_read, &baseline.directory_read),
            directory_close: OperationDiff::new(
                &current.directory_close,
                &baseline.directory_close,
            ),
            file_open: OperationDiff::new(&current.file_open, &baseline.file_open),
            file_io: OperationDiff::new(&current.file_io, &baseline.file_io),
            file_close: OperationDiff::new(&current.file_close, &baseline.file_close),
            per_file_size: current
                .per_file_size
                .iter()
                .zip(&baseline.per_file_size)
                .map(|(current, baseline)| FileSizeDiff::new(current, baseline))
                .collect(),
        }
    }
}

impl FileSizeDiff {
    fn new(current: &FileSizeResult, baseline: &FileSizeResult) -> Self {
        Self {
            size_bytes: current.size_bytes,
            sum_time_percent: percent_change(current.sum_seconds, baseline.sum_seconds),
            p50_latency_percent: percent_change(
                current.p50_nanoseconds_per_file as f64,
                baseline.p50_nanoseconds_per_file as f64,
            ),
            p90_latency_percent: percent_change(
                current.p90_nanoseconds_per_file as f64,
                baseline.p90_nanoseconds_per_file as f64,
            ),
            p99_latency_percent: percent_change(
                current.p99_nanoseconds_per_file as f64,
                baseline.p99_nanoseconds_per_file as f64,
            ),
            open_sum_time_percent: percent_change(
                current.file_open.sum_seconds,
                baseline.file_open.sum_seconds,
            ),
            io_sum_time_percent: percent_change(
                current.file_io.sum_seconds,
                baseline.file_io.sum_seconds,
            ),
            close_sum_time_percent: percent_change(
                current.file_close.sum_seconds,
                baseline.file_close.sum_seconds,
            ),
        }
    }
}

impl OperationDiff {
    fn new(current: &OperationResult, baseline: &OperationResult) -> Self {
        Self {
            calls: current.calls,
            sum_time_percent: percent_change(current.sum_seconds, baseline.sum_seconds),
            p50_latency_percent: percent_change(
                current.p50_nanoseconds as f64,
                baseline.p50_nanoseconds as f64,
            ),
            p90_latency_percent: percent_change(
                current.p90_nanoseconds as f64,
                baseline.p90_nanoseconds as f64,
            ),
            p99_latency_percent: percent_change(
                current.p99_nanoseconds as f64,
                baseline.p99_nanoseconds as f64,
            ),
        }
    }
}

fn validate_cache_preparation(
    current: &CachePreparationResult,
    baseline: &CachePreparationResult,
) -> Result<()> {
    ensure!(
        current.drop_kernel_caches == baseline.drop_kernel_caches,
        "cache-drop mode does not match baseline: baseline {}, current {}",
        baseline.drop_kernel_caches,
        current.drop_kernel_caches
    );
    validate_operation(
        "cache preparation file sync",
        &current.file_sync,
        &baseline.file_sync,
    )
}

fn validate_phase(name: &str, current: &PhaseResult, baseline: &PhaseResult) -> Result<()> {
    ensure!(
        current.files == baseline.files,
        "{name} file count does not match baseline: baseline {}, current {}",
        baseline.files,
        current.files
    );
    ensure!(
        current.directories == baseline.directories,
        "{name} directory count does not match baseline: baseline {}, current {}",
        baseline.directories,
        current.directories
    );
    ensure!(
        current.bytes == baseline.bytes,
        "{name} byte count does not match baseline: baseline {}, current {}",
        baseline.bytes,
        current.bytes
    );

    for (operation_name, current_operation, baseline_operation) in [
        (
            "directory create",
            &current.directory_create,
            &baseline.directory_create,
        ),
        (
            "directory open",
            &current.directory_open,
            &baseline.directory_open,
        ),
        (
            "directory read",
            &current.directory_read,
            &baseline.directory_read,
        ),
        (
            "directory close",
            &current.directory_close,
            &baseline.directory_close,
        ),
        ("file open", &current.file_open, &baseline.file_open),
        ("file I/O", &current.file_io, &baseline.file_io),
        ("file close", &current.file_close, &baseline.file_close),
    ] {
        validate_operation(
            &format!("{name} {operation_name}"),
            current_operation,
            baseline_operation,
        )?;
    }

    ensure!(
        current.per_file_size.len() == baseline.per_file_size.len(),
        "{name} file-size row count does not match baseline: baseline {}, current {}",
        baseline.per_file_size.len(),
        current.per_file_size.len()
    );
    for (index, (current_size, baseline_size)) in current
        .per_file_size
        .iter()
        .zip(&baseline.per_file_size)
        .enumerate()
    {
        ensure!(
            current_size.size_bytes == baseline_size.size_bytes,
            "{name} file-size row {index} does not match baseline: baseline {}, current {}",
            baseline_size.size_bytes,
            current_size.size_bytes
        );
        ensure!(
            current_size.files == baseline_size.files,
            "{name} file count for size {} does not match baseline: baseline {}, current {}",
            current_size.size_bytes,
            baseline_size.files,
            current_size.files
        );
        ensure!(
            current_size.bytes == baseline_size.bytes,
            "{name} byte count for size {} does not match baseline: baseline {}, current {}",
            current_size.size_bytes,
            baseline_size.bytes,
            current_size.bytes
        );
        validate_operation(
            &format!("{name} open for size {}", current_size.size_bytes),
            &current_size.file_open,
            &baseline_size.file_open,
        )?;
        validate_operation(
            &format!("{name} I/O for size {}", current_size.size_bytes),
            &current_size.file_io,
            &baseline_size.file_io,
        )?;
        validate_operation(
            &format!("{name} close for size {}", current_size.size_bytes),
            &current_size.file_close,
            &baseline_size.file_close,
        )?;
    }
    Ok(())
}

fn validate_operation(
    name: &str,
    current: &OperationResult,
    baseline: &OperationResult,
) -> Result<()> {
    ensure!(
        current.calls == baseline.calls,
        "{name} call count does not match baseline: baseline {}, current {}",
        baseline.calls,
        current.calls
    );
    Ok(())
}

fn percent_change(current: f64, baseline: f64) -> Option<f64> {
    if baseline == 0.0 {
        return (current == 0.0).then_some(0.0);
    }
    Some((current - baseline) * 100.0 / baseline)
}

pub(super) fn write_diff(formatter: &mut fmt::Formatter<'_>, diff: &FsIoDiff) -> fmt::Result {
    writeln!(formatter, "\nChanges versus baseline")?;
    writeln!(
        formatter,
        "Lower times are faster; higher throughput is faster."
    )?;
    if let Some(write) = &diff.write {
        write_phase_diff(formatter, "Write", "file create", write)?;
    }
    if let Some(cache_preparation) = &diff.cache_preparation {
        write_cache_preparation_diff(formatter, cache_preparation)?;
    }
    write_phase_diff(formatter, "Read", "file open", &diff.read)?;
    if let Some(remove) = &diff.remove {
        write_remove_diff(formatter, remove)?;
    }
    Ok(())
}

fn write_remove_diff(formatter: &mut fmt::Formatter<'_>, diff: &RemoveDiff) -> fmt::Result {
    writeln!(
        formatter,
        "\nRemove: wall {}, filesystem calls {}, files/s {}",
        format_change(diff.wall_time_percent, false),
        format_change(diff.syscall_time_percent, false),
        format_change(diff.files_per_second_percent, true)
    )?;
    write_operation_diff_header(formatter)?;
    write_operation_diff(formatter, "file unlink", &diff.file_unlink)?;
    write_operation_diff(formatter, "directory remove", &diff.directory_remove)
}

fn write_cache_preparation_diff(
    formatter: &mut fmt::Formatter<'_>,
    diff: &CachePreparationDiff,
) -> fmt::Result {
    writeln!(
        formatter,
        "\nRead-cache preparation: {}",
        format_change(diff.wall_time_percent, false)
    )?;
    if diff.file_sync.calls > 0 {
        write_operation_diff_header(formatter)?;
        write_operation_diff(formatter, "file sync", &diff.file_sync)?;
    }
    Ok(())
}

fn write_phase_diff(
    formatter: &mut fmt::Formatter<'_>,
    name: &str,
    file_open_label: &str,
    diff: &PhaseDiff,
) -> fmt::Result {
    writeln!(
        formatter,
        "\n{name}: wall {}, filesystem calls {}, throughput {}",
        format_change(diff.wall_time_percent, false),
        format_change(diff.syscall_time_percent, false),
        format_change(diff.throughput_percent, true)
    )?;
    write_operation_diff_header(formatter)?;
    write_operation_diff(formatter, "directory create", &diff.directory_create)?;
    write_operation_diff(formatter, "directory open", &diff.directory_open)?;
    write_operation_diff(formatter, "directory read", &diff.directory_read)?;
    write_operation_diff(formatter, "directory close", &diff.directory_close)?;
    write_operation_diff(formatter, file_open_label, &diff.file_open)?;
    write_operation_diff(formatter, "file I/O", &diff.file_io)?;
    write_operation_diff(formatter, "file close", &diff.file_close)?;

    if diff.per_file_size.is_empty() {
        return Ok(());
    }
    writeln!(
        formatter,
        "\n{:>10} {:>16} {:>16} {:>16} {:>16} {:>16} {:>16} {:>16}",
        "size", "sum", "p50", "p90", "p99", "open sum", "I/O sum", "close sum"
    )?;
    for size in &diff.per_file_size {
        writeln!(
            formatter,
            "{:>10} {:>16} {:>16} {:>16} {:>16} {:>16} {:>16} {:>16}",
            format_byte_size(size.size_bytes as u64),
            format_change(size.sum_time_percent, false),
            format_change(size.p50_latency_percent, false),
            format_change(size.p90_latency_percent, false),
            format_change(size.p99_latency_percent, false),
            format_change(size.open_sum_time_percent, false),
            format_change(size.io_sum_time_percent, false),
            format_change(size.close_sum_time_percent, false),
        )?;
    }
    Ok(())
}

fn write_operation_diff_header(formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    writeln!(
        formatter,
        "{:<20} {:>16} {:>16} {:>16} {:>16}",
        "operation", "sum", "p50", "p90", "p99"
    )
}

fn write_operation_diff(
    formatter: &mut fmt::Formatter<'_>,
    name: &str,
    diff: &OperationDiff,
) -> fmt::Result {
    if diff.calls == 0 {
        return Ok(());
    }
    writeln!(
        formatter,
        "{name:<20} {:>16} {:>16} {:>16} {:>16}",
        format_change(diff.sum_time_percent, false),
        format_change(diff.p50_latency_percent, false),
        format_change(diff.p90_latency_percent, false),
        format_change(diff.p99_latency_percent, false),
    )
}

fn format_change(percent: Option<f64>, higher_is_better: bool) -> String {
    let Some(percent) = percent else {
        return "n/a".to_owned();
    };
    if percent.abs() < 0.05 {
        return "unchanged".to_owned();
    }
    let improved = (percent > 0.0) == higher_is_better;
    format!(
        "{:.1}% {}",
        percent.abs(),
        if improved { "faster" } else { "slower" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug::bench::fsio::FileSizeConfig;
    use crate::debug::bench::fsio::FsIoConfig;

    fn operation(scale: f64) -> OperationResult {
        OperationResult {
            calls: 10,
            sum_seconds: scale,
            average_nanoseconds: scale * 100_000_000.0,
            p50_nanoseconds: (scale * 100_000.0) as u64,
            p90_nanoseconds: (scale * 200_000.0) as u64,
            p99_nanoseconds: (scale * 300_000.0) as u64,
        }
    }

    fn phase(scale: f64) -> PhaseResult {
        let operation = operation(scale);
        PhaseResult {
            wall_seconds: scale,
            throughput_mib_per_second: 100.0 / scale,
            syscall_seconds: scale / 2.0,
            syscall_nanoseconds_per_file: scale * 50_000_000.0,
            files: 10,
            directories: 2,
            bytes: 1_000,
            directory_create: operation,
            directory_open: operation,
            directory_read: operation,
            directory_close: operation,
            file_open: operation,
            file_io: operation,
            file_close: operation,
            per_file_size: vec![FileSizeResult {
                size_bytes: 100,
                files: 10,
                bytes: 1_000,
                sum_seconds: scale,
                average_nanoseconds_per_file: scale * 100_000_000.0,
                p50_nanoseconds_per_file: (scale * 100_000.0) as u64,
                p90_nanoseconds_per_file: (scale * 200_000.0) as u64,
                p99_nanoseconds_per_file: (scale * 300_000.0) as u64,
                file_open: operation,
                file_io: operation,
                file_close: operation,
            }],
        }
    }

    fn result(scale: f64) -> FsIoResult {
        FsIoResult {
            config: FsIoConfig {
                read_only: false,
                file_list: false,
                skipped_files: 0,
                number_of_files: 10,
                files_per_dir: Some(5),
                virtual_repo_factor: 0,
                read_size_bytes: 4_096,
                write_size_bytes: 4_096,
                jobs: 1,
                drop_kernel_caches: 0,
                total_bytes: 1_000,
                file_sizes: vec![FileSizeConfig {
                    size_bytes: 100,
                    files: 10,
                }],
            },
            write: Some(phase(scale)),
            cache_preparation: CachePreparationResult {
                drop_kernel_caches: 0,
                wall_seconds: scale,
                file_sync: operation(scale),
                cache_drop_method: "disabled".to_owned(),
            },
            read: phase(scale),
            remove: Some(RemoveResult {
                wall_seconds: scale,
                files_per_second: 10.0 / scale,
                syscall_seconds: scale / 2.0,
                syscall_nanoseconds_per_file: scale * 50_000_000.0,
                files: 10,
                directories: 2,
                file_unlink: operation(scale),
                directory_remove: operation(scale),
            }),
            diff: None,
        }
    }

    #[test]
    fn computes_and_formats_improvements() {
        let baseline = result(1.0);
        let mut current = result(0.5);

        current
            .add_diff(&baseline)
            .expect("matching runs should compare");

        let diff = current.diff.as_ref().expect("diff should be populated");
        let write = diff
            .write
            .as_ref()
            .expect("read-write runs have a write diff");
        assert_eq!(write.wall_time_percent, Some(-50.0));
        assert_eq!(write.throughput_percent, Some(100.0));
        let output = current.to_string();
        assert!(output.contains(
            "wall 50.0% faster, filesystem calls 50.0% faster, throughput 100.0% faster"
        ));
        assert!(output.contains("file I/O"));
    }

    #[test]
    fn compares_cache_preparation_only_when_caches_were_dropped() {
        let baseline = result(1.0);
        let mut current = result(0.5);
        current
            .add_diff(&baseline)
            .expect("matching runs should compare");
        let diff = current.diff.as_ref().expect("diff should be populated");
        assert!(diff.cache_preparation.is_none());

        let mut baseline = result(1.0);
        baseline.cache_preparation.drop_kernel_caches = 1;
        let mut current = result(0.5);
        current.cache_preparation.drop_kernel_caches = 1;
        current
            .add_diff(&baseline)
            .expect("matching runs should compare");
        assert!(
            current
                .to_string()
                .contains("Read-cache preparation: 50.0% faster")
        );
    }

    #[test]
    fn rejects_configuration_and_call_count_mismatches() {
        let baseline = result(1.0);
        let mut different_config = result(1.0);
        different_config.config.number_of_files = 11;
        assert!(different_config.add_diff(&baseline).is_err());

        let mut different_calls = result(1.0);
        different_calls
            .write
            .as_mut()
            .expect("read-write runs have a write phase")
            .file_io
            .calls = 11;
        assert!(different_calls.add_diff(&baseline).is_err());
    }

    #[test]
    fn result_json_can_be_reused_as_a_baseline() {
        let baseline = result(1.0);
        let mut current = result(0.5);
        current
            .add_diff(&baseline)
            .expect("matching runs should compare");

        let json = serde_json::to_string(&current).expect("result should serialize");
        let parsed: FsIoResult = serde_json::from_str(&json).expect("result should deserialize");

        assert_eq!(parsed.config, current.config);
        assert!(
            parsed.diff.is_none(),
            "nested baseline diffs should be ignored"
        );
    }

    #[test]
    fn handles_zero_baselines_and_metric_directions() {
        assert_eq!(percent_change(0.0, 0.0), Some(0.0));
        assert_eq!(percent_change(1.0, 0.0), None);
        assert_eq!(format_change(Some(-10.0), false), "10.0% faster");
        assert_eq!(format_change(Some(10.0), true), "10.0% faster");
        assert_eq!(format_change(Some(10.0), false), "10.0% slower");
    }
}
