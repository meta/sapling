/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use cloned::cloned;
use context::CoreContext;
use derived_data_manager::BonsaiDerivable;
use futures::future::FutureExt;
use futures_watchdog::WatchdogExt;
use mononoke_types::ChangesetId;
use phases::ArcPhases;
use repo_derivation_queues::DerivationPriority;
use repo_derived_data::ArcRepoDerivedData;
use tracing::Instrument;

use super::IsWarmFn;
use super::Warmer;
use super::WarmerFn;
use crate::WarmerTag;

pub fn create_derived_data_warmer<Derivable>(
    _ctx: &CoreContext,
    repo_derived_data: ArcRepoDerivedData,
    tags: Vec<WarmerTag>,
) -> Warmer
where
    Derivable: BonsaiDerivable,
{
    let warmer: Box<WarmerFn> = Box::new({
        cloned!(repo_derived_data);
        move |ctx: &CoreContext, cs_id: ChangesetId| {
            cloned!(repo_derived_data);
            async move {
                repo_derived_data
                    .derive::<Derivable>(ctx, cs_id, DerivationPriority::HIGH)
                    .await?;
                Ok(())
            }
            .instrument(tracing::info_span!("warmer", ddt = %Derivable::NAME))
            .boxed()
        }
    });

    let is_warm: Box<IsWarmFn> = Box::new({
        move |ctx: &CoreContext, cs_id: ChangesetId| {
            cloned!(repo_derived_data);
            async move {
                let maybe_derived = repo_derived_data
                    .fetch_derived::<Derivable>(ctx, cs_id)
                    .await?;
                Ok(maybe_derived.is_some())
            }
            .watched()
            .instrument(tracing::info_span!("is warm", ddt = %Derivable::NAME))
            .boxed()
        }
    });

    Warmer {
        warmer,
        is_warm,
        tags,
        name: Derivable::NAME.to_string(),
    }
}

pub fn create_public_phase_warmer(_ctx: &CoreContext, phases: ArcPhases) -> Warmer {
    let warmer: Box<WarmerFn> = Box::new({
        cloned!(phases);
        move |ctx: &CoreContext, cs_id: ChangesetId| {
            cloned!(phases);
            async move {
                let client_correlator =
                    ctx.client_request_info().map(|cri| cri.correlator.as_str());
                tracing::info!(
                    changeset = %cs_id,
                    client_correlator = ?client_correlator,
                    "phases warmer: calling add_reachable_as_public"
                );
                let start = std::time::Instant::now();
                phases.add_reachable_as_public(ctx, vec![cs_id]).await?;
                tracing::info!(
                    changeset = %cs_id,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    client_correlator = ?client_correlator,
                    "phases warmer: add_reachable_as_public completed"
                );
                Ok(())
            }
            .boxed()
        }
    });

    let is_warm: Box<IsWarmFn> = Box::new(move |ctx: &CoreContext, cs_id: ChangesetId| {
        cloned!(phases);
        async move {
            let client_correlator = ctx.client_request_info().map(|cri| cri.correlator.as_str());
            tracing::info!(
                changeset = %cs_id,
                client_correlator = ?client_correlator,
                "phases is_warm: calling get_cached_public"
            );
            let start = std::time::Instant::now();
            let maybe_public = phases.get_cached_public(ctx, vec![cs_id]).await?;
            let elapsed_ms = start.elapsed().as_millis();
            tracing::info!(
                changeset = %cs_id,
                elapsed_ms = elapsed_ms as u64,
                is_public = maybe_public.contains(&cs_id),
                client_correlator = ?client_correlator,
                "phases is_warm: get_cached_public completed"
            );

            Ok(maybe_public.contains(&cs_id))
        }
        .boxed()
    });
    Warmer {
        warmer,
        is_warm,
        name: "public phases".to_string(),
        tags: vec![WarmerTag::Hg, WarmerTag::Git],
    }
}
