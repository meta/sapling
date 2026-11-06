/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! RIM workload rate limiting on the EdenAPI request path. RIM failures fail open.
//!
//! Configerator source of truth: `source/rim/backend_settings/mononoke_workload/`.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use anyhow::ensure;
use backend_if::RimBackend;
use cached_config::ConfigHandle;
use cached_config::ConfigStore;
use context::CoreContext;
use context::PerfCounterType;
use gotham::state::State;
use gotham_ext::middleware::PostResponseCallbacks;
use metadata::Metadata;
use permission_checker::MononokeIdentitySetExt;
use rate_limiting_config::RimMetadataField;
use rate_limiting_config::RimWorkloadConfig;
use rim_if::ReturnCode;
use rim_ligen::RimThinClient;
use sha2::Digest;
use sha2::Sha256;
use stats::prelude::*;
use tokio::time::timeout;
use tracing::debug;
use tracing::warn;

const RIM_ACQUIRE_TIMEOUT: Duration = Duration::from_millis(500);
const WORKLOAD_CONFIG_PATH: &str = "scm/mononoke/ratelimiting/rim_workloads";
pub(crate) const STORAGE_READS_RESOURCE: &str = "storage_reads";

define_stats! {
    prefix = "mononoke.rim";
    admission_report_errors: timeseries(Rate, Sum),
}

pub(crate) type WorkloadConfig = ConfigHandle<RimWorkloadConfig>;

#[derive(Clone)]
pub(crate) struct RimRateLimiter {
    pub backend: RimBackend,
    pub config: WorkloadConfig,
    pub cpu_resource: String,
}

pub(crate) fn workload_config(store: &ConfigStore) -> Result<WorkloadConfig> {
    store.get_config_handle(WORKLOAD_CONFIG_PATH.to_owned())
}

pub(crate) fn request_attribution(
    metadata: &Metadata,
    config: &RimWorkloadConfig,
) -> (Vec<String>, Option<String>) {
    let path = metadata.rim_tenancy_path();
    let selection_key = config
        .unit_of_work
        .get(path.category().as_str())
        .and_then(|field| metadata_field(metadata, *field));
    let path = path.with_workload_allocation(|category, workload| {
        config
            .named_workloads
            .get(category)
            .is_some_and(|workloads| workloads.contains(workload))
    });
    (path.into_vec(), selection_key)
}

