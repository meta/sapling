/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use context::CoreContext;
use diff::types::DiffSingleInput;
use diff::utils::content::get_content_id_from_changeset_path;
use mononoke_api::RepoContext;

use crate::errors::ServerError;
use crate::repo::Repo;
use crate::service::ResolvedDiffInputs;
use crate::service::ResolvedDiffSide;

/// Read the `scm/mononoke:diff_service_memory_multiplier` JustKnob value.
pub(crate) fn memory_multiplier() -> u64 {
    justknobs::get_as::<u64>("scm/mononoke:diff_service_memory_multiplier", None)
}

/// Read the `scm/mononoke:diff_service_default_input_estimate_bytes` JustKnob value.
fn default_input_estimate_bytes() -> u64 {
    justknobs::get_as::<u64>("scm/mononoke:diff_service_default_input_estimate_mb", None)
        * 1024
        * 1024
}

/// Guard that releases reserved bytes from the inflight counter on drop.
pub struct InflightBytesGuard {
    inflight_bytes: Arc<AtomicU64>,
    reserved: u64,
}

impl Drop for InflightBytesGuard {
    fn drop(&mut self) {
        self.inflight_bytes
            .fetch_sub(self.reserved, Ordering::Relaxed);
    }
}

/// Check whether there is enough free memory for the estimated diff size,
/// accounting for bytes already reserved by concurrent in-flight requests.
/// On success, returns a guard that holds the reservation until dropped.
pub fn try_reserve_memory(
    ctx: &CoreContext,
    phase: &str,
    estimated_bytes: u64,
    inflight_bytes: &Arc<AtomicU64>,
) -> Result<InflightBytesGuard, ServerError> {
    let stats = match memory::get_stats() {
        Ok(stats) => stats,
        Err(_) => {
            // If memory tracking isn't configured, still track inflight for observability
            return Ok(InflightBytesGuard {
                inflight_bytes: inflight_bytes.clone(),
                reserved: 0,
            });
        }
    };
    let multiplier = memory_multiplier();
    try_reserve_memory_with_stats(
        ctx,
        phase,
        estimated_bytes,
        inflight_bytes,
        stats,
        multiplier,
    )
}

fn try_reserve_memory_with_stats(
    ctx: &CoreContext,
    phase: &str,
    estimated_bytes: u64,
    inflight_bytes: &Arc<AtomicU64>,
    stats: memory::MemoryStats,
    multiplier: u64,
) -> Result<InflightBytesGuard, ServerError> {
    let free_bytes = stats.rss_free_bytes.saturating_sub(stats.swap_bytes) as u64;
    let adjusted_bytes = estimated_bytes.saturating_mul(multiplier);

    // Reserve our bytes and check total against free memory.
    //
    // Note: this is intentionally conservative. rss_free_bytes is max_memory
    // minus current RSS, but current RSS already includes memory allocated by
    // in-flight requests that are also counted in the inflight counter. This
    // double-counting means we reject earlier than strictly necessary, but it
    // is safe and the multiplier JK can be tuned to compensate.
    let prev_inflight = inflight_bytes.fetch_add(adjusted_bytes, Ordering::Relaxed);
    let total_inflight = prev_inflight.saturating_add(adjusted_bytes);

    if total_inflight > free_bytes {
        // undo the reservation
        inflight_bytes.fetch_sub(adjusted_bytes, Ordering::Relaxed);

        ctx.scuba()
            .clone()
            .unsampled()
            .add("memory_admission_phase", phase)
            .add("estimated_input_size", estimated_bytes)
            .add("memory_admission_multiplier", multiplier)
            .add("memory_admission_adjusted_bytes", adjusted_bytes)
            .add("memory_admission_reserved_before_bytes", prev_inflight)
            .add("memory_admission_proposed_total_bytes", total_inflight)
            .add_memory_stats(&stats)
            .add("memory_admission_swap_bytes", stats.swap_bytes)
            .add("memory_admission_free_bytes", free_bytes)
            .log_with_msg("Memory admission rejected", None);

        let message = format!(
            "Insufficient memory: request needs ~{} bytes ({}x multiplier = {} adjusted), \
             {} bytes already reserved by inflight requests, but only {} bytes free \
             (max_memory={}, current_rss={}, current_swap={})",
            estimated_bytes,
            multiplier,
            adjusted_bytes,
            prev_inflight,
            free_bytes,
            stats.total_rss_bytes,
            stats.total_rss_bytes.saturating_sub(stats.rss_free_bytes),
            stats.swap_bytes,
        );
        return Err(ServerError::Overloaded(message));
    }

    Ok(InflightBytesGuard {
        inflight_bytes: inflight_bytes.clone(),
        reserved: adjusted_bytes,
    })
}

