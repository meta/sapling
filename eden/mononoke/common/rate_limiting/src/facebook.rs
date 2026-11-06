/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::RwLock;

use anyhow::Error;
use async_trait::async_trait;
use fbinit::FacebookInit;
use fbwhoami::FbWhoAmI;
use ods_counters::OdsCounterManager;
use permission_checker::MononokeIdentitySet;
use rate_limiting_config::RateLimitStatus;
use ratelim::loadlimiter;
use ratelim::loadlimiter::LoadCost;
use ratelim::loadlimiter::LoadLimitCounter;
use scuba_ext::MononokeScubaSampleBuilder;

use crate::BoxRateLimiter;
use crate::FciMetric;
use crate::LoadShedResult;
use crate::Metric;
use crate::MononokeRateLimitConfig;
use crate::RateLimit;
use crate::RateLimitBody;
use crate::RateLimitReason;
use crate::RateLimitResult;
use crate::RateLimiter;
use crate::Scope;
use crate::log_would_have_rate_limited;

pub static MAIN_IDS_THRESHOLDS: LazyLock<HashMap<&'static str, f64>> = LazyLock::new(|| {
    let mut main_id_to_threshold = HashMap::new();
    main_id_to_threshold.insert("MACHINE_TIER:snc", 0.9);
    main_id_to_threshold.insert("MACHINE_TIER:ash", 0.9);
    main_id_to_threshold
});

pub fn create_rate_limiter(
    fb: FacebookInit,
    category: String,
    config: Arc<MononokeRateLimitConfig>,
    ods_counters: Arc<RwLock<OdsCounterManager>>,
) -> BoxRateLimiter {
    Box::new(MononokeRateLimits {
        config,
        fb,
        category: category.clone(),
        load_limits: Arc::new(LoadLimitsInner::new(category)),
        ods_counters,
    })
}

pub fn log_or_enforce_status(
    body: &RateLimitBody,
    metric: FciMetric,
    scuba: &mut MononokeScubaSampleBuilder,
    no_target: bool,
) -> RateLimitResult {
    match body.raw_config.status {
        RateLimitStatus::Disabled => RateLimitResult::Pass,
        RateLimitStatus::Tracked => {
            log_would_have_rate_limited(
                scuba,
                &metric.metric.to_string(),
                &RateLimitReason::RateLimitedMetric(metric.metric, metric.window, no_target),
            );
            RateLimitResult::Pass
        }
        RateLimitStatus::Enforced => RateLimitResult::Fail(RateLimitReason::RateLimitedMetric(
            metric.metric,
            metric.window,
            no_target,
        )),
        _ => panic!(
            "Thrift enums aren't real enums once in Rust. We have to account for other values here."
        ),
    }
}

async fn check_and_log_close_to_limit(
    fb: FacebookInit,
    counter: &LoadLimitCounter,
    main_id: &str,
    limit: &RateLimit,
    scuba: &mut MononokeScubaSampleBuilder,
) -> Result<(), Error> {
    if let Some(&threshold) = MAIN_IDS_THRESHOLDS.get(main_id) {
        let adjusted_limit = threshold * limit.body.raw_config.limit;
        if loadlimiter::should_throttle(fb, counter, adjusted_limit, limit.fci_metric.window)
            .await?
        {
            let msg = format!(
                "Rate limit close to exhaustion: {}% of {} in {}s",
                threshold * 100.0,
                limit.fci_metric.metric,
                limit.fci_metric.window.as_secs()
            );
            scuba.log_with_msg("Ratelimit close to exhaustion", Some(msg));
        }
    }
    Ok(())
}

#[async_trait]
impl RateLimiter for MononokeRateLimits {
    async fn check_rate_limit(
        &self,
        metric: Metric,
        identities: &MononokeIdentitySet,
        main_id: Option<&str>,
        scuba: &mut MononokeScubaSampleBuilder,
        atlas: Option<bool>,
    ) -> Result<RateLimitResult, Error> {
        for limit in &self.config.rate_limits {
            let fci_metric = limit.fci_metric;

            if fci_metric.metric != metric {
                continue;
            }

            if !limit.applies_to_client(identities, main_id, atlas) {
                continue;
            }

            let counter = self.counter(fci_metric.metric, fci_metric.scope);

            if loadlimiter::should_throttle(
                self.fb,
                counter,
                limit.body.raw_config.limit,
                limit.fci_metric.window,
            )
            .await?
            {
                let no_target = limit.target.is_none();
                match log_or_enforce_status(&limit.body, fci_metric, scuba, no_target) {
                    RateLimitResult::Pass => {
                        break;
                    }
                    RateLimitResult::Fail(reason) => RateLimitResult::Fail(reason),
                };
            } else if let Some(main_id) = main_id {
                check_and_log_close_to_limit(self.fb, counter, main_id, limit, scuba).await?;
            }
        }
        Ok(RateLimitResult::Pass)
    }

    fn check_load_shed(
        &self,
        identities: &MononokeIdentitySet,
        main_id: Option<&str>,
        scuba: &mut MononokeScubaSampleBuilder,
        atlas: Option<bool>,
    ) -> LoadShedResult {
        for limit in &self.config.load_shed_limits {
            if let LoadShedResult::Fail(reason) = limit.should_load_shed(
                self.fb,
                Some(identities),
                main_id,
                scuba,
                self.ods_counters.clone(),
                atlas,
            ) {
                return LoadShedResult::Fail(reason);
            }
        }
        LoadShedResult::Pass
    }