fn metadata_field(metadata: &Metadata, field: RimMetadataField) -> Option<String> {
    let (prefix, value) = match field {
        RimMetadataField::SANDCASTLE_JOB => {
            ("SANDCASTLE_JOB", metadata.identities().sandcastle_job_id())
        }
        _ => return None,
    };
    value
        .filter(|value| !value.is_empty())
        .map(|value| format!("{prefix}:{value}"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RimDecision {
    Allow,
    Reject,
    FailOpen,
}

fn unit_decision(backend: RimBackend, key: &str, admit_percent: f64) -> RimDecision {
    if !admit_percent.is_finite() || !(0.0..=100.0).contains(&admit_percent) {
        return RimDecision::FailOpen;
    }
    let hash = Sha256::new()
        .chain_update(backend.0.to_be_bytes())
        .chain_update(key.as_bytes())
        .finalize();
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&hash[..8]);
    let rank = (u64::from_be_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64;
    if rank * 100.0 < admit_percent {
        RimDecision::Allow
    } else {
        RimDecision::Reject
    }
}

/// Call once at server startup. Failures are logged and swallowed so RIM
/// infra outages can't prevent the server from serving traffic.
pub(crate) fn init(rim_backend: RimBackend) {
    match RimThinClient::initialize(rim_backend) {
        Ok(status) if status.success() => {
            debug!("RIM thin client initialized for backend {:?}", rim_backend);
        }
        Ok(status) => {
            warn!(
                "RIM initialize returned non-success: code={:?} msg={}",
                status.code(),
                status.message(),
            );
        }
        Err(e) => {
            warn!("RIM initialize failed for backend {:?}: {}", rim_backend, e);
        }
    }
}

pub(crate) async fn check_rate_limit(
    ctx: &CoreContext,
    tenancy_path: &[String],
    rim_backend: RimBackend,
    requirements: &HashMap<String, f64>,
    selection_key: Option<&str>,
) -> RimDecision {
    let log = |tag: &str, detail: String| {
        let mut scuba = ctx.scuba().clone();
        scuba.add("rim_tenancy_path", tenancy_path.join("/"));
        scuba.add("rim_backend", rim_backend.0);
        scuba.add("rim_requirements", format!("{requirements:?}"));
        scuba.log_with_msg(tag, detail);
    };

    let result = match timeout(RIM_ACQUIRE_TIMEOUT, async {
        let options = rim_ligen::create_acquire_options(false)?;
        RimThinClient::acquire_v2(
            rim_backend,
            tenancy_path.to_vec(),
            requirements.clone(),
            options,
        )
        .await
    })
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => {
            log("RIM rate-limit check error", e.to_string());
            return RimDecision::FailOpen;
        }
        Err(_) => {
            log(
                "RIM rate-limit check timeout",
                format!("timeout after {RIM_ACQUIRE_TIMEOUT:?}"),
            );
            return RimDecision::FailOpen;
        }
    };

    if result.failed() && !result.rejected() {
        log(
            "RIM rate-limit check failed",
            format!("code={:?}", result.code()),
        );
        return RimDecision::FailOpen;
    }

    let decision = if let Some(key) = selection_key {
        let decision = unit_decision(rim_backend, key, result.admit_probability());
        if decision == RimDecision::FailOpen {
            log(
                "RIM rate-limit check failed",
                format!(
                    "invalid admission percentage: {}",
                    result.admit_probability()
                ),
            );
            return decision;
        }
        decision
    } else if result.rejected() {
        RimDecision::Reject
    } else {
        RimDecision::Allow
    };
    if decision == RimDecision::Reject {
        log(
            "RIM rejected request",
            format!("admission percentage={}", result.admit_probability()),
        );
    }
    decision
}

fn report_admission(
    backend: RimBackend,
    path: &[String],
    requirements: &HashMap<String, f64>,
    rejected: bool,
) -> Result<()> {
    let code = if rejected {
        ReturnCode::ADMITTANCE_REJECTED
    } else {
        ReturnCode::SUCCESS
    };
    let status = RimThinClient::track_resource_request_explicitly(
        backend,
        path.to_vec(),
        requirements
            .keys()
            .map(|resource| (resource.clone(), (code, 1)))
            .collect(),
    )?;
    ensure!(status.success(), "code={:?}", status.code());
    Ok(())
}

pub(crate) fn report_request(
    ctx: &CoreContext,
    tenancy_path: &[String],
    rim_backend: RimBackend,
    usage: &HashMap<String, f64>,
    rejected: bool,
) {
    let log = |tag: &str, detail: String| {
        let mut scuba = ctx.scuba().clone();
        scuba.add("rim_tenancy_path", tenancy_path.join("/"));
        scuba.add("rim_backend", rim_backend.0);
        scuba.add("rim_requirements", format!("{usage:?}"));
        scuba.log_with_msg(tag, detail);
    };

    if let Err(error) = report_admission(rim_backend, tenancy_path, usage, rejected) {
        STATS::admission_report_errors.add_value(1);
        log("RIM admission report failed", error.to_string());
    }
}

pub(crate) fn record_usage(
    state: &mut State,
    ctx: CoreContext,
    backend: RimBackend,
    tenancy_path: Vec<String>,
    cpu_resource: String,
) {
    if let Some(callbacks) = state.try_borrow_mut::<PostResponseCallbacks>() {
        callbacks.add(move |_| {
            let usage = completed_usage(&ctx, &cpu_resource);
            let result = RimThinClient::sync_report(backend, tenancy_path.clone(), usage);
            let error = match result {
                Ok(result) if result.success() => return,
                Ok(result) => format!("{:?}", result.status()),
                Err(error) => error.to_string(),
            };
            let mut scuba = ctx.scuba().clone();
            scuba.add("rim_tenancy_path", tenancy_path.join("/"));
            scuba.add("rim_backend", backend.0);
            scuba.log_with_msg("RIM usage report failed", error);
        });
    }
}

fn completed_usage(ctx: &CoreContext, cpu_resource: &str) -> HashMap<String, f64> {
    let counters = ctx.perf_counters();
    [
        (
            cpu_resource.to_owned(),
            counters.get_counter(PerfCounterType::CpuTimeNs) as f64 / 1000.0,
        ),
        (
            STORAGE_READS_RESOURCE.to_owned(),
            counters.get_counter(PerfCounterType::BlobGets) as f64,
        ),
    ]
    .into()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use backend_if::RimBackend;
    use cached_config::ConfigStore;
    use cached_config::ModificationTime;
    use cached_config::TestSource;
    use clientinfo::ClientInfo;
    use metadata::Metadata;
    use mononoke_macros::mononoke;
    use permission_checker::MononokeIdentity;
    use rate_limiting_config::RimMetadataField;
    use rate_limiting_config::RimWorkloadConfig;

    use super::RimDecision;
    use super::WORKLOAD_CONFIG_PATH;
    use super::request_attribution;
    use super::unit_decision;
    use super::workload_config;

    #[mononoke::test]
    fn unit_selection_respects_cutoffs_and_rejects_invalid_percentages() {
        let backend = RimBackend::RIM_USE_CASE_52;
        let key = "SANDCASTLE_JOB:123";
        assert_eq!(unit_decision(backend, key, 0.0), RimDecision::Reject);
        assert_eq!(unit_decision(backend, key, 100.0), RimDecision::Allow);
        for invalid in [-1.0, 101.0, f64::NAN, f64::INFINITY] {
            assert_eq!(unit_decision(backend, key, invalid), RimDecision::FailOpen);
        }
    }

    #[mononoke::test]
    fn unit_selection_is_nested() {
        let backend = RimBackend::RIM_USE_CASE_52;
        let mut admitted = 0;
        for job in 0..2000 {
            let key = format!("SANDCASTLE_JOB:{job}");
            if unit_decision(backend, &key, 50.0) == RimDecision::Allow {
                admitted += 1;
                assert_eq!(unit_decision(backend, &key, 80.0), RimDecision::Allow);
            } else {
                assert_eq!(unit_decision(backend, &key, 20.0), RimDecision::Reject);
            }
        }
        assert!((850..1150).contains(&admitted));
    }

    fn ci_metadata(job: Option<&str>, purpose: Option<&str>) -> Metadata {
        let mut metadata = Metadata::default().set_identities(
            std::iter::once(MononokeIdentity::from_legacy_type_data(
                "SERVICE_IDENTITY",
                "sandcastle.svn.push",
            ))
            .chain(job.map(|job| MononokeIdentity::from_legacy_type_data("SANDCASTLE_JOB", job)))
            .collect(),
        );
        metadata.add_client_info(
            ClientInfo::from_json(
                &serde_json::json!({"sandcastle_alias": "build", "ci_purpose": purpose})
                    .to_string(),
            )
            .unwrap(),
        );
        metadata
    }

    fn unit_config() -> RimWorkloadConfig {
        RimWorkloadConfig {
            unit_of_work: [("ci_sandcastle".to_owned(), RimMetadataField::SANDCASTLE_JOB)].into(),
            named_workloads: [(
                "ci_sandcastle".to_owned(),
                ["CI_PURPOSE:land".to_owned()].into(),
            )]
            .into(),
            ..Default::default()
        }
    }

    #[mononoke::test]
    fn ci_attempts_share_a_purpose_without_splitting_fetch_causes() {
        let config = unit_config();
        for job in ["123", "456"] {
            for cause in [
                None,
                Some("sl-prefetch"),
                Some("edenfs-prefetch"),
                Some("eden-walk-prefetch"),
                Some("sl-checkout"),
            ] {
                let metadata =
                    ci_metadata(Some(job), Some("land")).set_fetch_cause(cause.map(str::to_owned));
                let (path, key) = request_attribution(&metadata, &config);
                let expected: Vec<_> = std::iter::once("root")
                    .chain([
                        "ci_sandcastle",
                        "CI_PURPOSE:land",
                        "CI_PURPOSE:land",
                        "default",
                    ])
                    .collect();
                assert_eq!(path, expected);
                assert_eq!(key, Some(format!("SANDCASTLE_JOB:{job}")));
            }
        }
    }

    #[mononoke::test]
    fn unit_selection_requires_both_config_and_a_unit_id() {
        let mut config = unit_config();
        for job in [None, Some("")] {
            assert_eq!(
                request_attribution(&ci_metadata(job, Some("land")), &config).1,
                None
            );
        }
        let metadata = ci_metadata(Some("123"), Some("land"));
        let interactive = Metadata::default()
            .set_identities([MononokeIdentity::from_legacy_type_data("USER", "alice")].into());
        assert_eq!(request_attribution(&interactive, &config).1, None);
        for purpose in [None, Some(""), Some("new_purpose")] {
            let (path, _) = request_attribution(&ci_metadata(Some("123"), purpose), &config);
            assert_eq!(path[2], "__mononoke_remaining__");
            assert_eq!(
                path[3],
                purpose
                    .filter(|p| !p.is_empty())
                    .map(|p| format!("CI_PURPOSE:{p}"))
                    .unwrap_or_else(|| "SERVICE_IDENTITY:sandcastle.svn.push".to_owned())
            );
        }
        let warming = Metadata::default().set_identities(
            [MononokeIdentity::from_legacy_type_data(
                "SERVICE_IDENTITY",
                "sandcastle.worker.warmed",
            )]
            .into(),
        );
        let (path, key) = request_attribution(&warming, &config);
        assert_eq!(path[1], "sandcastle_automation");
        assert_eq!(key, None);
        config.unit_of_work.clear();
        let metadata = metadata.set_fetch_cause(Some("sl-prefetch".to_owned()));
        let (path, key) = request_attribution(&metadata, &config);
        assert_eq!(key, None);
        assert_eq!(
            path,
            vec![
                "root",
                "ci_sandcastle",
                "CI_PURPOSE:land",
                "CI_PURPOSE:land",
                "default",
            ]
        );
    }

    #[mononoke::test]
    fn allocation_follows_live_catalog() {
        let source = Arc::new(TestSource::new());
        let store = ConfigStore::new(source.clone(), None::<Duration>, None);
        let mut handle = None;
        for (version, named) in [(1, false), (2, true), (3, false)] {
            let config = RimWorkloadConfig {
                allocation_backends: Default::default(),
                unit_of_work: Default::default(),
                named_workloads: if named {
                    [("unknown".to_owned(), ["unknown".to_owned()].into())].into()
                } else {
                    Default::default()
                },
            };
            source.insert_config(
                WORKLOAD_CONFIG_PATH,
                core::str::from_utf8(&fbthrift::simplejson_protocol::serialize(&config)).unwrap(),
                ModificationTime::UnixTimestamp(version),
            );
            source.insert_to_refresh(WORKLOAD_CONFIG_PATH.to_owned());
            let handle = handle.get_or_insert_with(|| workload_config(&store).unwrap());
            store.force_update_configs();
            assert_eq!(
                request_attribution(&Metadata::default(), &handle.get()).0,
                [
                    "root",
                    "unknown",
                    if named {
                        "unknown"
                    } else {
                        "__mononoke_remaining__"
                    },
                    "unknown",
                    "default"
                ],
            );
            source.insert_config(
                WORKLOAD_CONFIG_PATH,
                "invalid",
                ModificationTime::UnixTimestamp(100 + version),
            );
            source.insert_to_refresh(WORKLOAD_CONFIG_PATH.to_owned());
            store.force_update_configs();
            assert_eq!(*handle.get(), config);
        }
    }

    #[mononoke::fbinit_test]
    fn usage_reads_completed_counters_in_cpu_microseconds(fb: fbinit::FacebookInit) {
        let ctx = context::CoreContext::test_mock(fb);
        let before = super::completed_usage(&ctx, "cpu_ldc");
        assert_eq!(before["cpu_ldc"], 0.0);
        ctx.perf_counters()
            .add_to_counter(context::PerfCounterType::CpuTimeNs, 1_234_567);
        ctx.perf_counters()
            .add_to_counter(context::PerfCounterType::BlobGets, 7);
        assert_eq!(
            super::completed_usage(&ctx, "cpu_ldc"),
            [
                ("cpu_ldc".to_owned(), 1234.567),
                ("storage_reads".to_owned(), 7.0)
            ]
            .into(),
        );
    }

    #[mononoke::test]
    fn workload_catalog_is_required_at_startup() {
        let source = Arc::new(TestSource::new());
        let store = ConfigStore::new(source.clone(), None::<Duration>, None);
        assert!(workload_config(&store).is_err());

        source.insert_config(
            WORKLOAD_CONFIG_PATH,
            "invalid",
            ModificationTime::UnixTimestamp(1),
        );
        assert!(workload_config(&store).is_err());

        let config = RimWorkloadConfig::default();
        source.insert_config(
            WORKLOAD_CONFIG_PATH,
            core::str::from_utf8(&fbthrift::simplejson_protocol::serialize(&config)).unwrap(),
            ModificationTime::UnixTimestamp(2),
        );
        let handle = workload_config(&store).unwrap();
        assert_eq!(*handle.get(), config);
    }
}
