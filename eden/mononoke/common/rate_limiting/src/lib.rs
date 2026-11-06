/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::num::NonZeroU64;
use std::string::ToString;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::Error;
use anyhow::anyhow;
use async_trait::async_trait;
use cached_config::ConfigHandle;
use client_memory::ClientGroup;
use client_memory::global_client_memory_registry;
use fbinit::FacebookInit;
use mononoke_macros::mononoke;
use ods_counters::CounterManager;
use ods_counters::DesiredCountersProvider;
use ods_counters::OdsCounterKey;
use ods_counters::OdsCounterManager;
use ods_counters::periodic_fetch_counter;
use permission_checker::MononokeIdentitySet;
use permission_checker::MononokeIdentitySetExt;
use rate_limiting_config::ExternalOdsCounter;
use scuba_ext::MononokeScubaSampleBuilder;
use stats::prelude::*;
use thiserror::Error;

#[cfg(fbcode_build)]
mod facebook;
#[cfg(not(fbcode_build))]
mod oss;

#[cfg(fbcode_build)]
pub use facebook::create_rate_limiter;
#[cfg(not(fbcode_build))]
pub use oss::create_rate_limiter;
pub use rate_limiting_config::LoadSheddingMetric;
pub use rate_limiting_config::RateLimitStatus;

pub mod config;

pub type LoadCost = f64;
pub type BoxRateLimiter = Box<dyn RateLimiter + Send + Sync + 'static>;

pub enum RateLimitResult {
    Pass,
    Fail(RateLimitReason),
}

#[async_trait]
pub trait RateLimiter {
    async fn check_rate_limit(
        &self,
        metric: Metric,
        identities: &MononokeIdentitySet,
        main_id: Option<&str>,
        scuba: &mut MononokeScubaSampleBuilder,
        atlas: Option<bool>,
    ) -> Result<RateLimitResult, Error>;

    fn check_load_shed(
        &self,
        identities: &MononokeIdentitySet,
        main_id: Option<&str>,
        scuba: &mut MononokeScubaSampleBuilder,
        atlas: Option<bool>,
    ) -> LoadShedResult;

    fn bump_load(&self, metric: Metric, scope: Scope, load: LoadCost);

    fn category(&self) -> &str;

    fn find_rate_limit(
        &self,
        metric: Metric,
        identities: Option<MononokeIdentitySet>,
        main_id: Option<&str>,
        atlas: Option<bool>,
    ) -> Option<RateLimit>;
}

define_stats! {
    load_shed_counter: dynamic_singleton_counter("{}", (key: String)),
    would_have_rate_limited: dynamic_timeseries("mononoke.rate_limiting.would_have_rate_limited.{}", (metric: String); Rate, Sum),
}

const WOULD_HAVE_RATE_LIMITED_SAMPLE_RATE: NonZeroU64 = NonZeroU64::new(1000).unwrap();

/// Records a limit that would have fired but is not enforced: an exact count in
/// ODS and a sampled Scuba row, so a fleet-wide limit cannot flood the request table.
fn log_would_have_rate_limited(
    scuba: &MononokeScubaSampleBuilder,
    metric: &str,
    reason: &RateLimitReason,
) {
    STATS::would_have_rate_limited.add_value(1, (metric.to_string(),));
    scuba
        .clone()
        .sampled(WOULD_HAVE_RATE_LIMITED_SAMPLE_RATE)
        .log_with_msg("Would have rate limited", format!("{reason:?}"));
}

#[derive(Clone)]
pub struct RateLimitEnvironment {
    fb: FacebookInit,
    category: String,
    config: ConfigHandle<MononokeRateLimitConfig>,
    counter_manager: Arc<RwLock<OdsCounterManager>>,
}

impl RateLimitEnvironment {
    pub fn new(
        fb: FacebookInit,
        category: String,
        config: ConfigHandle<MononokeRateLimitConfig>,
        counter_manager: Arc<RwLock<OdsCounterManager>>,
    ) -> Self {
        mononoke::spawn_task(periodic_fetch_counter(
            counter_manager.clone(),
            desired_ods_counters_provider(config.clone()),
            Duration::from_mins(1),
        ));

        Self {
            fb,
            category,
            config,
            counter_manager,
        }
    }