/// Estimate total input size for a diff request by summing the estimated
/// sizes of both inputs. Errors (including JustKnob read failures) are
/// propagated to the caller.
pub async fn estimate_diff_size(
    inputs: &ResolvedDiffInputs,
    ctx: &CoreContext,
) -> Result<u64, anyhow::Error> {
    let base_fut = async {
        match &inputs.base {
            ResolvedDiffSide::Resolved(input) => {
                estimate_input_size(ctx, &inputs.base_repo, input).await
            }
            _ => Ok(0),
        }
    };
    let other_fut = async {
        match &inputs.other {
            ResolvedDiffSide::Resolved(input) => {
                estimate_input_size(ctx, &inputs.other_repo, input).await
            }
            _ => Ok(0),
        }
    };

    let (base_result, other_result) = futures::join!(base_fut, other_fut);
    Ok(base_result? + other_result?)
}

async fn estimate_input_size(
    ctx: &CoreContext,
    repo: &RepoContext<Repo>,
    input: &DiffSingleInput,
) -> Result<u64, anyhow::Error> {
    let default = default_input_estimate_bytes();

    // Other errors (blobstore issues, missing content) fall back to the
    // JK default so that admission control is never bypassed.
    let result: anyhow::Result<u64> = async {
        let cid = match input {
            DiffSingleInput::ChangesetPath(cs_path) => get_content_id_from_changeset_path(
                ctx,
                repo.repo(),
                cs_path.changeset_id,
                cs_path.path.clone(),
            )
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No content found for path {} at changeset {}",
                    cs_path.path,
                    cs_path.changeset_id
                )
            })?,
            DiffSingleInput::Content(content) => content.content_id,
            DiffSingleInput::String(s) => return Ok(s.content.len() as u64),
        };

        let key = filestore::FetchKey::Canonical(cid);
        let metadata = filestore::get_metadata(&repo.repo_blobstore(), ctx, &key).await?;
        Ok(metadata.map(|mtd| mtd.total_size).unwrap_or(default))
    }
    .await;

    Ok(result.unwrap_or_else(|err| {
        tracing::warn!(
            "Failed to estimate input size, using default of {} bytes: {:#}",
            default,
            err,
        );
        default
    }))
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use anyhow::Result;
    use anyhow::anyhow;
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;
    use scuba_ext::MononokeScubaSampleBuilder;
    use serde_json::Value;
    use tempfile::NamedTempFile;

    use super::*;

    #[mononoke::fbinit_test]
    fn rejected_admission_logs_decision_operands(fb: FacebookInit) -> Result<()> {
        // Given an admission attempt whose adjusted reservation exceeds RSS-plus-swap headroom.
        let log_file = NamedTempFile::new()?;
        let scuba = MononokeScubaSampleBuilder::with_discard().with_log_file(log_file.path())?;
        let ctx = CoreContext::new_with_scuba(fb, scuba);
        let inflight_bytes = Arc::new(AtomicU64::new(40));
        let stats = memory::MemoryStats {
            total_rss_bytes: 1_000,
            rss_free_bytes: 900,
            rss_free_pct: 90.0,
            swap_bytes: 800,
        };

        // When the request needs 30 bytes with a multiplier of 3.
        let result = try_reserve_memory_with_stats(&ctx, "initial", 30, &inflight_bytes, stats, 3);

        // Then it is rejected with the overload-specific wire error, its tentative
        // reservation is released, and the exact decision operands are logged.
        assert!(matches!(
            result,
            Err(ServerError::Overloaded(message)) if message.contains("Insufficient memory")
        ));
        assert_eq!(inflight_bytes.load(Ordering::Relaxed), 40);

        let contents = std::fs::read_to_string(log_file.path())
            .with_context(|| format!("failed to read scuba log {}", log_file.path().display()))?;
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let row: Value = serde_json::from_str(lines[0])?;
        let fields = row
            .as_object()
            .ok_or_else(|| anyhow!("scuba row should be a JSON object"))?
            .values()
            .filter_map(Value::as_object)
            .flat_map(|category| category.iter())
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<serde_json::Map<_, _>>();

        for (field, expected) in [
            ("log_tag", "Memory admission rejected"),
            ("memory_admission_phase", "initial"),
            ("estimated_input_size", "30"),
            ("memory_admission_multiplier", "3"),
            ("memory_admission_adjusted_bytes", "90"),
            ("memory_admission_reserved_before_bytes", "40"),
            ("memory_admission_proposed_total_bytes", "130"),
            ("total_rss_bytes", "1000"),
            ("rss_free_bytes", "900"),
            ("memory_admission_swap_bytes", "800"),
            ("memory_admission_free_bytes", "100"),
        ] {
            assert_eq!(
                logged_field(&fields, field).as_deref(),
                Some(expected),
                "unexpected {field}"
            );
        }
        assert_eq!(
            fields.get("rss_free_pct").and_then(Value::as_f64),
            Some(90.0)
        );
        for duplicate in [
            "memory_admission_estimated_bytes",
            "memory_admission_max_memory_bytes",
            "memory_admission_current_rss_bytes",
            "memory_admission_accepted",
        ] {
            assert!(!fields.contains_key(duplicate), "unexpected {duplicate}");
        }

        Ok(())
    }

    #[mononoke::fbinit_test]
    fn accepted_admission_does_not_log(fb: FacebookInit) -> Result<()> {
        // Given an admission attempt whose adjusted reservation fits in RSS-plus-swap headroom.
        let log_file = NamedTempFile::new()?;
        let scuba = MononokeScubaSampleBuilder::with_discard().with_log_file(log_file.path())?;
        let ctx = CoreContext::new_with_scuba(fb, scuba);
        let inflight_bytes = Arc::new(AtomicU64::new(40));
        let stats = memory::MemoryStats {
            total_rss_bytes: 1_000,
            rss_free_bytes: 900,
            rss_free_pct: 90.0,
            swap_bytes: 700,
        };

        // When the request needs 30 bytes with a multiplier of 3.
        let guard = try_reserve_memory_with_stats(&ctx, "initial", 30, &inflight_bytes, stats, 3)
            .map_err(|error| anyhow!("admission should succeed: {error:?}"))?;

        // Then its reservation is held without emitting a row and released on drop.
        assert_eq!(inflight_bytes.load(Ordering::Relaxed), 130);
        drop(guard);
        assert_eq!(inflight_bytes.load(Ordering::Relaxed), 40);
        assert!(std::fs::read_to_string(log_file.path())?.is_empty());

        Ok(())
    }

    #[mononoke::fbinit_test]
    fn swap_exceeding_rss_headroom_rejects_without_reserving(fb: FacebookInit) {
        let ctx = CoreContext::new_with_scuba(fb, MononokeScubaSampleBuilder::with_discard());
        let inflight_bytes = Arc::new(AtomicU64::new(0));
        let stats = memory::MemoryStats {
            total_rss_bytes: 1_000,
            rss_free_bytes: 900,
            rss_free_pct: 90.0,
            swap_bytes: 1_100,
        };
        let result = try_reserve_memory_with_stats(&ctx, "initial", 1, &inflight_bytes, stats, 1);
        assert!(matches!(result, Err(ServerError::Overloaded(_))));
        assert_eq!(inflight_bytes.load(Ordering::Relaxed), 0);
    }

    fn logged_field(fields: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
        match fields.get(key) {
            Some(Value::String(value)) => Some(value.clone()),
            Some(Value::Bool(value)) => Some(value.to_string()),
            Some(Value::Number(value)) => Some(value.to_string()),
            _ => None,
        }
    }
}