    fn bump_load(&self, metric: Metric, scope: Scope, load: LoadCost) {
        loadlimiter::bump_load(self.fb, self.counter(metric, scope), load)
    }

    fn category(&self) -> &str {
        &self.category
    }

    // Find the most specific rate limit that applies to the given identities or main_id
    // If no rate limit applies, return None
    // If multiple rate limits apply, return the most specific one
    // The most specific rate limit is the one that strictly matches the main_id
    // If none match, return the most specific rate limit that matches the most identities
    fn find_rate_limit(
        &self,
        metric: Metric,
        identities: Option<MononokeIdentitySet>,
        main_id: Option<&str>,
        atlas: Option<bool>,
    ) -> Option<crate::RateLimit> {
        // First, try to find a rate limit that matches the main client ID
        if let Some(main_id) = main_id {
            if let Some(rate_limit) = self
                .config
                .rate_limits
                .iter()
                .filter(|r| r.fci_metric.metric == metric)
                .find(|r| {
                    if let Some(crate::Target::MainClientId(ref id)) = r.target {
                        id == main_id
                    } else {
                        false
                    }
                })
                .cloned()
            {
                return Some(rate_limit);
            }
        }

        // If no main client ID match is found, find the most specific one that applies to the identities
        let mut max_identities = 0;
        let mut most_specific_rate_limit = self
            .config
            .rate_limits
            .iter()
            .filter(|r| r.fci_metric.metric == metric)
            .rfind(|r| r.target.is_none())
            .cloned();

        if let Some(identities) = identities {
            self.config
                .rate_limits
                .iter()
                .filter(|r| r.fci_metric.metric == metric)
                .filter(|r| r.applies_to_client(&identities, None, atlas))
                .for_each(|r| match &r.target {
                    Some(crate::Target::Identities(is)) => {
                        let num_identities = is.len();
                        if num_identities >= max_identities {
                            max_identities = num_identities;
                            most_specific_rate_limit = Some(r.clone());
                        }
                    }
                    _ => {}
                });
        }

        most_specific_rate_limit
    }
}

#[derive(Clone)]
pub struct MononokeRateLimits {
    config: Arc<MononokeRateLimitConfig>,
    fb: FacebookInit,
    category: String,
    load_limits: Arc<LoadLimitsInner>,
    ods_counters: Arc<RwLock<OdsCounterManager>>,
}

impl std::fmt::Debug for MononokeRateLimits {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("MononokeRateLimits")
            .field("category", &self.category)
            .field("load_limits", &self.load_limits)
            .finish()
    }
}

#[derive(Debug)]
struct LoadLimitsInner {
    regional_egress_bytes: LoadLimitCounter,
    regional_total_manifests: LoadLimitCounter,
    regional_getpack_files: LoadLimitCounter,
    regional_commits: LoadLimitCounter,
    commits_per_user: LoadLimitCounter,
    edenapi_qps: LoadLimitCounter,
}

impl LoadLimitsInner {
    pub fn new(category: String) -> Self {
        Self {
            regional_egress_bytes: LoadLimitCounter {
                category: category.clone(),
                key: make_regional_limit_key("egress-bytes"),
            },
            regional_total_manifests: LoadLimitCounter {
                category: category.clone(),
                key: make_regional_limit_key("egress-total-manifests"),
            },
            regional_getpack_files: LoadLimitCounter {
                category: category.clone(),
                key: make_regional_limit_key("egress-getpack-files"),
            },
            regional_commits: LoadLimitCounter {
                category: category.clone(),
                key: make_regional_limit_key("egress-commits"),
            },
            commits_per_user: LoadLimitCounter {
                category: category.clone(),
                key: "commits_per_author".to_string(),
            },
            edenapi_qps: LoadLimitCounter {
                category,
                key: "edenapi_qps".to_string(),
            },
        }
    }
}

fn make_regional_limit_key(prefix: &str) -> String {
    let fbwhoami = FbWhoAmI::get().unwrap();
    let region = fbwhoami.region_datacenter_prefix.as_deref().unwrap();
    let mut key = prefix.to_owned();
    key.push(':');
    key.push_str(region);
    key
}

impl MononokeRateLimits {
    fn counter(&self, metric: Metric, scope: Scope) -> &LoadLimitCounter {
        match (metric, scope) {
            (Metric::EgressBytes, Scope::Regional) => &self.load_limits.regional_egress_bytes,
            (Metric::TotalManifests, Scope::Regional) => &self.load_limits.regional_total_manifests,
            (Metric::GetpackFiles, Scope::Regional) => &self.load_limits.regional_getpack_files,
            (Metric::Commits, Scope::Regional) => &self.load_limits.regional_commits,
            (Metric::CommitsPerUser, Scope::Global) => &self.load_limits.commits_per_user,
            (Metric::EdenApiQps, Scope::Global) => &self.load_limits.edenapi_qps,
            _ => panic!("Unsupported metric/scope combination"),
        }
    }
}