    pub fn new_with_runtime(
        fb: FacebookInit,
        category: String,
        config: ConfigHandle<MononokeRateLimitConfig>,
        counter_manager: Arc<RwLock<OdsCounterManager>>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        runtime.spawn(periodic_fetch_counter(
            counter_manager.clone(),
            desired_ods_counters_provider(config.clone()),
            Duration::from_mins(1),
        ));

        Self {
            fb,
            category,
            config,
            counter_manager,
        }
    }

    pub fn get_rate_limiter(&self) -> BoxRateLimiter {
        let config = self.config.get();

        create_rate_limiter(
            self.fb,
            self.category.clone(),
            config,
            self.counter_manager.clone(),
        )
    }
}

fn desired_ods_counters_provider(
    config: ConfigHandle<MononokeRateLimitConfig>,
) -> DesiredCountersProvider {
    Box::new(move || {
        config
            .get()
            .load_shed_limits
            .iter()
            .filter_map(|limit| match &limit.raw_config.load_shedding_metric {
                LoadSheddingMetric::external_ods_counter(counter) => Some(OdsCounterKey {
                    entity: counter.entity.clone(),
                    key: counter.key.clone(),
                    reduce: counter.reduce.clone(),
                    transform: counter.transform.clone(),
                }),
                _ => None,
            })
            .collect::<HashSet<OdsCounterKey>>()
    })
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RateLimitBody {
    pub raw_config: rate_limiting_config::RateLimitBody,
}

#[derive(Debug, Clone)]
pub struct MononokeRateLimitConfig {
    pub rate_limits: Vec<RateLimit>,
    pub load_shed_limits: Vec<LoadShedLimit>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateLimit {
    pub body: RateLimitBody,
    pub target: Option<Target>,
    pub fci_metric: FciMetric,
}

#[cfg(fbcode_build)]
impl RateLimit {
    fn applies_to_client(
        &self,
        identities: &MononokeIdentitySet,
        main_id: Option<&str>,
        atlas: Option<bool>,
    ) -> bool {
        match &self.target {
            // TODO (harveyhunt): Pass identities rather than Some(identities) once LFS server has
            // been updated to require certs.
            Some(t) => t.matches_client(Some(identities), main_id, atlas),
            None => true,
        }
    }
}

pub enum LoadShedResult {
    Pass,
    Fail(RateLimitReason),
}

pub fn log_or_enforce_status(
    raw_config: rate_limiting_config::LoadShedLimit,
    metric: String,
    counter_key: &str,
    value: i64,
    scuba: &mut MononokeScubaSampleBuilder,
    no_target: bool,
) -> LoadShedResult {
    match raw_config.status {
        RateLimitStatus::Disabled => LoadShedResult::Pass,
        RateLimitStatus::Tracked => {
            log_would_have_rate_limited(
                scuba,
                counter_key,
                &RateLimitReason::LoadShedMetric(metric, value, raw_config.limit, no_target),
            );
            LoadShedResult::Pass
        }
        RateLimitStatus::Enforced => LoadShedResult::Fail(RateLimitReason::LoadShedMetric(
            metric,
            value,
            raw_config.limit,
            no_target,
        )),
        _ => panic!(
            "Thrift enums aren't real enums once in Rust. We have to account for other values here."
        ),
    }
}

impl LoadShedLimit {
    // TODO(harveyhunt): Make identities none optional once LFS server enforces that.
    pub fn should_load_shed(
        &self,
        fb: FacebookInit,
        identities: Option<&MononokeIdentitySet>,
        main_id: Option<&str>,
        scuba: &mut MononokeScubaSampleBuilder,
        ods_counters: Arc<RwLock<OdsCounterManager>>,
        atlas: Option<bool>,
    ) -> LoadShedResult {
        let applies_to_client = match &self.target {
            Some(t) => t.matches_client(identities, main_id, atlas),
            None => true,
        };

        if !applies_to_client {
            return LoadShedResult::Pass;
        }

        // Fetch the counter
        let (metric_string, counter_key, value, force_targeted) = match self
            .raw_config
            .load_shedding_metric
            .clone()
        {
            LoadSheddingMetric::local_fb303_counter(metric) => {
                let metric = metric.to_string();
                (
                    metric.clone(),
                    metric.clone(),
                    STATS::load_shed_counter.get_value(fb, (metric,)),
                    false,
                )
            }
            LoadSheddingMetric::external_ods_counter(ExternalOdsCounter {
                entity,
                key,
                reduce,
                transform,
            }) => {
                let value = ods_counters
                    .read()
                    .expect("Poisoned lock")
                    .get_counter_value(&entity, &key, reduce.as_deref(), transform.as_deref())
                    .map(|v| v as i64);
                (
                    format!(
                        "Ods key:{key} entity:{entity} reduce:{reduce:?} transform:{transform:?}"
                    ),
                    key,
                    value,
                    false,
                )
            }
            LoadSheddingMetric::top_client(top_client) => {
                let group = ClientGroup::for_request(identities, main_id);
                let is_top = global_client_memory_registry().top_group() == Some(group);
                let value = if is_top {
                    STATS::load_shed_counter.get_value(fb, (top_client.metric.clone(),))
                } else {
                    None
                };
                (
                    format!(
                        "top_client (group: {}, metric: {})",
                        group.name(),
                        top_client.metric
                    ),
                    top_client.metric,
                    value,
                    true,
                )
            }
            _ => ("".to_string(), "".to_string(), None, false),
        };

        match value {
            Some(value) if value > self.raw_config.limit => {
                let no_target = if force_targeted {
                    false
                } else {
                    self.target.is_none()
                };
                log_or_enforce_status(
                    self.raw_config.clone(),
                    metric_string,
                    &counter_key,
                    value,
                    scuba,
                    no_target,
                )
            }
            _ => LoadShedResult::Pass,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadShedLimit {
    pub raw_config: rate_limiting_config::LoadShedLimit,
    target: Option<Target>,
}

#[derive(Debug, Copy, Clone, PartialEq, strum::Display)]
pub enum Metric {
    EgressBytes,
    TotalManifests,
    GetpackFiles,
    Commits,
    CommitsPerUser,
    EdenApiQps,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum Scope {
    Global,
    Regional,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct FciMetric {
    pub metric: Metric,
    pub window: Duration,
    pub scope: Scope,
}

#[must_use]
#[derive(Debug, Error)]
pub enum RateLimitReason {
    /// Rate limited due to an aggregate FCI counter exceeding a threshold.
    /// The bool indicates whether the limit has no target (applies to
    /// everyone). Untargeted limits yield HTTP 503; targeted limits yield
    /// HTTP 429.
    #[error("Rate limited by {0:?} over {1:?}")]
    RateLimitedMetric(Metric, Duration, bool),
    /// Load shed due to an infrastructure metric (ODS/fb303) exceeding a
    /// threshold. The bool indicates whether the limit has no target.
    #[error("Load shed due to {0} (value: {1}, limit: {2})")]
    LoadShedMetric(String, i64, i64, bool),
}

impl RateLimitReason {
    /// Returns true if the rate limit has no target (applies to all clients).
    pub fn no_target(&self) -> bool {
        match self {
            Self::RateLimitedMetric(_, _, no_target) => *no_target,
            Self::LoadShedMetric(_, _, _, no_target) => *no_target,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    StaticSlice(StaticSlice),
    MainClientId(String),
    Identities(MononokeIdentitySet),
    Atlas(AtlasTarget),
}

#[derive(Debug, Copy, Clone, PartialEq)]
struct SlicePct(u8);

impl TryFrom<i32> for SlicePct {
    type Error = Error;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        if !(0..=100).contains(&value) {
            return Err(anyhow!("Invalid percentage"));
        }

        Ok(Self(value.try_into()?))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StaticSlice {
    slice_pct: SlicePct,
    // This is hashed with a client's hostname to allow us to change
    // which percentage of hosts are in a static slice.
    nonce: String,
    target: StaticSliceTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AtlasTarget {
    // Empty struct - applies to all Atlas environments
}

#[derive(Debug, Clone, PartialEq)]
pub enum StaticSliceTarget {
    Identities(MononokeIdentitySet),
    MainClientId(String),
}

impl Target {
    pub fn matches_client(
        &self,
        identities: Option<&MononokeIdentitySet>,
        main_client_id: Option<&str>,
        atlas: Option<bool>,
    ) -> bool {
        match self {
            Self::Identities(target_identities) => {
                if target_identities.is_empty() {
                    true
                } else {
                    match identities {
                        Some(identities) => {
                            // Check that identities is a subset of client_idents
                            target_identities.is_subset(identities)
                        }
                        None => false,
                    }
                }
            }
            Self::MainClientId(id) => match main_client_id {
                Some(client_id) => client_id == id,
                None => false,
            },
            Self::StaticSlice(s) => {
                // Check that identities is a subset of client_idents
                match matches_static_slice_target(&s.target, identities, main_client_id) {
                    true => in_throttled_slice(identities, s.slice_pct, &s.nonce),
                    false => false,
                }
            }
            Self::Atlas(_atlas_target) => {
                // Check if the client is an Atlas client
                atlas == Some(true)
            }
        }
    }
}

fn matches_static_slice_target(
    target: &StaticSliceTarget,
    identities: Option<&MononokeIdentitySet>,
    main_client_id: Option<&str>,
) -> bool {
    match target {
        StaticSliceTarget::Identities(target_identities) => {
            match identities {
                Some(identities) => {
                    // Check that identities is a subset of client_idents
                    target_identities.is_subset(identities)
                }
                None => false,
            }
        }
        StaticSliceTarget::MainClientId(id) => match main_client_id {
            Some(client_id) => client_id == id,
            None => false,
        },
    }
}

fn in_throttled_slice(
    identities: Option<&MononokeIdentitySet>,
    slice_pct: SlicePct,
    nonce: &str,
) -> bool {
    let hostname = if let Some(hostname) = identities.map(|i| i.hostname()) {
        hostname
    } else {
        return false;
    };

    let mut hasher = DefaultHasher::new();
    hostname.hash(&mut hasher);
    nonce.hash(&mut hasher);

    hasher.finish() % 100 < slice_pct.0.into()
}

#[cfg(test)]
mod test {
    #[cfg(fbcode_build)]
    use std::collections::HashMap;
    #[cfg(fbcode_build)]
    use std::sync::Arc;

    use mononoke_macros::mononoke;
    use permission_checker::MononokeIdentity;

    use super::*;

    #[mononoke::test]
    fn test_target_matches() {
        let test_ident = MononokeIdentity::from_legacy_type_data("USER", "foo");
        let test2_ident = MononokeIdentity::from_legacy_type_data("USER", "baz");
        let test_client_id = String::from("test_client_id");
        let empty_idents = Some(MononokeIdentitySet::new());

        let ident_target = Target::Identities([test_ident.clone()].into());

        assert!(!ident_target.matches_client(empty_idents.as_ref(), None, None));

        let mut idents = MononokeIdentitySet::new();
        idents.insert(test_ident.clone());
        idents.insert(test2_ident.clone());
        let idents = Some(idents);

        assert!(ident_target.matches_client(idents.as_ref(), None, None));

        let two_idents = Target::Identities([test_ident, test2_ident].into());

        assert!(two_idents.matches_client(idents.as_ref(), None, None));

        let client_id_target = Target::MainClientId(test_client_id.clone());
        assert!(client_id_target.matches_client(None, Some(&test_client_id), None));

        // Check that all match if the target is empty.
        let empty_ident_target = Target::Identities([].into());
        assert!(empty_ident_target.matches_client(None, None, None));
        assert!(empty_ident_target.matches_client(idents.as_ref(), None, None));
        assert!(empty_ident_target.matches_client(None, Some(&test_client_id), None));
        assert!(empty_ident_target.matches_client(idents.as_ref(), Some(&test_client_id), None));
    }

    #[mononoke::test]
    fn test_target_in_static_slice() {
        let mut identities = MononokeIdentitySet::new();
        identities.insert(MononokeIdentity::from_legacy_type_data(
            "MACHINE",
            "abc123.abc1.facebook.com",
        ));

        assert!(!in_throttled_slice(None, 100.try_into().unwrap(), "abc"));

        assert!(!in_throttled_slice(
            Some(&identities),
            0.try_into().unwrap(),
            "abc"
        ));

        assert!(in_throttled_slice(
            Some(&identities),
            100.try_into().unwrap(),
            "abc"
        ));

        assert!(in_throttled_slice(
            Some(&identities),
            50.try_into().unwrap(),
            "123"
        ));

        // Check that changing the nonce results in a different slice.
        assert!(!in_throttled_slice(
            Some(&identities),
            50.try_into().unwrap(),
            "abc"
        ));
    }

    #[mononoke::test]
    fn test_atlas_target_matches() {
        // Test Atlas target - matches all Atlas
        let atlas_target = Target::Atlas(AtlasTarget {});

        // Should match any Atlas client
        assert!(atlas_target.matches_client(None, None, Some(true)));

        // Should not match non-Atlas clients
        assert!(!atlas_target.matches_client(None, None, Some(false)));
        assert!(!atlas_target.matches_client(None, None, None));
    }

    #[cfg(fbcode_build)]
    #[mononoke::test]
    fn test_static_slice_of_identity_set() {
        let test_ident = MononokeIdentity::from_legacy_type_data("USER", "foo");
        let test2_ident = MononokeIdentity::from_legacy_type_data("SERVICE_IDENTITY", "bar");
        let test3_ident =
            MononokeIdentity::from_legacy_type_data("MACHINE", "abc125.abc.facebook.com");
        let test4_ident =
            MononokeIdentity::from_legacy_type_data("MACHINE", "abc124.abc.facebook.com");

        let ident_target = Target::Identities([test2_ident.clone()].into());
        let twenty_pct_service_identity = Target::StaticSlice(StaticSlice {
            slice_pct: 20.try_into().unwrap(),
            nonce: "nonce".into(),
            target: StaticSliceTarget::Identities([test2_ident.clone()].into()),
        });
        let hundred_pct_service_identity = Target::StaticSlice(StaticSlice {
            slice_pct: 100.try_into().unwrap(),
            nonce: "nonce".into(),
            target: StaticSliceTarget::Identities([test2_ident.clone()].into()),
        });

        let mut idents = MononokeIdentitySet::new();
        idents.insert(test_ident.clone());
        idents.insert(test2_ident.clone());
        idents.insert(test3_ident);
        let idents1 = Some(idents);

        let mut idents = MononokeIdentitySet::new();
        idents.insert(test_ident);
        idents.insert(test2_ident);
        idents.insert(test4_ident);
        let idents2 = Some(idents);

        // All of SERVICE_IDENTITY: bar
        assert!(ident_target.matches_client(idents1.as_ref(), None, None));

        // 20% of SERVICE_IDENTITY: bar. ratelimited host
        assert!(twenty_pct_service_identity.matches_client(idents1.as_ref(), None, None));

        // 20% of SERVICE_IDENTITY: bar. not ratelimited host
        assert!(!twenty_pct_service_identity.matches_client(idents2.as_ref(), None, None));

        // 100% of SERVICE_IDENTITY: bar
        assert!(hundred_pct_service_identity.matches_client(idents1.as_ref(), None, None));

        // 100% of SERVICE_IDENTITY: bar
        assert!(hundred_pct_service_identity.matches_client(idents2.as_ref(), None, None));
    }

    #[cfg(fbcode_build)]
    #[mononoke::fbinit_test]
    fn test_find_rate_limit(fb: FacebookInit) {
        let main_client_id_rate_limit = RateLimit {
            body: RateLimitBody::default(),
            target: Some(Target::MainClientId("client_id".to_string())),
            fci_metric: FciMetric {
                metric: Metric::EgressBytes,
                window: Duration::from_mins(1),
                scope: Scope::Global,
            },
        };

        let identities_rate_limit = RateLimit {
            body: RateLimitBody::default(),
            target: Some(Target::Identities(
                [MononokeIdentity::from_legacy_type_data("TIER", "foo")].into(),
            )),
            fci_metric: FciMetric {
                metric: Metric::EgressBytes,
                window: Duration::from_mins(1),
                scope: Scope::Global,
            },
        };

        let generic_rate_limit = RateLimit {
            body: RateLimitBody::default(),
            target: None,
            fci_metric: FciMetric {
                metric: Metric::EgressBytes,
                window: Duration::from_mins(1),
                scope: Scope::Global,
            },
        };

        let rate_limiter = create_rate_limiter(
            fb,
            "test".to_string(),
            Arc::new(MononokeRateLimitConfig {
                rate_limits: vec![
                    main_client_id_rate_limit.clone(),
                    identities_rate_limit.clone(),
                    generic_rate_limit.clone(),
                ],
                load_shed_limits: vec![],
            }),
            OdsCounterManager::new(fb),
        );

        let mut idents = MononokeIdentitySet::new();
        idents.insert(MononokeIdentity::from_legacy_type_data("USER", "bar"));

        assert!(
            rate_limiter.find_rate_limit(
                Metric::EgressBytes,
                Some(idents.clone()),
                Some("non_matching_id"),
                None,
            ) == Some(generic_rate_limit.clone())
        );
        assert!(
            rate_limiter.find_rate_limit(Metric::EgressBytes, None, None, None)
                == Some(generic_rate_limit)
        );
        assert!(
            rate_limiter.find_rate_limit(
                Metric::EgressBytes,
                Some(idents.clone()),
                Some("client_id"),
                None,
            ) == Some(main_client_id_rate_limit.clone())
        );

        idents.insert(MononokeIdentity::from_legacy_type_data("TIER", "foo"));
        assert!(
            rate_limiter.find_rate_limit(Metric::EgressBytes, Some(idents.clone()), None, None,)
                == Some(identities_rate_limit)
        );
        assert!(
            rate_limiter.find_rate_limit(
                Metric::EgressBytes,
                Some(idents),
                Some("client_id"),
                None,
            ) == Some(main_client_id_rate_limit)
        );
    }

    #[cfg(fbcode_build)]
    fn quicksand_identities() -> MononokeIdentitySet {
        MononokeIdentitySet::from([MononokeIdentity::from_legacy_type_data(
            "SERVICE_IDENTITY",
            "quicksand_builder",
        )])
    }

    #[cfg(fbcode_build)]
    #[mononoke::fbinit_test]
    fn test_top_client_load_shed_is_top_consumer_above_limit(fb: FacebookInit) {
        use client_memory::global_client_memory_registry;
        use rate_limiting_config::TopClient;

        let metric_name = "test_memory_pct";

        // Set up the fb303 counter to simulate 80% memory usage
        STATS::load_shed_counter.set_value(fb, 80, (metric_name.to_string(),));

        // Register quicksand as the top consumer
        let registry = global_client_memory_registry();
        registry.add_weight(ClientGroup::Quicksand, 1_000_000);

        let limit = LoadShedLimit {
            raw_config: rate_limiting_config::LoadShedLimit {
                status: RateLimitStatus::Enforced,
                target: None,
                limit: 75,
                load_shedding_metric: LoadSheddingMetric::top_client(TopClient {
                    metric: metric_name.to_string(),
                }),
                ..Default::default()
            },
            target: None,
        };

        let mut scuba = MononokeScubaSampleBuilder::with_discard();
        let ods_counters = OdsCounterManager::new(fb);

        // Client IS the top consumer and metric (80) > limit (75) → Fail
        let result = limit.should_load_shed(
            fb,
            Some(&quicksand_identities()),
            Some("SERVICE_IDENTITY:quicksand_builder"),
            &mut scuba,
            ods_counters.clone(),
            None,
        );
        match result {
            LoadShedResult::Fail(RateLimitReason::LoadShedMetric(
                _,
                value,
                threshold,
                no_target,
            )) => {
                assert_eq!(value, 80);
                assert_eq!(threshold, 75);
                // force_targeted = true → no_target must be false → 429
                assert!(!no_target);
            }
            _ => panic!("Expected LoadShedResult::Fail for top consumer above limit"),
        }

        // Clean up
        registry.remove_weight(ClientGroup::Quicksand, 1_000_000);
    }

    #[cfg(fbcode_build)]
    #[mononoke::fbinit_test]
    fn test_top_client_load_shed_is_top_consumer_below_limit(fb: FacebookInit) {
        use client_memory::global_client_memory_registry;
        use rate_limiting_config::TopClient;

        let metric_name = "test_memory_pct_below";

        // Set up the fb303 counter to simulate 60% memory usage (below limit)
        STATS::load_shed_counter.set_value(fb, 60, (metric_name.to_string(),));

        let registry = global_client_memory_registry();
        registry.add_weight(ClientGroup::Quicksand, 1_000_000);

        let limit = LoadShedLimit {
            raw_config: rate_limiting_config::LoadShedLimit {
                status: RateLimitStatus::Enforced,
                target: None,
                limit: 75,
                load_shedding_metric: LoadSheddingMetric::top_client(TopClient {
                    metric: metric_name.to_string(),
                }),
                ..Default::default()
            },
            target: None,
        };

        let mut scuba = MononokeScubaSampleBuilder::with_discard();
        let ods_counters = OdsCounterManager::new(fb);

        // Client IS the top consumer but metric (60) < limit (75) → Pass
        let result = limit.should_load_shed(
            fb,
            Some(&quicksand_identities()),
            Some("SERVICE_IDENTITY:quicksand_builder"),
            &mut scuba,
            ods_counters,
            None,
        );
        assert!(matches!(result, LoadShedResult::Pass));

        registry.remove_weight(ClientGroup::Quicksand, 1_000_000);
    }

    #[cfg(fbcode_build)]
    #[mononoke::fbinit_test]
    fn test_top_client_load_shed_not_top_consumer(fb: FacebookInit) {
        use client_memory::global_client_memory_registry;
        use rate_limiting_config::TopClient;

        let metric_name = "test_memory_pct_not_top";

        // Set up the fb303 counter to simulate 80% memory usage
        STATS::load_shed_counter.set_value(fb, 80, (metric_name.to_string(),));

        // Register quicksand as top consumer, but the requesting client is "other"
        let registry = global_client_memory_registry();
        registry.add_weight(ClientGroup::Quicksand, 1_000_000);

        let limit = LoadShedLimit {
            raw_config: rate_limiting_config::LoadShedLimit {
                status: RateLimitStatus::Enforced,
                target: None,
                limit: 75,
                load_shedding_metric: LoadSheddingMetric::top_client(TopClient {
                    metric: metric_name.to_string(),
                }),
                ..Default::default()
            },
            target: None,
        };

        let mut scuba = MononokeScubaSampleBuilder::with_discard();
        let ods_counters = OdsCounterManager::new(fb);

        // Client is NOT the top consumer → Pass (even though metric > limit)
        let result = limit.should_load_shed(
            fb,
            Some(&MononokeIdentitySet::from([
                MononokeIdentity::from_legacy_type_data("SERVICE_IDENTITY", "some_other_client"),
            ])),
            Some("SERVICE_IDENTITY:some_other_client"),
            &mut scuba,
            ods_counters,
            None,
        );
        assert!(matches!(result, LoadShedResult::Pass));

        registry.remove_weight(ClientGroup::Quicksand, 1_000_000);
    }

    #[cfg(fbcode_build)]
    #[mononoke::fbinit_test]
    fn test_top_client_load_shed_no_consumers(fb: FacebookInit) {
        use rate_limiting_config::TopClient;

        let metric_name = "test_memory_pct_no_consumers";

        STATS::load_shed_counter.set_value(fb, 80, (metric_name.to_string(),));

        let limit = LoadShedLimit {
            raw_config: rate_limiting_config::LoadShedLimit {
                status: RateLimitStatus::Enforced,
                target: None,
                limit: 75,
                load_shedding_metric: LoadSheddingMetric::top_client(TopClient {
                    metric: metric_name.to_string(),
                }),
                ..Default::default()
            },
            target: None,
        };

        let mut scuba = MononokeScubaSampleBuilder::with_discard();
        let ods_counters = OdsCounterManager::new(fb);

        // No consumers registered (or all at zero) → Pass
        let result = limit.should_load_shed(
            fb,
            Some(&quicksand_identities()),
            Some("SERVICE_IDENTITY:quicksand_builder"),
            &mut scuba,
            ods_counters,
            None,
        );
        assert!(matches!(result, LoadShedResult::Pass));
    }

    #[cfg(fbcode_build)]
    #[mononoke::fbinit_test]
    fn test_top_client_load_shed_tracked_mode(fb: FacebookInit) {
        use client_memory::global_client_memory_registry;
        use rate_limiting_config::TopClient;

        let metric_name = "test_memory_pct_tracked";

        STATS::load_shed_counter.set_value(fb, 80, (metric_name.to_string(),));

        let registry = global_client_memory_registry();
        registry.add_weight(ClientGroup::Quicksand, 1_000_000);

        let limit = LoadShedLimit {
            raw_config: rate_limiting_config::LoadShedLimit {
                status: RateLimitStatus::Tracked,
                target: None,
                limit: 75,
                load_shedding_metric: LoadSheddingMetric::top_client(TopClient {
                    metric: metric_name.to_string(),
                }),
                ..Default::default()
            },
            target: None,
        };

        let mut scuba = MononokeScubaSampleBuilder::with_discard();
        let ods_counters = OdsCounterManager::new(fb);

        // Shadow mode passes; the sampled log goes to a clone, so the
        // request's builder is untouched.
        let result = limit.should_load_shed(
            fb,
            Some(&quicksand_identities()),
            Some("SERVICE_IDENTITY:quicksand_builder"),
            &mut scuba,
            ods_counters,
            None,
        );
        assert!(matches!(result, LoadShedResult::Pass));
        assert!(scuba.get_sample().get("log_tag").is_none());

        registry.remove_weight(ClientGroup::Quicksand, 1_000_000);
    }

    #[cfg(fbcode_build)]
    fn top_client_limit(fb: FacebookInit, metric_name: &str) -> LoadShedLimit {
        use rate_limiting_config::TopClient;

        STATS::load_shed_counter.set_value(fb, 80, (metric_name.to_string(),));
        LoadShedLimit {
            raw_config: rate_limiting_config::LoadShedLimit {
                status: RateLimitStatus::Enforced,
                target: None,
                limit: 75,
                load_shedding_metric: LoadSheddingMetric::top_client(TopClient {
                    metric: metric_name.to_string(),
                }),
                ..Default::default()
            },
            target: None,
        }
    }

    #[cfg(fbcode_build)]
    fn is_shed(
        fb: FacebookInit,
        limit: &LoadShedLimit,
        identities: &MononokeIdentitySet,
        main_id: &str,
    ) -> bool {
        matches!(
            limit.should_load_shed(
                fb,
                Some(identities),
                Some(main_id),
                &mut MononokeScubaSampleBuilder::with_discard(),
                OdsCounterManager::new(fb),
                None,
            ),
            LoadShedResult::Fail(_)
        )
    }

    #[cfg(fbcode_build)]
    fn identity(id_type: &str, id_data: &str) -> MononokeIdentitySet {
        MononokeIdentitySet::from([MononokeIdentity::from_legacy_type_data(id_type, id_data)])
    }

    /// Adds memory for twenty Sandcastle CI aliases, as in S717136, classified
    /// the same way the memory tracker does. Returns the weights to remove.
    #[cfg(fbcode_build)]
    fn add_ci_fan_out() -> Vec<(ClientGroup, i64)> {
        let sandcastle = identity("SERVICE_IDENTITY", "sandcastle.worker.warmed");
        (0..20)
            .map(|i| {
                let main_id = format!("ALIAS:build-{i}");
                let group = ClientGroup::for_request(Some(&sandcastle), Some(&main_id));
                global_client_memory_registry().add_weight(group, 100_000);
                (group, 100_000)
            })
            .collect()
    }

    #[cfg(fbcode_build)]
    fn remove_weights(weights: Vec<(ClientGroup, i64)>) {
        for (group, weight) in weights {
            global_client_memory_registry().remove_weight(group, weight);
        }
    }

    #[cfg(fbcode_build)]
    #[mononoke::fbinit_test]
    fn test_top_client_identity_groups_shed_ci_fan_out_but_not_users(fb: FacebookInit) {
        use justknobs::test_helpers::JustKnobsInMemory;
        use justknobs::test_helpers::KnobVal;
        use justknobs::test_helpers::with_just_knobs;

        let knobs = JustKnobsInMemory::new(HashMap::from([(
            "scm/mononoke:load_shed_client_groups_from_identities".to_string(),
            KnobVal::Bool(true),
        )]));
        with_just_knobs(knobs, || {
            let weights = add_ci_fan_out();
            let limit = top_client_limit(fb, "test_memory_pct_identity_groups");

            assert!(
                is_shed(
                    fb,
                    &limit,
                    &identity("SERVICE_IDENTITY", "sandcastle.worker.warmed"),
                    "ALIAS:another-pipeline",
                ),
                "a new Sandcastle request should be shed when Sandcastle is the top group"
            );
            assert!(
                !is_shed(fb, &limit, &identity("USER", "alice"), "USER:alice"),
                "users are not shed for a Sandcastle fan-out"
            );
            remove_weights(weights);
        });
    }

    #[cfg(fbcode_build)]
    #[mononoke::fbinit_test]
    fn test_top_client_legacy_groups_shed_users_with_ci_fan_out(fb: FacebookInit) {
        // With the knob at its default, alias-only Sandcastle traffic is
        // classified as `other`, so users in `other` are shed with it.
        let weights = add_ci_fan_out();
        let limit = top_client_limit(fb, "test_memory_pct_legacy_groups");

        assert!(is_shed(
            fb,
            &limit,
            &identity("SERVICE_IDENTITY", "sandcastle.worker.warmed"),
            "ALIAS:another-pipeline",
        ));
        assert!(
            is_shed(fb, &limit, &identity("USER", "alice"), "USER:alice"),
            "the legacy classification puts users and CI aliases in the same group"
        );
        remove_weights(weights);
    }
}
