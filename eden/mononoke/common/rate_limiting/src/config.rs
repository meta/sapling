/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use anyhow::Error;
use anyhow::anyhow;
use permission_checker::MononokeIdentity;
use permission_checker::MononokeIdentitySet;
use serde::Deserialize;
use serde::de::Deserializer;
use serde::de::Error as _;

use crate::AtlasTarget;
use crate::FciMetric;
use crate::LoadShedLimit;
use crate::Metric;
use crate::MononokeRateLimitConfig;
use crate::RateLimit;
use crate::RateLimitBody;
use crate::Scope;
use crate::StaticSlice;
use crate::StaticSliceTarget;
use crate::Target;

impl TryFrom<rate_limiting_config::Target> for Target {
    type Error = Error;

    fn try_from(value: rate_limiting_config::Target) -> Result<Self, Self::Error> {
        match value {
            rate_limiting_config::Target::static_slice(s) => {
                let slice_pct = s.slice_pct.try_into()?;
                Ok(Target::StaticSlice(StaticSlice {
                    slice_pct,
                    nonce: s.nonce,
                    target: s.target.try_into()?,
                }))
            }
            rate_limiting_config::Target::main_client_id(i) => {
                Ok(Target::MainClientId(FromStr::from_str(&i)?))
            }
            rate_limiting_config::Target::identities(i) => Ok(Target::Identities(
                i.into_iter()
                    .map(|s| MononokeIdentity::from_str(&s))
                    .collect::<Result<MononokeIdentitySet, _>>()?,
            )),
            rate_limiting_config::Target::atlas(_) => Ok(Target::Atlas(AtlasTarget {})),
            _ => Err(anyhow!(
                "Invalid target. Are you using deprecated `and`, `or` or `not` targets?"
            )),
        }
    }
}

impl TryFrom<rate_limiting_config::StaticSliceTarget> for StaticSliceTarget {
    type Error = Error;
    fn try_from(value: rate_limiting_config::StaticSliceTarget) -> Result<Self, Self::Error> {
        match value {
            rate_limiting_config::StaticSliceTarget::main_client_id(i) => {
                Ok(StaticSliceTarget::MainClientId(FromStr::from_str(&i)?))
            }
            rate_limiting_config::StaticSliceTarget::identities(i) => {
                Ok(StaticSliceTarget::Identities(
                    i.into_iter()
                        .map(|s| MononokeIdentity::from_str(&s))
                        .collect::<Result<MononokeIdentitySet, _>>()?,
                ))
            }
            _ => Err(anyhow!("Invalid target")),
        }
    }
}

impl TryFrom<rate_limiting_config::RateLimitBody> for RateLimitBody {
    type Error = Error;

    fn try_from(value: rate_limiting_config::RateLimitBody) -> Result<Self, Self::Error> {
        Ok(Self { raw_config: value })
    }
}

impl TryFrom<rate_limiting_config::FciMetricKey> for Metric {
    type Error = Error;

    fn try_from(value: rate_limiting_config::FciMetricKey) -> Result<Self, Self::Error> {
        match value {
            rate_limiting_config::FciMetricKey::EgressBytes => Ok(Metric::EgressBytes),
            rate_limiting_config::FciMetricKey::TotalManifests => Ok(Metric::TotalManifests),
            rate_limiting_config::FciMetricKey::GetpackFiles => Ok(Metric::GetpackFiles),
            rate_limiting_config::FciMetricKey::Commits => Ok(Metric::Commits),
            rate_limiting_config::FciMetricKey::CommitsPerUser => Ok(Metric::CommitsPerUser),
            rate_limiting_config::FciMetricKey::EdenApiQps => Ok(Metric::EdenApiQps),
            _ => Err(anyhow!("Invalid FciMetricKey")),
        }
    }
}

impl TryFrom<rate_limiting_config::FciMetricScope> for Scope {
    type Error = Error;

    fn try_from(value: rate_limiting_config::FciMetricScope) -> Result<Self, Self::Error> {
        match value {
            rate_limiting_config::FciMetricScope::Global => Ok(Scope::Global),
            rate_limiting_config::FciMetricScope::Regional => Ok(Scope::Regional),
            _ => Err(anyhow!("Invalid Scope")),
        }
    }
}

impl TryFrom<rate_limiting_config::FciMetric> for FciMetric {
    type Error = Error;

    fn try_from(value: rate_limiting_config::FciMetric) -> Result<Self, Self::Error> {
        let metric: Metric = value.metric.try_into().context("Invalid metric")?;
        let window: u64 = value.window.try_into().context("Invalid window")?;
        let scope: Scope = value.scope.try_into().context("Invalid scope")?;

        Ok(Self {
            metric,
            window: Duration::from_secs(window),
            scope,
        })
    }
}

impl TryFrom<rate_limiting_config::RateLimit> for RateLimit {
    type Error = Error;

    fn try_from(value: rate_limiting_config::RateLimit) -> Result<Self, Self::Error> {
        let body = value
            .limit
            .clone()
            .try_into()
            .context("Invalid limit body")?;

        let target = value
            .target
            .clone()
            .map(Target::try_from)
            .transpose()
            .context("Invalid target")?;

        let fci_metric = value
            .fci_metric
            .clone()
            .try_into()
            .context("Invalid fci metric")?;

        Ok(Self {
            body,
            target,
            fci_metric,
        })
    }
}

impl TryFrom<rate_limiting_config::LoadShedLimit> for LoadShedLimit {
    type Error = Error;

    fn try_from(value: rate_limiting_config::LoadShedLimit) -> Result<Self, Self::Error> {
        let target = value
            .target
            .clone()
            .map(Target::try_from)
            .transpose()
            .context("Invalid target")?;

        Ok(Self {
            raw_config: value,
            target,
        })
    }
}

impl<'de> Deserialize<'de> for LoadShedLimit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = rate_limiting_config::LoadShedLimit::deserialize(deserializer)?;
        let load_shed_limit = raw
            .try_into()
            .map_err(|e| D::Error::custom(format!("{e:?}")))?;

        Ok(load_shed_limit)
    }
}

impl<'de> Deserialize<'de> for MononokeRateLimitConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw_config = rate_limiting_config::MononokeRateLimits::deserialize(deserializer)?;

        let rate_limits = raw_config
            .rate_limits
            .clone()
            .into_iter()
            .map(|r| r.try_into())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| D::Error::custom(format!("{e:?}")))?;

        let load_shed_limits = raw_config
            .load_shed_limits
            .clone()
            .into_iter()
            .map(|r| r.try_into())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| D::Error::custom(format!("{e:?}")))?;

        Ok(Self {
            rate_limits,
            load_shed_limits,
        })
    }
}
